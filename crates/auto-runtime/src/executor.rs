//! Tier-1 wasm execution: load a module, refuse anything v0 capabilities
//! forbid, run it on canonical-JSON bytes under fuel and memory limits.
//!
//! Frozen ABI (spec/artifact.md §ABI): the module has ZERO imports; exports
//! `memory`, `alloc: (i32 len) -> i32 ptr`, and
//! `run: (i32 in_ptr, i32 in_len) -> i64` where the result packs
//! `(out_ptr << 32) | out_len` (unsigned fields, bit-cast to i64). Input and
//! output are canonical-JSON utf-8 bytes. A trap is an execution failure.
//! One `run` call per instance; the executor instantiates fresh per call, so
//! no cross-call state exists.
//!
//! Additive `init` extension (S4, spec/artifact.md §ABI): an artifact that
//! carries a `program.json` entry names a module exporting
//! `init: (i32 in_ptr, i32 in_len)`. The executor writes the program bytes
//! via `alloc` and calls `init` exactly once per instance, before `run`.
//! Program/`init` mismatches in either direction are refused as ABI errors;
//! artifacts without programs execute exactly as before.

use auto_contract::harness::Subject;
use auto_trace::model::canonical_json;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

use wasmtime::{Caller, Config, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder};

/// Fuel budget for one `run` call. Bounds runaway execution during
/// verification: a module that never terminates exhausts fuel and traps —
/// an execution failure ([`ExecError::Trap`]), never a hang.
pub const FUEL_PER_CALL: u64 = 500_000_000;

/// Memory cap for one call: 64 MiB. Growth past this is denied by the store
/// limiter, so a memory-hungry module fails instead of exhausting the host.
pub const MAX_MEMORY_BYTES: usize = 64 * 1024 * 1024;

/// The dispatch closure a [`HostTools::Callback`] host carries: tool name +
/// input value in, output value or error string out — the same contract as
/// one row of a Live table, minus the subprocess.
///
/// Bounds, measured against wasmtime 46.0.1: the host function registered
/// with `Linker::func_wrap` must satisfy `IntoFunc`, whose impls require
/// `Fn(..) + Send + Sync + 'static` (wasmtime-46.0.1 src/runtime/func.rs:1845,
/// 1913 — the store data `T` itself only needs `'static`). The callback rides
/// inside that closure behind `Arc<Mutex<..>>`; `Mutex<T>` is `Sync` exactly
/// when `T: Send`, so `Send` IS required here and `Sync` is not (the mutex
/// supplies it). `FnMut` is allowed (not just `Fn`) because every invocation
/// holds the mutex.
pub type ToolCallback = Box<dyn FnMut(&str, &Value) -> Result<Value, String> + Send>;

/// The tool host behind an artifact's declared capabilities (ADR-0017).
/// Table membership IS the allowlist: callers build the table exactly from
/// the manifest's capability list, and a requested tool outside it is an
/// error envelope (which the interpreter turns into an honest trap). For
/// [`HostTools::Callback`] the `names` set plays the table-key role: the
/// loader cross-checks it and [`call`](Self::call) consults it before
/// dispatch, so the closure never sees an undeclared name.
#[derive(Clone)]
pub enum HostTools {
    /// hermetic replay from recorded observations — the emit gate's host:
    /// (tool name, canonical input json) -> recorded output. A pair the
    /// reference never witnessed errors; replay invents nothing.
    Replay(BTreeMap<(String, String), Value>),
    /// live execution — `auto run`'s host: tool name -> argv; the canonical
    /// input json is appended as the final argument and stdout must be the
    /// output value as JSON (the pluggable contract of tier-0 commands,
    /// spec/runtime.md §3).
    Live(BTreeMap<String, Vec<String>>),
    /// in-process host callbacks — the embedded host (ADR-0027): one
    /// dispatch closure serving every name in `names`. Built with
    /// [`HostTools::callback`]; `auto-py` maps declared capabilities to
    /// Python callables through this variant. Calls serialize on the mutex.
    Callback {
        /// the names this host covers — cross-checked against the manifest
        /// exactly like a Live table's keys, and enforced again per call
        names: BTreeSet<String>,
        /// (tool name, input) -> output; shared because `execute` registers
        /// a fresh `Fn + Send + Sync + 'static` host closure per call
        call: Arc<Mutex<ToolCallback>>,
    },
}

// Manual impl: `Callback` carries a closure, which has no `Debug`. Replay and
// Live render exactly as the former derive did.
impl std::fmt::Debug for HostTools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostTools::Replay(recorded) => f.debug_tuple("Replay").field(recorded).finish(),
            HostTools::Live(table) => f.debug_tuple("Live").field(table).finish(),
            HostTools::Callback { names, .. } => f
                .debug_struct("Callback")
                .field("names", names)
                .finish_non_exhaustive(),
        }
    }
}

impl HostTools {
    /// Build a [`HostTools::Callback`] host: the names it covers plus one
    /// dispatch closure. The closure only ever receives names from `names`
    /// (the seam refuses others before dispatch, mirroring Live's
    /// not-in-table error).
    ///
    /// Reentrancy hazard, stated not hidden: calls serialize on an internal
    /// mutex, so a callback that re-enters the SAME executor/runner (e.g. a
    /// Python tool calling back into the `Runner` it serves) deadlocks or
    /// panics on the second lock. Callbacks must answer, not recurse.
    pub fn callback<I, F>(names: I, call: F) -> Self
    where
        I: IntoIterator<Item = String>,
        F: FnMut(&str, &Value) -> Result<Value, String> + Send + 'static,
    {
        HostTools::Callback {
            names: names.into_iter().collect(),
            call: Arc::new(Mutex::new(Box::new(call))),
        }
    }

    /// Execute one tool request against this host.
    fn call(&self, name: &str, input: &Value) -> Result<Value, String> {
        match self {
            HostTools::Replay(recorded) => {
                let key = (name.to_owned(), canonical_json(input));
                recorded.get(&key).cloned().ok_or_else(|| {
                    format!(
                        "tool {name:?} has no recorded output for this input — the \
                         reference never witnessed it (hermetic replay invents nothing)"
                    )
                })
            }
            HostTools::Live(table) => {
                let argv = table
                    .get(name)
                    .ok_or_else(|| format!("tool {name:?} is not in the provided tool table"))?;
                let (command, args) = argv
                    .split_first()
                    .ok_or_else(|| format!("tool {name:?} has an empty command"))?;
                let output = std::process::Command::new(command)
                    .args(args)
                    .arg(canonical_json(input))
                    .output()
                    .map_err(|e| format!("tool {name:?} failed to spawn: {e}"))?;
                if !output.status.success() {
                    return Err(format!(
                        "tool {name:?} exited with {}: {}",
                        output.status,
                        String::from_utf8_lossy(&output.stderr)
                    ));
                }
                serde_json::from_slice(output.stdout.trim_ascii())
                    .map_err(|e| format!("tool {name:?} stdout is not JSON: {e}"))
            }
            HostTools::Callback { names, call } => {
                // the allowlist check lives HERE, not in the embedder's
                // closure, so Callback keeps Live's semantics: an undeclared
                // name errors without the callback ever seeing it
                if !names.contains(name) {
                    return Err(format!("tool {name:?} is not in the provided tool table"));
                }
                // a panicking callback poisons the mutex after wasmtime
                // resumes the unwind (wasmtime-46.0.1 traphandlers.rs:264,
                // 446); recover the inner state rather than brick a resident
                // host forever — the callback owns its own invariants, and
                // its ERRORS travel as err envelopes, never as panics
                let mut callback = call.lock().unwrap_or_else(PoisonError::into_inner);
                callback(name, input)
            }
        }
    }
}

/// The `auto.tool_call` host function: read the request envelope from guest
/// memory, run the tool, write the response envelope back via the guest's
/// own `alloc`, return the packed region. Host-side infrastructure failures
/// (unreadable memory, missing alloc) trap the guest; TOOL failures return
/// an err envelope so the interpreter traps with the tool's own message.
fn host_tool_call(
    caller: &mut Caller<'_, CallState>,
    tools: &HostTools,
    ptr: u32,
    len: u32,
) -> Result<u64, wasmtime::Error> {
    let memory = caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .ok_or_else(|| wasmtime::Error::msg("guest exports no memory"))?;
    let mut request = vec![0u8; len as usize];
    memory
        .read(&mut *caller, ptr as usize, &mut request)
        .map_err(|e| wasmtime::Error::msg(format!("tool request region unreadable: {e}")))?;
    let envelope: Value = match serde_json::from_slice(&request) {
        Ok(v) => v,
        Err(e) => serde_json::json!({ "err": format!("tool request is not JSON: {e}") }),
    };
    let response = match (
        envelope.get("name").and_then(Value::as_str),
        envelope.get("input"),
    ) {
        (Some(name), Some(input)) => match tools.call(name, input) {
            Ok(output) => serde_json::json!({ "ok": output }),
            Err(error) => serde_json::json!({ "err": error }),
        },
        _ => serde_json::json!({ "err": "tool request must carry name and input" }),
    };
    let bytes = serde_json::to_string(&response)
        .expect("envelope serialization cannot fail")
        .into_bytes();
    let out_len = u32::try_from(bytes.len())
        .map_err(|_| wasmtime::Error::msg("tool response exceeds u32 length"))?;
    let alloc = caller
        .get_export("alloc")
        .and_then(|e| e.into_func())
        .ok_or_else(|| wasmtime::Error::msg("guest exports no alloc"))?
        .typed::<u32, u32>(&mut *caller)
        .map_err(|e| wasmtime::Error::msg(format!("guest alloc has the wrong type: {e}")))?;
    let out_ptr = alloc.call(&mut *caller, out_len)?;
    memory
        .write(&mut *caller, out_ptr as usize, &bytes)
        .map_err(|e| wasmtime::Error::msg(format!("tool response region unwritable: {e}")))?;
    Ok((u64::from(out_ptr) << 32) | u64::from(out_len))
}

/// Load or execution failure. Every variant is an observed condition, not a
/// policy placeholder.
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    /// a capability artifact was loaded without a tool host — the caller
    /// must provide (or refuse to provide) the declared tools explicitly
    #[error(
        "artifact declares capabilities {capabilities:?} but no tool host was provided \
         (supply --tool name=command per capability, or refuse)"
    )]
    ToolsRequired { capabilities: Vec<String> },
    /// a module import outside the capability ABI (only auto.tool_call may
    /// ever be imported, and only by capability artifacts; ADR-0017)
    #[error("module imports {found:?}; the capability ABI allows exactly auto.tool_call")]
    UnexpectedImport { found: String },
    /// manifest capabilities and module imports/tools disagree
    #[error("capability mismatch: {0}")]
    CapabilityMismatch(String),
    /// The bytes are not a compilable wasm (or wat) module.
    #[error("module failed to load: {0}")]
    Load(String),
    /// v0 capability rule: only pure artifacts exist — declared capabilities
    /// are empty, so a module with ANY import is refused at load.
    #[error(
        "module declares {count} import(s), first `{first}`; \
         v0 artifacts are pure (zero imports)"
    )]
    ImportsForbidden { count: usize, first: String },
    /// A required export (`memory`, `alloc`, `run`) is absent.
    #[error("module does not export `{0}`")]
    MissingExport(&'static str),
    /// Wrong export signature, out-of-bounds ptr/len, or a program/`init`
    /// mismatch in either direction.
    #[error("module violates the ABI: {0}")]
    BadAbi(String),
    /// A wasm trap — panic/unreachable, memory fault, fuel exhaustion.
    #[error("execution trapped: {0}")]
    Trap(String),
    /// The returned bytes are not utf-8.
    #[error("module output is not utf-8")]
    OutputNotUtf8,
    /// The returned utf-8 is not JSON.
    #[error("module output is not JSON: {0}")]
    OutputNotJson(String),
    /// Container/manifest failure while loading from a `.cbin` artifact.
    #[error("artifact: {0}")]
    Artifact(String),
}

/// Per-call store state: only the resource limits. v0 modules get no host
/// state — there is nothing to persist or leak between calls.
struct CallState {
    limits: StoreLimits,
}

/// One loaded module, executable on JSON values under the frozen ABI.
///
/// The compiled [`Module`] is reused across calls; every [`execute`] call
/// runs in a fresh [`Store`] and instance with its own fuel and memory caps.
/// A stored program (the `init` extension) is fed to every instance before
/// `run`.
///
/// [`execute`]: WasmExecutor::execute
pub struct WasmExecutor {
    engine: Engine,
    module: Module,
    /// `program.json` bytes for the `init` extension; `None` for plain S3
    /// modules.
    program: Option<Vec<u8>>,
    /// the tool host behind `auto.tool_call`; `None` = the pure rule (zero
    /// imports) was enforced at load
    tools: Option<HostTools>,
}

impl WasmExecutor {
    /// Compile a module (wasm binary or wat text) with no program.
    /// Equivalent to [`from_parts`]`(bytes, None)`.
    ///
    /// [`from_parts`]: WasmExecutor::from_parts
    pub fn from_module_bytes(bytes: &[u8]) -> Result<Self, ExecError> {
        Self::from_parts(bytes, None)
    }

    /// Compile a module (wasm binary or wat text), enforce the v0 capability
    /// rule before anything else (zero imports, or refusal), and store the
    /// optional program the module's `init` export will receive each call.
    pub fn from_parts(module_bytes: &[u8], program: Option<Vec<u8>>) -> Result<Self, ExecError> {
        Self::from_parts_with_tools(module_bytes, program, None)
    }

    /// [`from_parts`] with a tool host (ADR-0017). Import rules, enforced
    /// at load: with no host, ZERO imports (the pure rule, unchanged); with
    /// a host, the module may import EXACTLY `auto.tool_call` — anything
    /// else is refused.
    ///
    /// [`from_parts`]: WasmExecutor::from_parts
    pub fn from_parts_with_tools(
        module_bytes: &[u8],
        program: Option<Vec<u8>>,
        tools: Option<HostTools>,
    ) -> Result<Self, ExecError> {
        let mut config = Config::new();
        config.consume_fuel(true);
        let engine = Engine::new(&config).map_err(|e| ExecError::Load(format!("{e:#}")))?;
        let module =
            Module::new(&engine, module_bytes).map_err(|e| ExecError::Load(format!("{e:#}")))?;
        let imports: Vec<String> = module
            .imports()
            .map(|i| format!("{}.{}", i.module(), i.name()))
            .collect();
        match &tools {
            None => {
                if !imports.is_empty() {
                    return Err(ExecError::ImportsForbidden {
                        count: imports.len(),
                        first: imports[0].clone(),
                    });
                }
            }
            Some(_) => {
                if let Some(alien) = imports.iter().find(|i| i.as_str() != "auto.tool_call") {
                    return Err(ExecError::UnexpectedImport {
                        found: alien.clone(),
                    });
                }
            }
        }
        Ok(Self {
            engine,
            module,
            program,
            tools,
        })
    }

    /// Load the `module.wasm` entry of a `.cbin` artifact, plus the optional
    /// `program.json` entry (synthesized artifacts, spec/synthesis.md).
    pub fn from_artifact(artifact: &auto_backend::Artifact) -> Result<Self, ExecError> {
        Self::from_artifact_with_tools(artifact, None)
    }

    /// [`from_artifact`] with a tool host, cross-checked against the
    /// manifest (ADR-0017): empty declared capabilities require a pure
    /// module and NO host; nonempty capabilities require a host whose table
    /// covers every declared name, and the module may import only
    /// `auto.tool_call`.
    ///
    /// [`from_artifact`]: WasmExecutor::from_artifact
    pub fn from_artifact_with_tools(
        artifact: &auto_backend::Artifact,
        tools: Option<HostTools>,
    ) -> Result<Self, ExecError> {
        // a missing manifest entry (minimal fixtures) is the pure path; a
        // PRESENT but unparseable manifest is a loud artifact error
        let capabilities: Vec<String> = if artifact
            .entries
            .contains_key(auto_backend::container::MANIFEST_ENTRY)
        {
            artifact
                .manifest()
                .map_err(|e| ExecError::Artifact(e.to_string()))?
                .capabilities
        } else {
            Vec::new()
        };
        let bytes = artifact
            .module_bytes()
            .map_err(|e| ExecError::Artifact(e.to_string()))?;
        let program = artifact
            .entries
            .get(auto_backend::container::PROGRAM_ENTRY)
            .cloned();
        if capabilities.is_empty() {
            if tools.is_some() {
                return Err(ExecError::CapabilityMismatch(
                    "artifact declares no capabilities; a tool host must not be attached"
                        .to_owned(),
                ));
            }
            return Self::from_parts(bytes, program);
        }
        let Some(tools) = tools else {
            return Err(ExecError::ToolsRequired { capabilities });
        };
        let provided: Vec<&String> = match &tools {
            HostTools::Replay(map) => {
                let mut names: Vec<&String> = map.keys().map(|(n, _)| n).collect();
                names.sort();
                names.dedup();
                names
            }
            HostTools::Live(table) => table.keys().collect(),
            HostTools::Callback { names, .. } => names.iter().collect(),
        };
        for capability in &capabilities {
            if !provided.contains(&capability) {
                return Err(ExecError::CapabilityMismatch(format!(
                    "declared capability {capability:?} has no provided tool"
                )));
            }
        }
        Self::from_parts_with_tools(bytes, program, Some(tools))
    }

    /// Run the module once on `input` and return its output value.
    ///
    /// Fresh store and instance per call (frozen ABI: one `run` per
    /// instance), fuel set to [`FUEL_PER_CALL`], memory capped at
    /// [`MAX_MEMORY_BYTES`]. When a program is stored, it is written via
    /// `alloc` and handed to the module's `init` export exactly once, before
    /// `run`; a program without `init` or an `init` without a program is an
    /// ABI refusal. Input is written as canonical-JSON utf-8 into a region
    /// the module's `alloc` returns; the output region `run` returns is
    /// bounds-checked, read, and parsed.
    pub fn execute(&self, input: &Value) -> Result<Value, ExecError> {
        let limits = StoreLimitsBuilder::new()
            .memory_size(MAX_MEMORY_BYTES)
            .build();
        let mut store = Store::new(&self.engine, CallState { limits });
        store.limiter(|state| &mut state.limits);
        store
            .set_fuel(FUEL_PER_CALL)
            .expect("consume_fuel(true) is set on the engine");

        // imports were validated at load; instantiation can still trap
        // (start function) or exceed the memory limiter
        let mut linker: Linker<CallState> = Linker::new(&self.engine);
        if let Some(tools) = &self.tools {
            let tools = tools.clone();
            linker
                .func_wrap(
                    "auto",
                    "tool_call",
                    move |mut caller: Caller<'_, CallState>, ptr: u32, len: u32| {
                        host_tool_call(&mut caller, &tools, ptr, len)
                    },
                )
                .map_err(|e| ExecError::Load(format!("{e:#}")))?;
        }
        let instance = linker
            .instantiate(&mut store, &self.module)
            .map_err(|e| ExecError::Trap(format!("{e:#}")))?;

        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or(ExecError::MissingExport("memory"))?;
        let alloc = instance
            .get_func(&mut store, "alloc")
            .ok_or(ExecError::MissingExport("alloc"))?
            .typed::<u32, u32>(&store)
            .map_err(|e| ExecError::BadAbi(format!("alloc: {e:#}")))?;
        let run = instance
            .get_func(&mut store, "run")
            .ok_or(ExecError::MissingExport("run"))?
            .typed::<(u32, u32), u64>(&store)
            .map_err(|e| ExecError::BadAbi(format!("run: {e:#}")))?;

        // init extension: exactly one of four cases (spec/artifact.md §ABI)
        match (&self.program, instance.get_func(&mut store, "init")) {
            (Some(program), Some(init)) => {
                let init = init
                    .typed::<(u32, u32), ()>(&store)
                    .map_err(|e| ExecError::BadAbi(format!("init: {e:#}")))?;
                let prog_len = u32::try_from(program.len())
                    .map_err(|_| ExecError::BadAbi("program exceeds u32 length".into()))?;
                let prog_ptr = alloc
                    .call(&mut store, prog_len)
                    .map_err(|e| ExecError::Trap(format!("{e:#}")))?;
                let mem_size = memory.data_size(&store) as u64;
                if u64::from(prog_ptr) + u64::from(prog_len) > mem_size {
                    return Err(ExecError::BadAbi(format!(
                        "alloc({prog_len}) returned ptr {prog_ptr}, past memory size {mem_size}"
                    )));
                }
                memory
                    .write(&mut store, prog_ptr as usize, program)
                    .map_err(|e| ExecError::BadAbi(format!("program write at {prog_ptr}: {e}")))?;
                init.call(&mut store, (prog_ptr, prog_len))
                    .map_err(|e| ExecError::Trap(format!("{e:#}")))?;
            }
            (Some(_), None) => {
                return Err(ExecError::BadAbi(
                    "artifact carries a program but the module exports no init".into(),
                ));
            }
            (None, Some(_)) => {
                return Err(ExecError::BadAbi(
                    "module expects a program (exports init) but the artifact carries none".into(),
                ));
            }
            (None, None) => {} // plain S3 path, unchanged
        }

        let input_bytes = canonical_json(input).into_bytes();
        let in_len = u32::try_from(input_bytes.len())
            .map_err(|_| ExecError::BadAbi("input exceeds u32 length".into()))?;

        let in_ptr = alloc
            .call(&mut store, in_len)
            .map_err(|e| ExecError::Trap(format!("{e:#}")))?;
        let mem_size = memory.data_size(&store) as u64;
        if u64::from(in_ptr) + u64::from(in_len) > mem_size {
            return Err(ExecError::BadAbi(format!(
                "alloc({in_len}) returned ptr {in_ptr}, past memory size {mem_size}"
            )));
        }
        memory
            .write(&mut store, in_ptr as usize, &input_bytes)
            .map_err(|e| ExecError::BadAbi(format!("input write at {in_ptr}: {e}")))?;

        let packed = run
            .call(&mut store, (in_ptr, in_len))
            .map_err(|e| ExecError::Trap(format!("{e:#}")))?;
        // frozen ABI: ((out_ptr as u64) << 32) | (out_len as u64)
        let out_ptr = u32::try_from(packed >> 32).expect("high 32 bits fit u32");
        let out_len = u32::try_from(packed & 0xffff_ffff).expect("low 32 bits fit u32");

        // re-read the size: run may have grown memory
        let mem_size = memory.data_size(&store) as u64;
        if u64::from(out_ptr) + u64::from(out_len) > mem_size {
            return Err(ExecError::BadAbi(format!(
                "run returned region ptr={out_ptr} len={out_len}, past memory size {mem_size}"
            )));
        }
        let mut out = vec![0u8; out_len as usize];
        memory
            .read(&store, out_ptr as usize, &mut out)
            .map_err(|e| ExecError::BadAbi(format!("output read at {out_ptr}: {e}")))?;
        let text = std::str::from_utf8(&out).map_err(|_| ExecError::OutputNotUtf8)?;
        serde_json::from_str(text).map_err(|e| ExecError::OutputNotJson(e.to_string()))
    }
}

/// A compiled module as a verification [`Subject`]: the harness drives
/// tier-1 code with exactly the checks it runs against recorded traces.
pub struct WasmSubject {
    executor: WasmExecutor,
    name: String,
}

impl WasmSubject {
    pub fn new(executor: WasmExecutor, name: impl Into<String>) -> Self {
        Self {
            executor,
            name: name.into(),
        }
    }
}

impl Subject for WasmSubject {
    fn describe(&self) -> String {
        format!("wasm:{}", self.name)
    }

    fn run(&mut self, input: &Value) -> Result<Value, String> {
        self.executor.execute(input).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;

    /// Bump-allocates from 4096 and echoes the input region back.
    const ECHO: &str = r#"(module
        (memory (export "memory") 2)
        (global $next (mut i32) (i32.const 4096))
        (func (export "alloc") (param i32) (result i32)
            global.get $next
            global.get $next local.get 0 i32.add global.set $next)
        (func (export "run") (param i32 i32) (result i64)
            local.get 0 i64.extend_i32_u i64.const 32 i64.shl
            local.get 1 i64.extend_i32_u i64.or))"#;

    fn echo_executor() -> WasmExecutor {
        WasmExecutor::from_module_bytes(ECHO.as_bytes()).expect("echo module loads")
    }

    fn load(wat: &str) -> WasmExecutor {
        WasmExecutor::from_module_bytes(wat.as_bytes()).expect("module loads")
    }

    #[test]
    fn echo_round_trips_an_object() {
        let exec = echo_executor();
        assert_eq!(exec.execute(&json!({"a": 1})).unwrap(), json!({"a": 1}));
    }

    #[test]
    fn echo_round_trips_a_string() {
        let exec = echo_executor();
        assert_eq!(exec.execute(&json!("text")).unwrap(), json!("text"));
    }

    #[test]
    fn any_import_is_refused() {
        let wat = r#"(module (import "env" "f" (func)))"#;
        match WasmExecutor::from_module_bytes(wat.as_bytes()) {
            Err(ExecError::ImportsForbidden { count, first }) => {
                assert_eq!(count, 1);
                assert_eq!(first, "env.f");
            }
            other => panic!("expected ImportsForbidden, got {:?}", other.err()),
        }
    }

    #[test]
    fn unloadable_bytes_are_a_load_error() {
        assert!(matches!(
            WasmExecutor::from_module_bytes(b"(module (this is not wat"),
            Err(ExecError::Load(_))
        ));
    }

    #[test]
    fn missing_run_export() {
        let exec = load(
            r#"(module
                (memory (export "memory") 1)
                (func (export "alloc") (param i32) (result i32) i32.const 0))"#,
        );
        assert!(matches!(
            exec.execute(&json!(null)),
            Err(ExecError::MissingExport("run"))
        ));
    }

    #[test]
    fn missing_memory_export() {
        let exec = load(
            r#"(module
                (func (export "alloc") (param i32) (result i32) i32.const 0)
                (func (export "run") (param i32 i32) (result i64) i64.const 0))"#,
        );
        assert!(matches!(
            exec.execute(&json!(null)),
            Err(ExecError::MissingExport("memory"))
        ));
    }

    #[test]
    fn wrong_run_signature_is_bad_abi() {
        let exec = load(
            r#"(module
                (memory (export "memory") 1)
                (func (export "alloc") (param i32) (result i32) i32.const 0)
                (func (export "run") (param i32) (result i32) local.get 0))"#,
        );
        assert!(matches!(
            exec.execute(&json!(null)),
            Err(ExecError::BadAbi(_))
        ));
    }

    #[test]
    fn unreachable_is_a_trap() {
        let exec = load(
            r#"(module
                (memory (export "memory") 1)
                (func (export "alloc") (param i32) (result i32) i32.const 0)
                (func (export "run") (param i32 i32) (result i64) unreachable))"#,
        );
        match exec.execute(&json!(null)) {
            Err(ExecError::Trap(msg)) => assert!(msg.contains("unreachable"), "{msg}"),
            other => panic!("expected Trap, got {other:?}"),
        }
    }

    #[test]
    fn fuel_bounds_runaway_execution() {
        let exec = load(
            r#"(module
                (memory (export "memory") 1)
                (func (export "alloc") (param i32) (result i32) i32.const 0)
                (func (export "run") (param i32 i32) (result i64)
                    loop $spin br $spin end
                    unreachable))"#,
        );
        match exec.execute(&json!(null)) {
            Err(ExecError::Trap(msg)) => assert!(msg.contains("fuel"), "{msg}"),
            other => panic!("expected Trap on fuel exhaustion, got {other:?}"),
        }
    }

    #[test]
    fn zeroed_output_bytes_are_not_json() {
        // run returns ptr=0 len=4: four NUL bytes are utf-8 but not JSON
        let exec = load(
            r#"(module
                (memory (export "memory") 1)
                (func (export "alloc") (param i32) (result i32) i32.const 4096)
                (func (export "run") (param i32 i32) (result i64) i64.const 4))"#,
        );
        assert!(matches!(
            exec.execute(&json!(null)),
            Err(ExecError::OutputNotJson(_))
        ));
    }

    #[test]
    fn non_utf8_output_bytes() {
        let exec = load(
            r#"(module
                (memory (export "memory") 1)
                (data (i32.const 0) "\ff\fe\fd")
                (func (export "alloc") (param i32) (result i32) i32.const 4096)
                (func (export "run") (param i32 i32) (result i64) i64.const 3))"#,
        );
        assert!(matches!(
            exec.execute(&json!(null)),
            Err(ExecError::OutputNotUtf8)
        ));
    }

    #[test]
    fn out_of_bounds_output_region_is_bad_abi() {
        // ptr = 0x7fffffff, len = 0x10 — far past the 64 KiB memory
        let exec = load(
            r#"(module
                (memory (export "memory") 1)
                (func (export "alloc") (param i32) (result i32) i32.const 4096)
                (func (export "run") (param i32 i32) (result i64)
                    i64.const 0x7fffffff00000010))"#,
        );
        assert!(matches!(
            exec.execute(&json!(null)),
            Err(ExecError::BadAbi(_))
        ));
    }

    #[test]
    fn out_of_bounds_alloc_is_bad_abi() {
        let exec = load(
            r#"(module
                (memory (export "memory") 1)
                (func (export "alloc") (param i32) (result i32) i32.const 0x7fffffff)
                (func (export "run") (param i32 i32) (result i64) i64.const 0))"#,
        );
        assert!(matches!(
            exec.execute(&json!(null)),
            Err(ExecError::BadAbi(_))
        ));
    }

    #[test]
    fn from_artifact_loads_the_module_entry() {
        let mut entries = BTreeMap::new();
        entries.insert(
            auto_backend::MODULE_ENTRY.to_owned(),
            ECHO.as_bytes().to_vec(),
        );
        let artifact = auto_backend::Artifact::new(entries);
        let exec = WasmExecutor::from_artifact(&artifact).unwrap();
        assert_eq!(exec.execute(&json!([1, 2])).unwrap(), json!([1, 2]));
    }

    #[test]
    fn from_artifact_without_module_is_an_artifact_error() {
        let artifact = auto_backend::Artifact::new(BTreeMap::new());
        assert!(matches!(
            WasmExecutor::from_artifact(&artifact),
            Err(ExecError::Artifact(_))
        ));
    }

    #[test]
    fn subject_answers_and_describes() {
        let mut subject = WasmSubject::new(echo_executor(), "echo");
        assert_eq!(subject.describe(), "wasm:echo");
        assert_eq!(
            subject.run(&json!({"k": [true]})).unwrap(),
            json!({"k": [true]})
        );
    }

    #[test]
    fn subject_maps_errors_to_strings() {
        let exec = load(
            r#"(module
                (memory (export "memory") 1)
                (func (export "alloc") (param i32) (result i32) i32.const 0)
                (func (export "run") (param i32 i32) (result i64) unreachable))"#,
        );
        let mut subject = WasmSubject::new(exec, "trapper");
        let err = subject.run(&json!(null)).unwrap_err();
        assert!(err.contains("trap"), "{err}");
    }

    // ---- init extension ----

    /// Stores the program region in globals at `init`; `run` returns exactly
    /// that region. Output == program bytes proves the host allocated, wrote,
    /// and called `init` before `run`, end to end.
    const INIT_ECHO: &str = r#"(module
        (memory (export "memory") 2)
        (global $next (mut i32) (i32.const 4096))
        (global $prog_ptr (mut i32) (i32.const 0))
        (global $prog_len (mut i32) (i32.const 0))
        (func (export "alloc") (param i32) (result i32)
            global.get $next
            global.get $next local.get 0 i32.add global.set $next)
        (func (export "init") (param i32 i32)
            local.get 0 global.set $prog_ptr
            local.get 1 global.set $prog_len)
        (func (export "run") (param i32 i32) (result i64)
            global.get $prog_ptr i64.extend_i32_u i64.const 32 i64.shl
            global.get $prog_len i64.extend_i32_u i64.or))"#;

    /// Valid JSON so the output parse of the echoed region succeeds.
    const PROGRAM: &[u8] = br#"{"marker":42}"#;

    #[test]
    fn init_receives_the_program_before_run() {
        let exec = WasmExecutor::from_parts(INIT_ECHO.as_bytes(), Some(PROGRAM.to_vec()))
            .expect("init-echo module loads");
        assert_eq!(
            exec.execute(&json!("ignored")).unwrap(),
            json!({"marker": 42})
        );
    }

    #[test]
    fn from_artifact_reads_the_program_entry() {
        let mut entries = BTreeMap::new();
        entries.insert(
            auto_backend::MODULE_ENTRY.to_owned(),
            INIT_ECHO.as_bytes().to_vec(),
        );
        entries.insert(
            auto_backend::container::PROGRAM_ENTRY.to_owned(),
            PROGRAM.to_vec(),
        );
        let artifact = auto_backend::Artifact::new(entries);
        let exec = WasmExecutor::from_artifact(&artifact).unwrap();
        assert_eq!(exec.execute(&json!(null)).unwrap(), json!({"marker": 42}));
    }

    #[test]
    fn program_without_init_export_is_refused() {
        let exec = WasmExecutor::from_parts(ECHO.as_bytes(), Some(PROGRAM.to_vec())).unwrap();
        match exec.execute(&json!(null)) {
            Err(ExecError::BadAbi(msg)) => assert!(msg.contains("exports no init"), "{msg}"),
            other => panic!("expected BadAbi, got {other:?}"),
        }
    }

    #[test]
    fn init_export_without_program_is_refused() {
        // from_module_bytes == from_parts(bytes, None)
        let exec = WasmExecutor::from_module_bytes(INIT_ECHO.as_bytes()).unwrap();
        match exec.execute(&json!(null)) {
            Err(ExecError::BadAbi(msg)) => assert!(msg.contains("carries none"), "{msg}"),
            other => panic!("expected BadAbi, got {other:?}"),
        }
    }

    #[test]
    fn init_trap_propagates() {
        let exec = WasmExecutor::from_parts(
            br#"(module
                (memory (export "memory") 1)
                (func (export "alloc") (param i32) (result i32) i32.const 4096)
                (func (export "init") (param i32 i32) unreachable)
                (func (export "run") (param i32 i32) (result i64) i64.const 0))"#,
            Some(PROGRAM.to_vec()),
        )
        .unwrap();
        match exec.execute(&json!(null)) {
            Err(ExecError::Trap(msg)) => assert!(msg.contains("unreachable"), "{msg}"),
            other => panic!("expected Trap, got {other:?}"),
        }
    }
}
