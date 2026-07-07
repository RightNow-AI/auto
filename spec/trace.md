# Auto traces — specification, v0

Status: v0, matches `crates/auto-trace` and `sdk/python` as merged. Emission
format version: **0** (the `v` field; the ADR-0025 task-level I/O additions
are named optional extensions of the strict v0 schema, not a version bump).
Store schema version: **3** (`PRAGMA user_version`; older stores migrate in
place, in sequence — §6). Where prose and code disagree, the validation rules
implemented in `auto-trace` win; this document is written for external readers.

A **trace** is one recorded agent run. Traces are the compiler's raw
material: determinism analysis over traces of the same **task** measures how
much of the agent's behavior is secretly symbolic, and replay re-executes a
run against its recorded world. Nothing in this layer estimates — every
number reported is a measurement over recorded data.

## 1. concepts and identity

- **task** — a string label naming a unit of agent work ("triage-email").
  Determinism analysis groups traces sharing a task. The label is chosen by
  the code constructing the tracer and recorded in the header.
- **trace id** — 128-bit, minted by the SDK per run (random), rendered as 32
  hex chars. Never minted by the rust side.
- **span** — one recorded operation. `span_id` (u64, unique per trace) is
  assigned at span *open*. `seq` (u64, strictly increasing) is also assigned
  at open and is the authoritative order.
- **line order ≠ seq order.** SDKs write a span's line when it *closes*, so
  an enclosing span's line appears after its children. Parsers sort by `seq`.
- **parenting** — `parent_span_id` points at the enclosing structural span;
  a parent always has a smaller `seq` than its children (validated).

## 2. span kinds

| kind | records | effectful |
|---|---|---|
| `model_call` | a model invocation (`name` = model/routing label) | yes |
| `tool_call` | an external tool invocation (`name` = tool) | yes |
| `env_read` | an environment variable read (`name` = variable) | yes |
| `memory_op` | a task-memory operation (`name` = read\|write\|append) | yes |
| `branch` | a decision (`output` = the decision value) | yes |
| `span` | structural grouping only | no |

**Effectful** spans participate in determinism analysis and replay;
structural `span` nodes never do.

## 3. emission format (JSONL, v0)

One file = one trace. UTF-8, one JSON object per line, blank lines tolerated.
The first non-blank line is the header; every other line is a span or the
at-most-once task output declaration (below).

Header line fields: `v` (must be 0), `t` (`"trace"`), `trace_id` (32 hex),
`task`, `started_at_ms` (unix epoch ms), `sdk` (e.g.
`"auto-sdk-python/0.1.0"`), `attrs` (string→string map), and optionally
`task_input` (any non-null JSON — the whole-run input, ADR-0025). The field
appears only when the recording code supplied one; `task_input: null` reads
as absent.

Span line fields: `v` (0), `t` (`"span"`), `trace_id` (must equal the
header's), `span_id`, `parent_span_id` (or null), `seq`, `kind` (§2 wire
strings), `name`, `input` (any JSON), `output` (any JSON or null), `error`
(string or null), `started_at_ms`, `duration_ms`, `attrs`.

**Task-level I/O (ADR-0025).** A run may declare its whole-run input and
output. The input rides the header (`task_input`, above — known at tracer
construction). The output is declared exactly once via the SDK's
`set_task_output(value)` and is appended as its own line — the header line
is already on disk by then; the stream is append-only:

`{"v": 0, "t": "task_output", "trace_id": ..., "output": <any JSON>,
"recorded_at_ms": <unix epoch ms>}`

At most one `task_output` line per trace (a duplicate fails the parse, and
the SDKs raise on a second `set_task_output` call — no silent last-wins).
`recorded_at_ms` is stamped by the same recorder clock as the header's
`started_at_ms`; their difference is the run's task-level wall-clock
(run start → output declared), the honest latency source for task-scope
budget checks (spec/contract.md §5). `None`/null/undefined mean "not
recorded" in the SDK APIs, so a task input or output of JSON null is not
recordable. A parsed trace carries both values on its header
(`TraceHeader.task_input` / `.task_output`); a run recording only one of
the two witnesses nothing at task level — consumers count it as partial,
never inventing the missing half. A file that uses no task-level I/O is
byte-identical to the pre-ADR-0025 format.

**Reserved attrs.** Two span attr keys are reserved: `cost_usd_micros` and
`tokens` — decimal u64 strings set by the *recording agent*, declaring what
its API billed for that call (micro-usd: 1,000,000 = $1). They are the
agent's own declaration, never computed by this layer; SDKs carry them
through like any other attr, and the verification harness
(spec/contract.md §6) reads them for cost/token budget checks — a malformed
value fails verification loudly. All other attr keys are free-form.

**Strictness.** Parsers reject: unknown fields, unknown `t` or `kind`
values, `v` ≠ 0, missing header, duplicate headers, spans (or task_output
lines) whose `trace_id` differs from the header, duplicate `span_id` or
`seq`, duplicate `task_output` lines, parents that do not exist or did not
open earlier. No best-effort reads. The task-level I/O additions widen the
strict schema with *named* optional fields — parsers stay strict against
genuinely unknown fields; a pre-ADR-0025 parser reading a task-I/O-bearing
file refuses loudly (unknown field / line type), never misreads it.

**Value space.** `output: null` and an absent output are one value (JSON
cannot distinguish them); recorded values must be real JSON — the python SDK
raises on NaN/Infinity rather than corrupting the file. Failure and success
are distinguished by `error`: a failed call records `error` plus `output:
null`.

## 4. digests and canonicalization

Determinism grouping and replay matching compare values by sha-256 over
**canonical JSON** (object keys sorted, compact separators). Two rules keep
this sound:

1. **Digests are implementation-local, never wire data.** The python SDK
   compares python-computed digests (replay); the rust side computes its own
   digests at ingest (analysis). Cross-language digest equality is never
   required, so float-formatting differences between languages cannot
   corrupt results.
2. **Bit-exact carriage.** The rust side parses JSON with exact float
   round-tripping (`serde_json` `float_roundtrip`; the default fast parser is
   lossy at extreme magnitudes — caught by the round-trip property test).

The one digest that crosses the wire is `env_read`'s recorded *output*
`{"digest", "len"}` — it is compared as an opaque JSON value, produced and
consumed by the same SDK.

## 5. secrets

`env_read` records a sha-256 digest and the length of the value — **never
the value**. This is the default and only mode. Prompt/args payloads of
model/tool calls are the caller's data domain and are recorded verbatim;
keeping secrets out of those is the caller's responsibility (a scrubbing
layer is an open question).

## 6. the store

Rust-owned sqlite (`rusqlite`, bundled, STRICT tables, WAL), schema version 3
in `PRAGMA user_version`. Older stores migrate on open, in sequence — v1→v2
adds three additive nullable columns on `traces` for task-level I/O
(`task_input`, `task_output`, `task_output_recorded_at_ms`; ADR-0025), and
v2→v3 adds one column `partial` (`NOT NULL DEFAULT 0`) for torn-tail recovery
(§12; ADR-0030). Each migration is an `ALTER TABLE ... ADD COLUMN` guarded by a
`pragma table_info` check so an interrupted migration resumes; old rows read as
their pre-migration default (no task I/O; `partial = 0`, complete); any
migration failure is a loud error. Any other version is rejected loudly —
migrations are decisions, never silent (an older build refuses a newer store
rather than misreading it). Traces are immutable: re-ingesting a trace id is an
error. `input_digest`/`output_digest` columns are computed at ingest — digests
are never trusted from input. All u64 wire values must fit i64 for storage;
out-of-range values are rejected at ingest.

## 7. determinism report

For one task, over all its ingested traces:

- **signature** = `(kind, name, input_digest)` — same operation, same input.
- A signature is **witnessed** iff observed ≥ 2 times across runs.
- A witnessed signature is **deterministic** iff zero observations errored
  and all outputs are identical (one distinct output digest); otherwise it is
  **divergent**.
- A signature observed once is **unwitnessed**: *no claim is made about it*,
  in either direction.

The report gives: effectful/structural span counts, witnessed coverage,
deterministic and divergent span counts, the deterministic fraction **of
witnessed spans** (by count, and weighted by recorded duration), per-kind
breakdowns, and the most-observed divergent signatures. Fractions are `None`
("no data") when the denominator is empty — including the time-weighted
fraction when all recorded durations are zero. Nothing is extrapolated to
unwitnessed spans; the headline number is meaningless without its coverage
and is always printed with it.

**Task-level section (ADR-0025).** When at least one trace of the task
carries task-level I/O, the report appends a task-level section applying the
same rules to whole-run observations: one observation per trace recording
BOTH a task input and a task output, grouped by task-input digest, witnessed
iff observed ≥ 2, deterministic iff all witnessed outputs are identical.
Traces recording only one of the two are counted as partial and excluded —
named, never guessed at. Stores without any task-level I/O render
byte-identical to the pre-ADR-0025 report: the section simply does not
exist.

## 8. replay

**SDK replay mode** substitutes the recorded world for the live one
(effectful spans only; structural spans play no role). Matching is
concurrency-tolerant (ADR-0029): each live effectful call consumes the
**first unconsumed** recorded span with the same `(kind, name, canonical
input)`, atomically. A sequential run arriving in recorded order consumes
exactly the sequence the old per-cursor matcher did — byte-identical
replay — while concurrent effectful calls (threads, `Promise.all`) match
order-independently instead of racing one shared cursor. Replay therefore
verifies the **multiset** of effectful calls and their recorded I/O, not
arrival order (cross-trace order comparison is the rust side's job, below).

- `tool_call` / `model_call` / `memory_op`: consume the matching recorded
  call; return its recorded output without executing. A recorded failure
  re-raises as `ReplayedError`.
- `env_read`: consume the matching recorded read; return the **live**
  value, but verify its digest matches the recording (a changed
  environment is a divergence — the SDK cannot invert a digest, by design).
- `branch`: consume the matching recorded decision; verify the live
  decision equals it (decisions are computed by the agent; replay
  witnesses them).
- **No match raises `ReplayDivergence`** naming the live `(kind, name)`, a
  canonical-input snippet, and what remains unconsumed: a live call whose
  `(kind, name)` still has unconsumed recordings is reported as an input
  mismatch; a fully consumed recording is reported as exhaustion. Changed
  environments and changed decisions raise after their span is consumed,
  as above. Replay may simultaneously record its own trace.
- **Divergent duplicates.** the same (kind, name, input) recorded twice
  with DIFFERENT outputs is assigned in recorded order by ARRIVAL; under
  concurrency arrival order is a race by construction — which concurrent
  call receives which recorded output is undefined. Sequential runs arrive
  in program order and are unaffected. Each recording is consumed exactly
  once either way.
- **End of replay.** Unconsumed recorded spans at exit are not an error
  (unchanged from the sequential matcher, which was also silent); the
  SDKs' `replay_remaining` / `replayRemaining` reports the count.
- Task-level I/O (§3) plays **no role** in replay matching: it is a
  whole-run record, not a call the agent makes. Replay loaders skip
  `task_output` lines and ignore the header's `task_input`.

**Rust comparison** (`auto-trace::replay::compare`) walks two traces'
effectful spans in `seq` order and reports the first divergence (signature /
output / length). Comparison stops at the first divergence because alignment
is lost beyond it.

## 9. versioning

- Emission format: the `v` field. Readers accept exactly 0; any change to
  fields or semantics of **existing** lines bumps it, with an ADR. Adding
  named optional fields or line types that old files never contain is
  additive, not a bump (the ADR-0025 task-level I/O additions are the
  precedent; rationale in `spec/adr/0025-task-scope.md`).
- Store: `PRAGMA user_version`, exactly 3; schema changes bump it with an
  explicit migration decision (v1→v2→v3 migrate in place, in sequence, §6).
- The rendered report and CLI output are debugging surfaces, not stable
  machine formats.

## 10. what this layer does not claim

No parity, cost, or latency numbers exist here. The determinism report
measures output stability of recorded calls under identical inputs; it does
not prove extractability (that is S4's job, with sandboxed verification), and
it says nothing about spans it did not witness at least twice.

## 11. recording proxy

`auto-proxy` records an OpenAI-backed agent with no code change: point the
agent's `base_url` at the proxy. It forwards each POST `/v1/chat/completions`
to the real upstream — carrying the caller's own `Authorization` header (the
proxy holds no key) — relays the response verbatim, and ingests the exchange
as a synthetic single-span `model_call` trace. The span `input` is the request
body verbatim (the prompt payload); the span `output` is the assistant text at
`choices[0].message.content`. The reserved attrs (§3) come from the response
usage: `tokens` = `prompt_tokens + completion_tokens`, and `cost_usd_micros`
from the pinned price table (ADR-0010) keyed on the request model — a model
absent from the table records `tokens` only, and a response with no usage
records neither. Recording is relay-first and best-effort: the response reaches
the caller before the store is touched, and an ingest failure is logged, never
raised. Honest gaps (v0): streaming is refused (not recordable as one
exchange), every trace is a single span (no tool-call or branch structure), a
tool-call-only reply (`content: null`) records a null output, and the raw
provider response envelope is not stored. See ADR-0012.

## 12. torn-tail recovery

A JSONL trace is written line-by-line, each record terminated by `\n`. A hard
kill mid-write (OOM, SIGKILL, power loss) leaves the final record truncated
with no trailing newline. The strict parser (§3) rejects the whole file for
that one fragment — discarding every committed span before it. **Recovery mode
(ADR-0030)** rescues the committed prefix. It is a separate, explicitly opt-in
entry point; the strict `parse_file` / `parse_str` path is unchanged and stays
the default everywhere.

**What qualifies.** Only an *unterminated* final line — one with no trailing
`\n` — that fails to parse is a torn tail. Recovery drops exactly that line and
parses the committed prefix strictly:

- A file that ends cleanly (trailing `\n`, or only trailing whitespace) has no
  torn tail: it is parsed strictly, so corruption on ANY line — middle or last
  — stays a strict error.
- An unterminated final line that is nonetheless a COMPLETE record (killed
  after the bytes, before the `\n`) is kept, **not** marked partial — nothing
  was lost.
- An unterminated final line that does not parse is dropped; the recovered
  prefix is returned **marked partial** with a description (line number, bytes
  dropped, reason). If the prefix itself fails to parse, the corruption is in a
  committed (middle) line — a **strict error even in recovery mode**. A torn
  header leaves no committed prefix, so there is nothing to recover (the file
  is empty of any complete record).

A **newline-terminated** corrupt final line is genuine corruption (the write
completed), never a torn tail. Only the last write can be interrupted, so there
is at most one torn tail; any earlier bad line is a committed corruption.

**Bytes, not text.** Recovery operates on raw bytes: a torn tail may be
truncated inside a multibyte UTF-8 character. The committed prefix (up to and
including the last `\n`) is always valid UTF-8 — `\n` never falls inside a
multibyte sequence — so it is decoded and parsed exactly as the strict reader
would; the dropped tail's bytes need not be valid UTF-8. Non-UTF-8 in a
*committed* line is corruption, not a torn tail, and is a loud error.

**The partial mark is carried, never lost.** A recovered trace is ingested as
partial (`Store::ingest_partial`), recorded in the store's additive `partial`
column (§6). Consumers keep it honest:

- **Determinism report (§7):** partial traces are excluded from witnessing and
  counted separately — a `partial traces excluded from witnessing (torn-tail,
  ADR-0030): N` line appears only when N > 0, so reports over stores with no
  partial traces are byte-identical to the pre-ADR-0030 output. A partial trace
  never witnesses a signature and never shifts a deterministic verdict.
- **Verification / replay:** the store's task loader returns COMPLETE traces
  only, so verification evidence and replay never rest on a truncated record;
  the tagged loader surfaces partials for a consumer that wants to report their
  exclusion. A task with only partial traces yields zero complete observations
  (Inconclusive), not torn evidence — never silently thinner, never silently
  wrong.

The wire format (`v`) is unchanged by recovery: torn-tail handling is a reader
concern, not a format change. The `Trace` value itself carries no partial flag
— partiality is a store/analysis property, held in the store column and in the
recovery/load result types.
