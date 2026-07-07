# ADR-0027: embedded tool host — capability artifacts inside the Python process

status: accepted · scope: `auto-runtime`
(`HostTools::Callback`, `ToolCallback`), `auto-py` (`Runner(…, tools=)`,
structured `AutoAbstained`), `spec/adr/0027-embedded-tool-host.md`

## context

ADR-0024 embedded the compiled artifact in the agent's own Python process but
PURE only: a capability artifact refused at load, with "per-request tool
policy + host callbacks" recorded as the follow-up. ADR-0017 (amended wave 7)
already made every other surface servable through one loader —
`WasmExecutor::from_artifact_with_tools` cross-checks manifest capabilities
against the host's table, refuses undeclared imports, refuses a host on a
pure artifact. What was missing was a host VARIANT whose tools are in-process
callables rather than recorded pairs (`Replay`) or subprocess argv (`Live`).
This ADR closes the host-callbacks half of the recorded follow-up. The
accounting/policy half (who may invoke which tool, charged to whom) is the
same per-request gap as ADR-0011 decision 2 and remains recorded, not built.

## decision

1. **`HostTools::Callback { names, call }`.** One dispatch closure serving a
   declared name set. `names: BTreeSet<String>` exists so the LOADER's
   capability cross-check runs unchanged against it — exactly as it checks a
   Live table's keys; one enforcement point, no Callback-shaped bypass — and
   so the seam itself refuses an undeclared name BEFORE dispatch (Live's
   not-in-table error), rather than trusting each embedder's closure.

2. **Bounds, measured not guessed.** `call` is
   `Arc<Mutex<Box<dyn FnMut(&str, &Value) -> Result<Value, String> + Send>>>`
   (`ToolCallback` aliases the box). wasmtime 46.0.1's `Linker::func_wrap`
   takes `impl IntoFunc`, whose impls demand the registered closure be
   `Fn + Send + Sync + 'static` (wasmtime-46.0.1 src/runtime/func.rs:1845,
   1913; the store data `T` itself only needs `'static`). `execute` registers
   a fresh host closure per call from `&self`, so the callback must be shared
   (`Arc`) and interior-mutable (`Mutex` — also what lets an `FnMut` be
   called from a `Fn`). `Mutex<T>` is `Sync` exactly when `T: Send`, so
   `Send` on the boxed callback IS required and `Sync` is not (the mutex
   supplies it). Same envelope semantics as Live: name + canonical input in,
   `{"ok"}|{"err"}` out; tool failures are err envelopes the interpreter
   turns into honest traps. A panicking callback propagates (wasmtime catches
   at the boundary and resumes the unwind after the wasm stack is gone,
   traphandlers.rs:264,446); the seam recovers the poisoned mutex
   (`PoisonError::into_inner`) so one panic does not brick a resident host.

3. **`Runner(artifact_path, tools=None)` — the exactly-declared rule.**
   `tools=None` is the pure-only path unchanged; the capability refusal now
   names the remedy (`tools=`) instead of ADR-0024's "not supported in v0"
   (that message claimed host callbacks were a recorded follow-up, which
   this ADR makes false — keeping it would lie). `tools={name: callable}`
   loads a capability artifact IFF the dict keys cover the declared
   capabilities EXACTLY: missing capabilities refuse naming them, extra keys
   refuse naming them, any tools on a pure artifact refuse (ADR-0017's
   loader rule; `tools={}` on a pure artifact is exact coverage of zero and
   loads pure). Deliberately STRICTER than serve's server-wide table, where
   extras are legitimate (many artifacts share one table): an embedded table
   serves exactly one artifact, so an extra key is dead weight or a typo —
   refuse loud at load. The loader still re-runs all its own cross-checks.

4. **str -> str bridge.** The callable receives the canonical input JSON as a
   `str` and must return the output JSON as a `str`. No object translation
   layer to get subtly wrong on either side; the canonical-JSON text IS the
   wire format everywhere else in the system. A raising callable becomes
   `Err(<exception text>)`; a non-str or non-JSON return becomes `Err`,
   loud — the artifact sees the `{"err"}` envelope and traps honestly; the
   host interpreter never crashes.

5. **GIL choreography.** `.answer` releases the GIL around the wasm call
   (`Python::detach`, pyo3 0.29 — marker.rs:561). A tool callback
   RE-ATTACHES for exactly the duration of its Python call
   (`Python::attach`, marker.rs:413; the attach guard restores the detached
   state on exit) and detaches again after. No thread ever waits on the tool
   mutex while holding the GIL (every `.answer` detaches before it can reach
   the mutex), so the lock ordering is safe. Stated hazard: tool calls
   serialize on the host mutex, and a callable that re-enters the SAME
   `Runner` deadlocks — callables answer, they do not recurse.

6. **Structured abstention (ADR-0024 follow-up 4, closed).** `AutoAbstained`
   carries `reason` (str | None), `distance` (float | None — None for a
   wrong-shaped input with no text to measure), and `threshold`
   (float | None) as attributes; the composed message string is unchanged.
   Additive: message-only callers keep working.

## alternatives considered

**Per-name callables inside `HostTools`** (a `BTreeMap<String, Callback>`
variant): closest mirror of the Python dict, but it either drags pyo3 types
into `auto-runtime` (layering violation — the runtime stays Python-free) or
boxes N closures where one dispatch closure plus a name set carries the same
information; the name set is what the loader checks either way. **Async
callbacks** (`func_wrap_async` + Python awaitables): wasmtime supports it,
but the executor is synchronous end to end and the tools this closes over
(dict lookups, local computation) are sync; recorded, not built. **Holding
the GIL across the whole `.answer`** (skip detach, no re-attach dance):
simpler choreography, but a long wasm run would freeze every other Python
thread — the exact thing ADR-0024 decision 6 exists to prevent. **Per-call
GIL retention for the callback's whole tool span** (attach once at first
tool call, release at `run` exit): fewer transitions on tool-heavy programs,
but wrong default — it starves host threads for the full remainder of the
wasm call, and transitions are cheap next to a wasm run. **Subprocess tools
from inside Python** (reuse `HostTools::Live` with argv): already works
today, but pays the process boundary per tool call that embedding exists to
remove, and turns Python functions into CLI shims.

## consequences

- A Python agent can hold a capability `.cbin` resident and serve its
  declared tools as plain functions; confinement holds — the artifact still
  cannot import anything but `auto.tool_call`, cannot reach an undeclared
  name (seam allowlist), and cannot load at all against a wrong table.
- The loader remains the single enforcement point; the embedded path added a
  stricter-by-choice exactness check in `auto-py`, not a second loader.
- ADR-0024's frozen v0 refusal message is superseded (decision 3);
  `evals/embedded-python/README.md` quotes the old text and needs the same
  correction.
- Python-side behavior is exercised via maturin + a smoke script
  (orchestrator-run); `cargo test -p auto-py` still needs no interpreter —
  the FFI stays cfg'd out of the unit-test binary.

## recorded follow-ups

- **Per-request tool policy + accounting** — who may invoke which tool,
  charged to whom (the other half of ADR-0024's follow-up; same shape as the
  per-request tier-0 spend gap, spec/runtime.md §8).
  *Landed (budget half):* serve per-request cap per ADR-0028; embedded
  per-answer cap (`max_tool_calls=`) per ADR-0032 — authorization/accounting
  itself remains recorded.
- **napi twin parity** — the same `tools=` seam for Node/TypeScript over the
  identical `HostTools::Callback`.
- **Async callbacks** — awaitable tools via `func_wrap_async`, if measured
  tool latency ever warrants it.

## sources

- wasmtime 46.0.1 vendored source: `IntoFunc: Send + Sync + 'static` and the
  `Fn(..) + Send + Sync + 'static` impls — src/runtime/func.rs:1845,1913;
  host-panic catch/resume — src/runtime/vm/traphandlers.rs:264,446.
- pyo3 0.29.0 vendored source: `Python::attach` — src/marker.rs:413;
  `Python::detach` (`F: Ungil + FnOnce() -> T`) — src/marker.rs:561;
  `PyErr::value` — src/err/mod.rs:231.
