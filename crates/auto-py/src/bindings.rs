//! The pyo3 surface: the `auto_py` extension module, its `Runner` class, the
//! two exceptions, and `version()`. All CPython C-API glue lives here so the
//! rest of the crate stays pyo3-free. Compiled for the cdylib and the
//! `check`/`clippy` lib target; cfg'd out of the unit-test binary (see lib.rs).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::PyDict;

use auto_runtime::executor::HostTools;
use auto_trace::model::canonical_json;
use serde_json::Value;

use crate::logic::{
    Decoded, budget_on_pure_message, budgeted, capability_refusal_message, decode_answer,
    tool_table_mismatch,
};

create_exception!(
    auto_py,
    AutoError,
    PyException,
    "Artifact load or tier-1 execution failure — including the refusal of a \
     capability-bearing artifact whose tools= table does not cover its declared \
     capabilities exactly (or was not provided at all)."
);
create_exception!(
    auto_py,
    AutoAbstained,
    PyException,
    "The runtime guard tripped: the input sits beyond the artifact's calibrated \
     witness distance, so tier-1 abstained rather than answer out of \
     distribution. The message carries the guard detail, and the structured \
     fields ride as attributes: `reason` (str | None), `distance` (float | \
     None — None for a wrong-shaped input with no text to measure), and \
     `threshold` (float | None). There is no in-process tier-0 \
     (spec/runtime.md \u{a7}9)."
);

/// One compiled artifact held resident in the host Python process. The wasm
/// module is compiled once here; `.answer` runs a fresh instance per call.
/// `frozen` because the runner is read-only and `Send + Sync` — every method
/// takes `&self`, which is what lets `.answer` release the GIL soundly. The
/// one mutable cell is the atomic tool-call counter (ADR-0032), whose
/// interior mutability carries the same `Send + Sync` story as the callback
/// the dispatch closure already rides behind.
#[pyclass(name = "Runner", module = "auto_py", frozen)]
struct Runner {
    inner: auto_runtime::Runner,
    /// Per-answer executed-tool-call counter (ADR-0032): `Some` iff
    /// `max_tool_calls` was given (which the loader only accepts alongside a
    /// real tool host). Shared with the [`budgeted`] dispatch wrapper and
    /// reset to zero at the top of every `.answer`, before the GIL is
    /// released. `None` = no budget = the pre-ADR-0032 byte-identical path.
    tool_calls: Option<Arc<AtomicU64>>,
}

/// Build the ONE dispatch closure a [`HostTools::Callback`] host carries
/// (ADR-0027): find the named callable, ATTACH to the interpreter for exactly
/// the duration of the Python call (`Python::attach`, pyo3 0.29 — `.answer`
/// detached before the wasm call, so this thread does not hold the GIL when a
/// tool fires; the attach guard restores the detached state on exit), hand it
/// the canonical input JSON as a `str`, and demand a `str` of output JSON
/// back. The bridge is deliberately str -> str: no object translation layer
/// to get subtly wrong on either side.
///
/// Every failure — a raising callable, a non-str return, non-JSON text —
/// becomes `Err(..)`: the artifact sees an `{"err"}` envelope and traps
/// honestly; the host interpreter never crashes.
fn dispatch(
    table: BTreeMap<String, Py<PyAny>>,
) -> impl FnMut(&str, &Value) -> Result<Value, String> + Send + 'static {
    move |name: &str, input: &Value| {
        // HostTools::Callback's allowlist already refused undeclared names
        // before dispatch; this arm is defensive, not a policy point
        let Some(callable) = table.get(name) else {
            return Err(format!("tool {name:?} has no callable in the tools table"));
        };
        Python::attach(|py| {
            let result = callable
                .call1(py, (canonical_json(input),))
                .map_err(|e| format!("tool {name:?} raised: {e}"))?;
            let text: String = result.extract(py).map_err(|_| {
                format!(
                    "tool {name:?} returned a non-str; the bridge is str -> str \
                     (canonical input JSON in, output JSON text out)"
                )
            })?;
            serde_json::from_str(&text)
                .map_err(|e| format!("tool {name:?} returned non-JSON text: {e}"))
        })
    }
}

#[pymethods]
impl Runner {
    /// Load `artifact_path` and compile the module once. Every failure is an
    /// `AutoError`.
    ///
    /// `tools=None` (the default) is the pure-only path, unchanged: a pure
    /// artifact loads; a capability artifact refuses at LOAD, naming the
    /// remedy. `tools={name: callable}` loads a capability artifact IFF the
    /// dict keys cover the declared capabilities EXACTLY — a missing
    /// capability, an extra key, or any tools on a pure artifact refuse,
    /// each naming the offender (ADR-0017's exactly-declared loader rule,
    /// embedded per ADR-0027). Each callable receives the canonical input
    /// JSON as a `str` and must return the output JSON as a `str`; raising
    /// or returning anything else surfaces as the tool's error envelope
    /// (an `AutoError` from `.answer`), never a crash.
    ///
    /// `max_tool_calls=None` (the default) is unlimited — byte-identical to
    /// the pre-budget behavior, no audit lines. `max_tool_calls=N` caps ONE
    /// `.answer` at `N` executed tool-callable invocations (ADR-0032 —
    /// ADR-0028's serve budget, embedded): the `N+1`-th call within one
    /// answer is refused WITHOUT invoking the callable, the artifact traps
    /// on the err envelope, and `.answer` raises `AutoError` carrying
    /// `tool budget exceeded: N per answer (ADR-0032)`. Each executed call
    /// logs one stderr audit line `tool audit: <name> call #<k> (embedded)`.
    /// A budget on a pure artifact (no tool host — `tools=None` or the pure
    /// `tools={}` form) refuses at LOAD: a tool budget needs `tools=`,
    /// there is nothing to bound.
    ///
    /// Reentrancy: tool calls serialize on a host mutex, so a callable must
    /// not call back into the SAME `Runner` — that deadlocks. Callables
    /// answer; they do not recurse. Relatedly, the budget counter is per
    /// Runner (reset per answer), so it is exact only while `.answer` calls
    /// on one `Runner` do not overlap — overlapping answers from multiple
    /// Python threads mix their counts (stated, ADR-0032).
    #[new]
    #[pyo3(signature = (artifact_path, tools=None, max_tool_calls=None))]
    fn new(
        artifact_path: &str,
        tools: Option<Bound<'_, PyDict>>,
        max_tool_calls: Option<u64>,
    ) -> PyResult<Self> {
        let bytes = std::fs::read(artifact_path).map_err(|e| {
            AutoError::new_err(format!("cannot read artifact {artifact_path:?}: {e}"))
        })?;
        // Parse the manifest HERE — before handing bytes to the runner — so
        // refusals carry the honest embedded-host messages rather than the
        // loader's generic text; the loader still re-runs every cross-check.
        let artifact = auto_backend::Artifact::from_bytes(&bytes)
            .map_err(|e| AutoError::new_err(format!("invalid artifact: {e}")))?;
        let manifest = artifact
            .manifest()
            .map_err(|e| AutoError::new_err(format!("invalid manifest: {e}")))?;

        let Some(tools) = tools else {
            // No tools: the pure-only path, byte-identical for pure
            // artifacts. A capability artifact refuses at LOAD, naming the
            // tools= remedy (ADR-0027) — not a surprise at call time. (That
            // refusal outranks a budget complaint: once tools= is supplied,
            // the budget becomes meaningful as-is.)
            if let Some(message) = capability_refusal_message(&manifest.capabilities) {
                return Err(AutoError::new_err(message));
            }
            // Pure artifact: a tool budget has nothing to bound — refuse at
            // LOAD (ADR-0032) rather than silently accept a dead parameter.
            if let Some(budget) = max_tool_calls {
                return Err(AutoError::new_err(budget_on_pure_message(budget)));
            }
            let inner = auto_runtime::Runner::new(&bytes).map_err(AutoError::new_err)?;
            return Ok(Self {
                inner,
                tool_calls: None,
            });
        };

        // tools provided: extract {name: callable} with loud, early failures.
        let mut table: BTreeMap<String, Py<PyAny>> = BTreeMap::new();
        for (key, value) in tools.iter() {
            let name: String = key
                .extract()
                .map_err(|_| AutoError::new_err("tools keys must be str (tool names)"))?;
            if !value.is_callable() {
                return Err(AutoError::new_err(format!(
                    "tools[{name:?}] is not callable"
                )));
            }
            table.insert(name, value.unbind());
        }
        let provided: Vec<String> = table.keys().cloned().collect();
        if let Some(message) = tool_table_mismatch(&manifest.capabilities, &provided) {
            return Err(AutoError::new_err(message));
        }
        if manifest.capabilities.is_empty() {
            // tools={} on a pure artifact: exact coverage of zero
            // capabilities. There is nothing to host, so load pure — the
            // loader (rightly) refuses any host attached to a pure artifact.
            // Pure means no host, so a budget refuses here exactly as on the
            // tools=None pure path (ADR-0032).
            if let Some(budget) = max_tool_calls {
                return Err(AutoError::new_err(budget_on_pure_message(budget)));
            }
            let inner = auto_runtime::Runner::new(&bytes).map_err(AutoError::new_err)?;
            return Ok(Self {
                inner,
                tool_calls: None,
            });
        }
        // A real tool host. With a budget, wrap the dispatch closure in the
        // counting seam (ADR-0032): the counter lives Rust-side with the
        // closure — counting and refusing need no GIL — and `.answer` resets
        // it per answer. Without one, the host is byte-identical to ADR-0027.
        let (host, tool_calls) = match max_tool_calls {
            Some(budget) => {
                let calls = Arc::new(AtomicU64::new(0));
                let host = HostTools::callback(
                    provided,
                    budgeted(budget, Arc::clone(&calls), dispatch(table)),
                );
                (host, Some(calls))
            }
            None => (HostTools::callback(provided, dispatch(table)), None),
        };
        let inner =
            auto_runtime::Runner::new_with_tools(&bytes, Some(host)).map_err(AutoError::new_err)?;
        Ok(Self { inner, tool_calls })
    }

    /// Answer one input. Returns the tier-1 output value as canonical JSON
    /// text; raises `AutoAbstained` on a guard trip (message + `reason` /
    /// `distance` / `threshold` attributes) and `AutoError` on any
    /// parse/execution failure — including a tool-budget breach (ADR-0032),
    /// which the artifact surfaces as an execution trap.
    fn answer(&self, py: Python<'_>, input_json: &str) -> PyResult<String> {
        // Per-ANSWER budget reset (ADR-0032): the executed-tool-call counter
        // starts at zero for every answer, before the GIL is released — an
        // atomic store, no lock, no GIL choreography. Absent a budget this
        // touches nothing (the pre-ADR-0032 byte-identical path).
        if let Some(calls) = &self.tool_calls {
            calls.store(0, Ordering::Relaxed);
        }
        // Own the input before releasing the GIL — nothing borrowed from Python
        // is read GIL-free. `detach` is pyo3 0.29's GIL release (the former
        // `allow_threads`); its `Ungil` bound rejects smuggling Python handles
        // into the closure. `Runner` is `Send + Sync`, so the `&self.inner`
        // borrow is sound across the release. A tool callback re-attaches for
        // exactly the duration of its Python call (see `dispatch`).
        let input = input_json.to_owned();
        let envelope = py.detach(|| self.inner.answer(&input));
        match decode_answer(&envelope) {
            Decoded::Output(output) => Ok(output),
            Decoded::Abstained(abstention) => {
                // message first (unchanged), structured fields as attributes
                // — additive (the ADR-0024 recorded follow-up, closed here)
                let err = AutoAbstained::new_err(abstention.message);
                let value = err.value(py);
                value.setattr("reason", abstention.reason.as_deref())?;
                value.setattr("distance", abstention.distance)?;
                value.setattr("threshold", abstention.threshold)?;
                Err(err)
            }
            Decoded::Error(detail) => Err(AutoError::new_err(detail)),
        }
    }
}

/// The crate version, exposed to Python as `auto_py.version()`.
#[pyfunction]
fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The extension module. Its name (`auto_py`) matches the `[lib] name`, so the
/// generated `PyInit_auto_py` is what CPython dlopens.
#[pymodule]
fn auto_py(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<Runner>()?;
    module.add_function(wrap_pyfunction!(version, module)?)?;
    module.add("__version__", env!("CARGO_PKG_VERSION"))?;
    module.add("AutoError", module.py().get_type::<AutoError>())?;
    module.add("AutoAbstained", module.py().get_type::<AutoAbstained>())?;
    Ok(())
}
