# ADR-0032: embedded per-answer tool budget — bounding host-callable execution in `auto-py`

status: accepted · scope: `crates/auto-py`
(`Runner(…, max_tool_calls=)`, `logic::budgeted`,
`logic::budget_on_pure_message`)

## context

ADR-0028 capped tool calls per REQUEST on `auto serve`: the `n+1`-th call in
one request is refused at the `HostTools::Callback` seam, becomes the err
envelope, the artifact traps, the request fails 500 honestly. The embedded
twin had no bound: ADR-0027's `Runner(…, tools={name: callable})` dispatches
host-callable invocations with no ceiling, so one `.answer()` can trigger
unbounded side-effectful execution in the host process — the exact exposure
ADR-0028 closed for serve, one surface over. This ADR closes it at the
BINDINGS layer (`auto-py`), where the notion of "one answer" lives; the
runtime has no per-answer concept and stays untouched (the same argument
that kept ADR-0028 out of `executor.rs`). The authorization/accounting half
(who may invoke which tool, charged to whom) remains recorded, as in
ADR-0011/0027/0028.

## decision

1. **`Runner(artifact_path, tools=None, max_tool_calls=None)`.**
   `max_tool_calls: int | None`. `None` is today's behavior — unlimited,
   no audit, byte-identical (no counter is even allocated). `max_tool_calls=N`
   caps ONE `.answer()` at `N` EXECUTED host-callable invocations.

2. **Enforced at the dispatch seam, in `auto-py`, not the runtime.** With a
   budget, the ADR-0027 dispatch closure is wrapped by `logic::budgeted`
   before `HostTools::callback` sees it: the wrapper counts, refuses, audits,
   then delegates. The Callback's `names` are unchanged, so the loader's
   capability cross-check runs exactly as before (ADR-0027 decision 1) — the
   budget adds a counter, it does not weaken or duplicate confinement. The
   wrapper is pyo3-free and lives in `logic.rs`, so the budget/reset counting
   is unit-tested without a Python interpreter.

3. **The `n+1`-th call is refused WITHOUT invoking the callable.**
   `Err("tool budget exceeded: N per answer (ADR-0032)")` → the `{"err"}`
   envelope → the artifact traps honestly → `.answer()` raises `AutoError`
   carrying the message (the ADR-0028 pattern, embedded). The refusal fires
   before the closure would `Python::attach`, so no Python runs for a refused
   call — an over-budget attempt never touches the interpreter.

4. **Per-answer reset via a shared `AtomicU64`; the overlap hazard is
   stated.** The frozen `Runner` pyclass holds `Option<Arc<AtomicU64>>`
   (interior mutability with the same `Send + Sync` story the callback
   already has); `.answer()` stores `0` into it at its top, before
   `Python::detach`. Counting takes no lock and no GIL — the Callback seam
   already serializes invocations behind its mutex, so `Relaxed` suffices
   (ADR-0028's reasoning). Stated hazard, the embedded twin of ADR-0028
   decision 3's sequential-server dependency: the counter is per RUNNER,
   reset per answer, so the budget is exact only while `.answer()` calls on
   one `Runner` do not overlap; overlapping answers from multiple Python
   threads mix their counts. True per-answer isolation needs per-execution
   host state — the runtime clones one `HostTools` into the executor at
   construction, so there is no per-answer identity at the seam without a
   runtime change. Recorded, not built.

5. **The counter holds EXECUTED calls; audit reflects executed calls only.**
   Each executed call logs one stderr line
   `tool audit: <name> call #<k> (embedded)` — ADR-0028 decision 5's rule
   (a refused call never ran, so an audit line would claim a side effect
   that did not happen; the breach is carried by the err envelope and the
   raised `AutoError`), with an `(embedded)` suffix so a merged log
   distinguishes this surface from serve's `tool audit: <tool> call #<k>`.
   Unlike serve's counter (which counts attempts), this counter advances only
   on execution — `#k` IS the executed index by construction, and refusals
   repeat identically forever. Observable behavior is identical either way
   (refused attempts are unaudited in both); only the internal count differs.
   A callable that raises or returns garbage still EXECUTED — it consumes
   budget and its own error propagates, not the budget message.

6. **A budget on a pure artifact refuses at LOAD.** `max_tool_calls` with no
   tool host — `tools=None`, or the pure `tools={}` form — is
   `AutoError("max_tool_calls=N on a pure artifact: a tool budget needs
   tools= — a pure artifact makes no tool calls, so there is nothing to
   bound (ADR-0032)")`. Deliberately STRICTER than serve, where a budget with
   no `--tool` table is vacuously satisfied (one server-wide flag spans many
   artifacts, some pure): an embedded `Runner` holds exactly one artifact, so
   a budget that can never act is a caller bug — refuse loud at load
   (ADR-0027 decision 3's embedded-strictness precedent). On a capability
   artifact with `tools=None`, the ADR-0027 capability refusal outranks the
   budget complaint: supplying `tools=` makes the budget meaningful as-is.

7. **Node parity: recorded, not built.** `auto-node` remains pure-only — the
   `tools=` seam itself is still ADR-0027's recorded napi follow-up — so
   there is no host-callable execution to bound there yet. The budget rides
   in with the seam when that lands.

## alternatives considered

**Count inside the runtime** (`HostTools::call` / `WasmExecutor::execute`):
rejected for the same reasons as ADR-0028 — the Callback seam exists for
exactly this, "one answer" is a bindings-layer notion, and the runtime is
shared by surfaces with no such notion. **Attempt-counting (serve's
mechanics, `fetch_add` per call including refusals):** observably identical
(audit lines and refusal behavior match exactly); executed-counting was
chosen so the counter IS the executed-call count — the quantity the budget
bounds — rather than an attempt index needing interpretation. **Vacuous
acceptance of a budget on a pure artifact (serve's choice):** rejected
embedded; decision 6. **Per-answer host construction (true isolation under
overlapping answers):** `auto_runtime::Runner` wires the host in at
construction (`new_with_tools`) and compiles once; rebuilding per answer
re-links per call and destroys the rung's economics (ADR-0024) for a hazard
that is stated instead. **A `threading.local` counter on the Python side:**
would isolate overlapping answers per thread, but counting must happen
inside the dispatch closure GIL-free (decision 3 refuses before attaching);
a Python-side counter would force an attach per refusal and put policy in
the embedder's hands. **Clamping/erroring on negative budgets in Rust:**
unreachable — pyo3 extraction of `Option<u64>` already rejects a negative
int at the call boundary (`OverflowError`), before any auto-py code runs.

## consequences

- A resident Python agent can bound the blast radius of one `.answer()`:
  the `N+1`-th host-callable invocation within an answer is refused before
  any Python runs, and the answer fails as an honest `AutoError`, never a
  truncated-but-successful answer.
- `max_tool_calls=None` is byte-identical to ADR-0027 behavior — no counter,
  no audit lines, same host construction.
- The overlap hazard (decision 4) is stated in the class docstring, the
  crate doc, and here; the recorded fix is per-execution host state at the
  runtime seam.
- `evals/embedded-python` gains three smoke directions (orchestrator-run,
  maturin): budget 1 answers and audits `#1` per answer (reset proof),
  budget 0 refuses the first call as an `AutoError` trap, budget-on-pure
  refuses at load. `cargo test -p auto-py` still needs no interpreter.
- The budget wording (`per answer` vs serve's `per request`) and the
  `(embedded)` audit suffix keep the two surfaces distinguishable in logs
  and error reports.

## recorded follow-ups

- **Per-execution host state** — true per-answer counter isolation under
  overlapping `.answer()` calls on one `Runner` (needs the runtime to carry
  per-execute host identity; today one `HostTools` is cloned in at load).
- **napi twin** — the same budget rides in with `auto-node`'s `tools=` seam
  (ADR-0027 recorded follow-up); node is pure-only today, so there is
  nothing to bound yet.
- **Authorization/accounting** — unchanged from ADR-0011/0027/0028: which
  caller may invoke which tool, charged to whom. Recorded, not built.

## sources

- ADR-0028 (`crates/auto-serve/src/api.rs`): `budgeted` wrapper (~line 114)
  — count, refuse with `tool budget exceeded: {budget} per request
  (ADR-0028)`, audit executed calls only (`tool audit: <tool> call #<k>`,
  ~line 127); per-request reset at the top of `/run` (~line 423).
- `auto-runtime` seam (`crates/auto-runtime/src/executor.rs`):
  `HostTools::callback` (~line 107) and the per-call allowlist + mutex
  (~lines 152–166); `HostTools` is cloned into the linker closure at load
  (~lines 450–458), so host identity is per-Runner, not per-answer
  (decision 4's constraint).
- `auto-py` GIL choreography (ADR-0027 decision 5, `bindings.rs`):
  `.answer` detaches before the wasm call; `dispatch` attaches per tool
  call — the budget check sits before that attach.
- pyo3 0.29: `Option<u64>` parameter extraction rejects negative ints with
  `OverflowError` at the boundary (unsigned conversion), before user code.
