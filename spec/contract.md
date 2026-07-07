# Auto contracts — specification, v0

Status: v0, matches `crates/auto-contract` as merged. Contract format
version: **0** (the `contract_version` field). Where prose and code disagree,
the validation rules implemented in `auto-contract` win; this document is
written for external readers.

A **contract** is the behavioral type of a unit of cognitive work: what goes
in, what comes out, which concrete cases are normative, which properties
every output must satisfy, and what the work may cost. The constitution's
line is literal — **the contract IS the type system**. Verification against
the contract is what lets a compiled artifact claim parity with the agent it
replaced; a contract that cannot be checked honestly reports so (§7).

## 1. concepts

- **contract** — one file declaring the behavior of one subject. Belongs to a
  **task** (the same string label traces carry, spec/trace.md §1).
- **scope** — what the contract binds to: a whole task, or one operation
  signature (every recorded span with a given kind + name).
- **interface** — the typed input/output of the subject, in the IR's value
  type grammar (§4).
- **examples** — named concrete input→output pairs. Normative: a subject that
  produces anything else on that input fails.
- **properties** — a closed set of machine-checkable predicates over every
  observed output (§5). Closed on principle: a contract file must never be
  able to execute code at verify time.
- **budgets** — declared resource ceilings. Declarations, not measurements;
  a budget the harness cannot measure is reported as unchecked, never passed.
- **eval sets** — bulk cases in external JSONL files, for volume beyond the
  handful of inline examples.

## 2. file format (TOML)

One contract = one TOML file, utf-8. Conventional name:
`<name>.contract.toml`. Reference fixture:
`evals/toy-agent/fake-frontier.contract.toml`.

| key | type | required | meaning |
|---|---|---|---|
| `contract_version` | integer | yes | must be exactly **0** |
| `task` | string | yes | task label; matches trace task labels |
| `[scope]` | table | yes | see below |
| `[interface]` | table | yes | `input`, `output`: type strings (§4) |
| `[[example]]` | array of tables | no | `name`, `match`, `input`, `output` |
| `[[property]]` | array of tables | no | `kind`, `target`, kind-specific fields |
| `[budgets]` | table | no | optional integer ceilings |
| `[acceptance]` | table | no | statistical acceptance: declared differential agreement threshold (ADR-0018) + differential match mode (ADR-0021) |
| `eval_sets` | array of strings | no | JSONL paths relative to the contract file |

`[scope]`: `type = "task"` (no other keys); `type = "span"` with `kind`
(one of the **five effectful span kinds** of spec/trace.md §2: `model_call`,
`tool_call`, `env_read`, `memory_op`, `branch` — never structural `span`) and
`name` (the recorded span name); or `type = "region"` with `from` and `to`
(two DIFFERENT recorded span names bounding a chain — the region's interface
is from-input → to-output; structure and purity rules in spec/synthesis.md
§8). Region contracts compile (`auto compile`) but do not trace-mode verify
in v0 — `auto verify` refuses them honestly.

`[[example]]`: `name` (non-empty, unique per contract), `match` (`"exact"`
or `"judged"` — see below; exact is the plain mode and the only one that
needs no judge), `input` and `output` (arbitrary values; TOML tables/arrays/
strings/integers/floats/booleans map to their JSON counterparts). TOML
cannot write `null`, so a `unit`-valued example field is not expressible in
a contract file; JSONL eval cases can carry `null`. TOML datetimes are
outside the value space — do not use them.

**Judged matching (ADR-0019).** `match = "judged"` relaxes one example's
comparison from exact reproduction to **LLM-judged semantic equivalence**
for the contracted task: the subject runs exactly as usual, and a judge
model is asked whether the produced output and the expected output are
semantically equivalent. Semantics, in order: outputs that are exactly
equal (canonical-json equality) pass **without consulting the judge** — a
free short-circuit, noted in the check detail; a divergent output with
**no judge supplied** leaves the claim unchecked → the verdict is
Inconclusive, never Pass; a judge verdict of not-equivalent, or a failed
judge call, **fails** the example — a judge failure never passes. Every
check that rests on a judge verdict says **JUDGED** in its detail and names
the judge: a judge is a model with opinions, not exact reproduction, and
the eval run record keeps that distinction visible. The judge is itself a
paid frontier call inside the gate and rides the ADR-0010 spend rails
(capped client, append-only ledger). `match` is part of the canonical
example form, so exact-vs-judged is id-bearing (§8). Judged comparison
applies to examples here; the differential reproduction claim is relaxed
separately, by declaring `[acceptance] differential_match = "judged"`
(ADR-0021, below).

`[[property]]`: `kind` is one of `len_range`, `regex`, `num_range`,
`json_has_keys`, `one_of`; `target` is `"output"` (the only target in v0);
remaining fields per §5. `len_range`/`num_range` bounds are optional on
either side (absent = unbounded); both are inclusive.

`[budgets]`: `max_latency_ms_p95`, `max_cost_usd_micros` (micro-usd:
1,000,000 = $1), `max_tokens`. All optional; absent means **not declared**.
Latency is measured by the harness itself. Cost and tokens are measured from
the **reserved span attrs** `cost_usd_micros` / `tokens` (spec/trace.md §3)
— the recording agent's own declaration of what its API billed; the harness
never fabricates either. Declared-but-unmeasurable budgets force an
Inconclusive verdict (§6).

`[acceptance]`: two optional keys. `differential_min_agreement_milli` — an
integer in **1..=1000**, thousandths (1000 = 100%; the integer-milli
convention of ADR-0014; `0` and anything above `1000` are rejected). Absent
table or absent key = **exact**: every replayed input must reproduce its
recorded output, the v0 behavior unchanged. When declared, it relaxes
**only** the differential reproduction claim — the compiled-vs-reference
replay accepts when `matched × 1000 ≥ milli × eligible` over the eligible
distinct inputs (pure integer math; zero eligible inputs is unchecked, never
a pass), and the artifact manifest records the **measured** agreement rate,
truncated, never rounded up. Everything else stays exact: examples,
properties, budgets, and interface conformance are not relaxed. Acceptance
is id-bearing (§8): contracts differing only in acceptance make different
normative claims and get different ids (an undeclared acceptance is omitted
from the canonical form, so pre-acceptance contract ids are unchanged).
Which witness a trainer learns from when a reference genuinely diverges
stays open (spec/adr/open-questions.md) — the gate accepts, the trainer
still needs a canonical pick. Rationale: ADR-0018.

`differential_match` — `"exact"` (the default) or `"judged"` (ADR-0021).
It relaxes **what counts as matched** in the differential above, nothing
else: under `"judged"`, a replayed group whose subject output differs
byte-wise from its reference is arbitrated by the ADR-0019 judge —
semantically equivalent counts as matched, not equivalent counts as
unmatched — and the declared `differential_min_agreement_milli` **still
decides** (it is required: `"judged"` without a declared threshold is
rejected at parse). Byte-equal groups pass free, without consulting the
judge; at most one judge call is made per byte-divergent distinct input;
for a group whose recorded outputs themselves diverge, the reference is the
ADR-0018 canonical pick (majority witness, lexicographic tie-break) and
every line over it says so. The judge is a paid frontier call riding the
ADR-0010 spend rails (capped client, append-only ledger). A judged
differential with **no judge supplied** is unchecked → the verdict is
Inconclusive, never Pass — it never silently falls back to exact counting,
even when every group is byte-equal; a judge failure fails the agreement
check outright. Every group that was judged says **JUDGED** in its evidence
line — never mistakable for byte reproduction — and the distinction
survives into the eval run record. `"judged"` is id-bearing (§8); declared
`"exact"` is the default made explicit and keeps the same id.

**Eval sets.** Each referenced file is JSONL: one JSON object per line, with
`input` (required, any JSON) and `expected` (optional; when present, an
exact-match expected output). Placement note: `eval_sets` is a **top-level**
key, and TOML assigns any key written after a `[table]` header to that
table — so `eval_sets` must appear before the first table header
(`budgets.eval_sets` would be an unknown key, rejected).

**Strictness.** Unknown keys are rejected everywhere — top level, every
table, every eval-set line. `contract_version` ≠ 0 is rejected. Unknown
`scope.type`, span `kind`, `match`, property `kind`, or `target` values are
rejected. No best-effort reads.

## 3. value type grammar

Interface types use the IR value type grammar (spec/ir.md §3), written
exactly as the IR renders them:

```
unit | bool | int | float | text | bytes | json | list<T>
```

`list<T>` nests (`list<list<int>>`). No other syntax, no whitespace
variants.

Contracts check **JSON values** (recorded span I/O, example values, eval
inputs) against these types. Conformance:

| type | a JSON value conforms iff |
|---|---|
| `unit` | it is `null` |
| `bool` | it is a boolean |
| `int` | it is a number that is integer-typed and i64-representable — `3.0` is **not** an int, `3` is; `1e20` is not |
| `float` | it is any number (integers conform: `3` is a valid float) |
| `text` | it is a string |
| `bytes` | it is a string, treated as opaque — the v0 carriage convention for byte payloads |
| `json` | anything conforms; the escape hatch |
| `list<T>` | it is an array and every element conforms to `T` |

## 4. property semantics

Every property is checked against its target on **every observation** (§6).
The rule for shape mismatches is uniform: **a property applied to a value of
the wrong shape FAILS** — it never skips. A `regex` meeting a number is a
violation, not a pass-by-absence; silent skips would let shape drift through
unchecked.

| kind | applies to | rule |
|---|---|---|
| `len_range` | text, list, bytes-string | length within `[min, max]` inclusive: chars (unicode scalar values) of text or bytes-string, elements of a list; absent bound = unbounded on that side |
| `regex` | text only | rust `regex` **search** semantics: the pattern must match *somewhere* in the value — anchor with `^…$` when a full match is meant |
| `num_range` | int/float numbers | value within `[min, max]` inclusive, compared as f64 — integers beyond 2⁵³ lose precision here (documented, accepted at v0) |
| `json_has_keys` | json objects | the value is an object containing **all** listed keys (values unconstrained) |
| `one_of` | any value | value equals one of the listed values under canonical-json equality — object key order irrelevant |

## 5. verification subjects

What a contract is checked *against*:

- **Trace stores (today).** The subject is a set of recorded runs
  (spec/trace.md §6). `scope = "span"` binds to every recorded span with the
  contract's kind + name across the task's traces; those spans are the
  **observations**. An example (or expected-output eval case) is
  **witnessed** iff some observation's recorded input equals the case input
  under canonical-json equality; its expected output is then compared to
  that observation's recorded output. Unwitnessed cases are unchecked — no
  claim is made (§7).
- **Executable artifacts (S3 on).** The harness runs the compiled subject on
  example and eval inputs directly, so every case is witnessed by
  construction. Same contract, same verdict semantics.

`scope = "task"` verifies against traces since ADR-0025: the observations
are the recorded whole-run input/output pairs — one per trace whose header
carries **both** `task_input` and a `task_output` declaration
(spec/trace.md §3). A trace recording only one of the two is counted as
partial and witnesses nothing; a store whose traces carry no task-level I/O
at all yields an Unchecked "task-level observations present" check
(Inconclusive, with a how-to-record detail) — no longer the pre-ADR-0025
loud error, and never a silent pass. Task-scope latency budgets measure the
recorded wall-clock from run start (header `started_at_ms`) to the output
declaration (`recorded_at_ms`), both stamped by the recorder's own clock;
cost/token budgets have no task-level declaration channel and stay
Unchecked. Task-scoped contracts also verify against executable subjects,
unchanged. Compilation is a different matter: `auto compile` and
`auto distill` still refuse task scope (emit is span- or region-scoped in
v0; task-scope synthesis remains recorded future work).

## 6. verdict semantics

Three-valued: **Pass | Fail | Inconclusive**. Fail wins over Inconclusive.

**Fail** iff anything actually checked was violated:

- a witnessed example (or expected-output eval case) whose observed output ≠
  expected output (`exact` = canonical-json equality);
- a witnessed `judged` example whose divergent output the judge rejects as
  not equivalent — or whose judge call itself fails (a judge failure never
  passes; §2);
- a property violated by any observation;
- an interface conformance violation (§3) on any observed input or output;
- a measured budget exceeded — the nearest-rank p95 over all observations:
  observed latencies vs `max_latency_ms_p95`; agent-declared
  `cost_usd_micros` / `tokens` attr values vs `max_cost_usd_micros` /
  `max_tokens` (measured only when every observation carries the attr —
  see below);
- a reserved budget attr whose value is not a decimal u64 — a malformed
  declaration is a loud failure, never silently ignored.

**Inconclusive** iff nothing was violated but a normative claim went
unchecked:

- an unwitnessed example, or an eval case with an expected output that was
  never witnessed;
- a witnessed `judged` example with a divergent output and no judge
  supplied (§2);
- zero observations;
- a declared budget the harness cannot measure. Cost and tokens are
  measurable only from the reserved span attrs `cost_usd_micros` / `tokens`
  (spec/trace.md §3), which the recording agent declares itself. The rule is
  **all-or-unchecked**: the budget is measured iff *every* observation
  carries the attr; if only some do, the check is unchecked with the partial
  count reported — partial data never passes. An agent that declared nothing
  leaves the budget unmeasurable, still forcing Inconclusive — the harness
  never fabricates billing. Executable subjects (§5) carry no billing
  declaration at all, so cost/token budgets against live subjects are always
  unchecked.

**Pass** otherwise: every declared claim was checked and none was violated.

Inconclusive is never rounded up to Pass, and nothing is extrapolated from
checked cases to unchecked ones. A Pass means exactly: everything this
contract declares was measured, and held.

## 7. eval runs

Every harness run writes an **eval run record**: a content-addressed JSON
document in a runs directory. The record's id is the sha-256 (lowercase hex)
of its canonical JSON body (sorted keys, compact separators) — the body
cannot contain its own id. Fields: the contract id (§8), a subject
description (what was checked, e.g. which trace store and task), a created
timestamp, per-check results (each example, property, conformance, and
budget check with its outcome), and the verdict. Two runs of the same
contract over the same subject are distinct records — the timestamp is part
of the body; a run is an event, not a value.

From S3 on, artifact manifests cite eval run ids: this record is what makes
"verified against the contract" a checkable claim rather than an assertion
(CLAUDE.md: never claim compiled parity without an eval run id attached).

## 8. contract identity

A contract's id is the lowercase-hex sha-256 of its **canonical JSON form**:
sorted object keys, compact separators, absent optionals omitted (never
serialized as `null`). Identity is computed over the parsed contract —
scope, interface, examples, properties, budgets, and the loaded eval cases —
not over the TOML bytes: reformatting or comments do not change the id;
any semantic change does, including a change to a referenced eval set.
Manifests and eval run records refer to contracts by this id.

## 9. versioning

- `contract_version` is read exact-match: this build accepts exactly **0**.
  Any change to fields or semantics of existing contracts bumps it, with an
  ADR. No silent best-effort reads.
- The property set is closed per version. New property kinds are a version
  bump; arbitrary predicates arrive only with sandboxed execution (S4+),
  never as unsandboxed code in a contract file.
- Format rationale and alternatives: `spec/adr/0003-contract-format.md`.

## 10. what this layer does not claim

A contract declares; only the harness measures. No verdict here implies
extractability, cost, or speed — those numbers live in determinism reports
(S1), eval run records (§7), and manifests (S7), each attached to what was
actually observed. A task-scoped contract is witnessable by traces only
where the recording chose to declare task-level I/O (ADR-0025); a verdict
over such traces claims nothing about compilability — emit still refuses
task scope.
