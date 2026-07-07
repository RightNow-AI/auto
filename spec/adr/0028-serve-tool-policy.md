# ADR-0028: `auto serve` per-request tool-call budget — bounding side-effectful execution

status: accepted · scope: `crates/auto-serve`
(`ServeConfig::max_tool_calls_per_request`, `ServerState::with_tool_policy`,
the `budgeted` wrapper, `spawn_tool`), `crates/auto-cli` (a
`--max-tool-calls-per-request` flag — wiring), `spec/adr/open-questions.md`

## context

ADR-0017 (amended wave 7) gave `auto serve` a server-wide `--tool` table so
capability artifacts can run their declared tools, and stated plainly that "a
per-request tool policy remains the deliberate residual gap". ADR-0011
decision 2 recorded the sibling gap for tier-0 spend (no per-request policy
exists — who authorizes it, charged to whom). ADR-0027 recorded the same open
item as "per-request tool policy + accounting".

The concrete exposure: a requester who can reach `POST /run/<id>` for a
capability artifact can drive as many side-effectful tool subprocesses as the
artifact's program will issue, with no ceiling. This ADR closes the *budget*
half — a hard per-request cap on tool calls plus an audit line — so a caller
cannot trigger unbounded side effects. It does NOT close the *authorization /
accounting* half (which caller may invoke which tool, charged to whom); that
stays recorded, the same shape as ADR-0011/0027.

## decision

1. **`--max-tool-calls-per-request N` (absent = today's behavior).**
   `ServeConfig::max_tool_calls_per_request: Option<u64>`. `None` is unchanged:
   the operator's Live table is forwarded as-is, unlimited, no audit — pure and
   unbudgeted servers are byte-identical to wave 9. `Some(n)` caps a single
   request at `n` executed tool calls.

2. **Enforced at the `HostTools::Callback` seam (ADR-0027), not in the
   runtime.** With a budget, `with_tool_policy` wraps the operator's
   `HostTools::Live(table)` in a counting `HostTools::Callback` over the same
   name set (`budgeted`). On each call the wrapper increments a shared counter;
   once the running count exceeds `n` it returns
   `Err("tool budget exceeded: N per request (ADR-0028)")`. That err becomes the
   tool-call err envelope the interpreter turns into an honest trap
   (executor.rs `host_tool_call`), so the over-budget request is a **500 with an
   honest body**, never a truncated-but-200 answer. The Callback's `names` are
   the table's keys, so the loader's capability cross-check runs unchanged
   (ADR-0027 decision 1) — the budget adds a counter, it does not weaken
   confinement.

3. **Per-request reset via a shared counter; correct because the server is
   sequential.** The counter is an `Arc<AtomicU64>` held by `ServerState` and
   captured by the wrapper closure; `run` stores `0` into it at the top of every
   `POST /run/<id>`. The server is a blocking sequential loop (ADR-0011
   decision 4), so exactly one request touches the counter at a time and the
   reset-then-execute sequence is race-free. **Stated dependency:** the recorded
   thread-per-request upgrade (ADR-0011) would share one counter across
   in-flight requests, so it MUST move the counter into per-request state
   (per-request `ServerState`, or a thread-local) before going concurrent — a
   shared counter with a per-request reset is a v0-sequential design, and this
   is written down, not assumed away.

4. **The wrapper owns the execution it counts (`spawn_tool`).** The runtime does
   not expose Live execution as a standalone public function — it runs only
   inside `WasmExecutor::execute` via the private `HostTools::call`. So the
   budgeted Callback cannot delegate to the Live host; serve reimplements the
   one Live arm (`spawn_tool`): argv lookup, canonical-JSON input appended as the
   final argument, exit-0 required, stdout parsed as the output value — the
   tier-0 command contract (spec/runtime.md §3), byte-for-byte in step with
   `auto-runtime`'s `HostTools::Live`. Canonical JSON (`auto-trace`, moved from a
   dev-dep to a dep) keeps the bytes a budgeted tool receives identical to an
   unbudgeted Live tool; the wire format is canonical everywhere (ADR-0027
   decision 4). A divergence from the runtime arm is a bug.

5. **Audit reflects executed calls only.** A call that passes the budget logs one
   line to stderr, `tool audit: <tool> call #<k>`, then runs the tool. A call
   refused by the budget is NOT audited — the tool never ran, so an audit line
   would imply a side effect that did not happen; its refusal is carried by the
   err envelope and the 500. The running counter still counts the refused
   attempt (so `#k` is the true attempt index), it just executes and audits
   nothing.

6. **Budget only; authorization and accounting stay recorded.** This bounds *how
   many* tool calls a request may make. *Which* caller may invoke *which* tool,
   and *charged to whom*, is untouched — the same unresolved policy as ADR-0011
   decision 2 and ADR-0027's recorded follow-up. Recorded, not built.

## alternatives considered

**Count inside the runtime (touch `executor.rs`).** Put the budget in
`HostTools::call` or `WasmExecutor::execute`. Rejected: the seam for exactly
this already exists (ADR-0027's `Callback`), per-request semantics belong at the
request boundary (serve owns "a request"), and the runtime is shared by `auto
run`, the emit gate, and `auto-py` — none of which have a per-request notion.

**Audit-only + refuse the flag at startup.** The fallback if per-request
counting were impossible without a runtime change. It is possible (decision 3),
so an honest budget beats an honest refusal here — the refusal was reserved for
the case where a real counter could not be built, which did not arise.

**Audit every attempt, including the refused one.** More visible in the log, but
a `tool audit: … call #k` line for a call that never spawned the tool overstates
side effects; the 500 + err envelope already record the breach. Kept the audit
to real executions.

**`Arc<Mutex<u64>>` for the counter** (as first sketched). `AtomicU64` is the
lock-free equivalent and the `Callback` seam already serializes closure
invocations behind its own mutex, so `Relaxed` fetch-add is sufficient and
carries no second lock.

**Audit / budget always on (ignore the flag's absence).** Rejected: "absent =
today's behavior" is a byte-identical promise — audit noise and a ceiling are
opt-in with the flag, off without it.

## consequences

- A requester can no longer drive unbounded side-effectful tool execution
  against a budgeted server: the `n+1`-th tool call in one request is refused and
  the request fails 500. Pure servers, and servers without the flag, are
  unchanged.
- `auto-trace` becomes a runtime dependency of `auto-serve` (canonical_json for
  `spawn_tool`). No cycle: `auto-runtime` already depends on `auto-trace`.
- The thread-per-request upgrade (ADR-0011) inherits a hard requirement: isolate
  the tool-call counter per request, or the budget silently mixes requests.
- `spawn_tool` duplicates the runtime's Live arm by necessity (no public Live
  execution API). The budget/audit logic is unit-tested with a fake inner (no
  process); the subprocess path is exercised by the loopback e2e
  (`evals/serve-proxy`, orchestrator-run). Keeping the two arms in step is a
  maintenance obligation, noted in `spawn_tool`'s doc.
- The authorization/accounting gap is unchanged and stays in open-questions.

## sources

- `auto-runtime` seam (crates/auto-runtime/src/executor.rs): `HostTools::callback`
  builds a `Callback { names, call }` (constructor ~line 107); the dispatch
  allowlist-checks `names` before invoking the closure, and locks
  `call: Arc<Mutex<ToolCallback>>` per invocation (~lines 152–166); a tool `Err`
  becomes the `{"err"}` envelope the interpreter traps on
  (`host_tool_call`, ~lines 194–203). `HostTools::call` is private — no
  standalone public Live execution — which forces `spawn_tool` (decision 4).
- `auto-dsl` `eval_pipeline` invokes the host exactly once per `Stage::Tool`
  (crates/auto-dsl/src/lib.rs ~line 386), so an N-tool pipeline makes N calls —
  the basis of the over-budget test.
- `std::sync::atomic::AtomicU64::{fetch_add, store}` with `Ordering::Relaxed`;
  Rust std, edition 2024.
