# ADR-0025: task-scope verification — recorded task-level I/O, trace-mode witnessing

status: accepted · scope: `sdk/python`, `sdk/typescript`
(record task I/O), `crates/auto-trace` (wire + store + report),
`crates/auto-contract` (harness trace-mode task scope),
`crates/auto-backend` (task-level gather), `spec/trace.md`,
`spec/contract.md` §5, `evals/toy-agent`. Emit paths unchanged.

## context

The oldest recorded gap (S2, open-questions "contracts"): `scope = "task"`
is declarable but traces carry span-level I/O only, so nothing can witness a
whole-run claim — the harness refused with a loud
`TaskScopeUnverifiable` error and the contract spec called a task-scoped
contract "a declaration awaiting a subject that can witness it". This ADR
builds that witness: SDKs record whole-run input/output, the wire and store
carry them, the determinism report measures them, and trace-mode
verification of task-scope contracts works. Compilation of task scope is
explicitly NOT here: `auto compile` / `auto distill` keep refusing task
scope with their existing messages (emit is span- or region-scoped in v0;
regions already cover multi-span compilation, and task-scope synthesis
stays recorded future work).

## decision

1. **SDK surface.** Python: `Tracer(task=..., task_input=<value or None>)`
   plus `t.set_task_output(value)`; TypeScript mirror: constructor option
   `taskInput`, `setTaskOutput(v)`. `None`/null/undefined mean "not
   recorded" in both positions — a task input/output of JSON null is not
   recordable (uniform absence semantics beat a representable-null asymmetry
   between input and output). `set_task_output` twice is an **error**, never
   a silent last-wins; `None` is rejected with a message saying to leave it
   uncalled. Serialization happens before the once-flag is taken, so a
   NaN/Infinity rejection burns nothing.
2. **Wire: tolerant-additive, `v` stays 0.** The header line gains an
   optional named field `task_input` (emitted only when supplied; `null`
   reads as absent). The output cannot ride the header — the header line is
   already on disk when the agent finally knows its output, and the stream
   is append-only — so `set_task_output` appends a new line type:
   `{"v":0,"t":"task_output","trace_id":...,"output":...,
   "recorded_at_ms":...}`, at most once per trace (duplicate = parse
   error). Parsers stay strict: the schema is widened by *named* optional
   extensions, `deny_unknown_fields` still rejects genuinely unknown ones.
   Hard invariants held: every pre-existing recorded file parses unchanged,
   and a new file that uses no task I/O is byte-identical to today's
   emission (pinned by SDK golden tests and the rust round-trip property).
3. **Model.** `TraceHeader` gains `task_input: Option<Value>` and
   `task_output: Option<TaskOutput>` (`TaskOutput` = value +
   `recorded_at_ms`); the parser folds the `task_output` line into the
   header. `TraceHeader::task_observation()` yields the pair only when BOTH
   are present — a run recording one of the two is **partial** and
   witnesses nothing; consumers count it honestly instead of inventing the
   missing half.
4. **Store: schema v2, in-place migration.** Three additive nullable
   columns on `traces` (`task_input`, `task_output`,
   `task_output_recorded_at_ms`); `PRAGMA user_version` 1 → 2 via
   `ALTER TABLE ADD COLUMN` guarded by a `pragma table_info` check (an
   interrupted migration resumes); failures are loud; v1 rows read as "no
   task I/O". Older builds refuse a v2 store loudly rather than misread it.
5. **Determinism report: conditional task-level section.** Same rules as
   spans (grouped by task-input digest, witnessed ≥ 2, deterministic iff
   one distinct output digest), counted over whole-run observations, with
   partial traces named. Appended ONLY when at least one trace carries task
   I/O — reports over stores without it render byte-identical (the toy e2e
   greps pin the old lines).
6. **Harness trace mode.** `Scope::Task` observations = one per
   task-observation-bearing trace; examples/properties/interface checks run
   unchanged over them. Latency budget: measured from the one honest source
   — recorded wall-clock, header `started_at_ms` → output
   `recorded_at_ms`, same recorder clock. Cost/token budgets: no task-level
   declaration channel exists, so they stay Unchecked (never fabricated).
   Zero task-I/O-bearing traces → Unchecked "task-level observations
   present" with a how-to-record detail (Inconclusive), replacing the
   `TaskScopeUnverifiable` error, which is **deleted** — after this change
   nothing raised it (the emit paths refuse task scope in `auto-cli` with
   their own messages before any harness/gather call, verified).
7. **Differential gather.** `gather_observations` with `Scope::Task` groups
   whole-run observations by canonical task input (errors always 0 — there
   is no task-level error channel; a failed run declares no output and is
   not an observation). The emit gates never reach it for task scope.
8. **Replay: no role.** Task I/O is a whole-run record, not a call; replay
   matchers skip `task_output` lines and ignore `task_input`.

## alternatives considered

**Version bump (`v: 1`) for task-I/O-bearing files.** Strictly worse than
tolerant-additive: old parsers would refuse ALL new recordings, including
ones that never use the feature. With named optional extensions, the only
incompatibility is old parser × task-I/O-bearing file — and it fails
loudly (unknown field / unknown line type), never a silent misread, which
is what the bump rule exists to prevent. The bump stays reserved for
changes to existing line semantics.

**Task output on the header line.** Requires rewriting line 1 after the
run (seek-and-patch on a canonical-JSON line whose length changed) or
buffering the header until close (a crashed run would leave spans before
any header — today's crash artifact is a parseable partial trace). The
append-only dedicated line preserves both properties and survives a crash
after declaration.

**Deriving task I/O instead of recording it** (first span's input / last
span's output). Fabrication: the agent's wrapper structure does not define
its task interface, and the whole point of the ledgered gap was that
nothing WITNESSED task-level behavior. Explicit declaration or nothing.

**Task-level latency from last-span-end.** Measures when the agent stopped
calling tools, not when it produced its answer. `recorded_at_ms` at the
`set_task_output` call is the agent's own declaration instant, same clock
as the header — the honest wall-clock. (An agent that declares late
measures late: its declaration is the only witness there is.)

**Silent last-wins on repeated `set_task_output`.** A verification subject
whose recorded output can be quietly replaced is not a record; honesty is
load-bearing, so the second call raises.

## consequences

- The S2 gap closes for verification: `auto verify` of a task-scope
  contract against a trace store returns a real verdict (the toy e2e
  live-fires a PASS from recorded reality).
- Emit still refuses task scope — a task-scope verdict claims nothing
  about compilability. Task-scope SYNTHESIS remains in open-questions.
- `TraceHeader` gained fields: every struct-literal constructor updates
  (auto-trace, harness/backend test fixtures here; `auto-cli`
  `ingest_deopt_observation`, `auto-proxy` record, `auto-daemon` test
  fixtures at their owners' wiring sites — all `None, None`).
- `auto-daemon`'s watch loop previously relied on gather ERRORING for task
  scope to report "unwatchable contract"; gather now succeeds, so the
  daemon needs its own upfront task-scope refusal (watching a scope that
  `auto compile` refuses would fire recompiles that always fail). One
  guard clause at its owner's wiring site.
- Cost/token budgets at task scope are Unchecked until a task-level
  billing declaration channel exists (open question: aggregate span attrs?
  a reserved task-level attr? nothing is fabricated meanwhile).
