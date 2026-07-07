# Auto synthesis — symbolic extraction, v0

Status: v0, matches `crates/auto-dsl` and `crates/auto-passes` as merged.
DSL wire-format version: **0** (the `dsl_version` field). Where prose and
code disagree, the code wins; this document is written for external readers.

**Symbolic extraction** is the pass that turns recorded agent behavior into
a program. The constitution's claim is that most agent behavior is secretly
a parser — a deterministic transformation re-derived token-by-token on a
frontier model every run. This pass finds the parser: given the distinct
recorded observations of one deterministic span signature (canonical input →
canonical output, spec/trace.md), it searches a closed program space for a
program that reproduces every one of them, and hands the result to the same
emit gate every artifact must pass (spec/artifact.md §7). Where S3 allowed a
hand-written module, S4 synthesizes it.

## 1. inputs and outcomes

The synthesizer consumes **observations**: distinct recorded input→output
pairs of one span signature, canonicalized (canonical JSON, duplicates
collapsed). It returns exactly one of three honest outcomes:

- **Found** — a program that reproduces every observation, with provenance:
  distinct input count, states explored, depth reached;
- **BudgetExhausted** — no fitting program within the search budget.
  Exhaustion is an outcome, not an error; nothing is emitted;
- **ConflictingObservations** — the same input was recorded with different
  outputs. The signature is not deterministic; there is nothing to
  synthesize, and no search runs.

## 2. the DSL

The program space is the extraction DSL (`crates/auto-dsl`): a straight-line
pipeline of typed ops over a **single value register**, initialized to the
input. The DSL is **closed and total** — no arbitrary code, no I/O, no
randomness; every op either produces a value or fails typed. Values are JSON
values; an op meeting the wrong register shape is a typed failure, so a
candidate that fails on any observed input simply does not fit. Empty
programs are invalid.

| op | register in | register out |
|---|---|---|
| `get_field {key}` | object | the field's value (missing key fails) |
| `lowercase` / `uppercase` / `trim` | text | text |
| `split_whitespace` | text | list\<text> |
| `split_on {sep}` | text | list\<text> |
| `trim_each_matches {set}` | list\<text> | list\<text> (each entry stripped of `set`'s chars at both ends) |
| `filter_longer_than {n}` | list\<text> | list\<text> (keep entries with more than `n` chars) |
| `dedup_sort` | list\<text> | list\<text> (unique, ascending) |
| `take {n}` | list | list (first `n`) |
| `first` / `last` | list | element (empty list fails) |
| `join {sep}` | list\<text> | text |
| `count` | list | int |
| `char_count` | text | int (chars = unicode scalar values, as in `filter_longer_than`) |
| `add {k}` | int | int (checked: overflow fails; non-i64 numbers fail) |
| `const_out {value}` | anything | `value` |

**Wire form.** A program serializes as canonical JSON — sorted keys, compact
separators — under the enum's **externally tagged** form (serde's default):
unit ops are bare strings, ops with fields are single-key objects.

```json
{"dsl_version":0,"ops":[{"get_field":{"key":"prompt"}},"lowercase",
 "split_whitespace",{"trim_each_matches":{"set":".,"}},
 {"filter_longer_than":{"n":4}},"dedup_sort",{"take":{"n":3}},
 {"join":{"sep":" "}}]}
```

**Strict parse.** Readers reject: `dsl_version` ≠ 0; an empty `ops` array;
unknown keys at the top level; unknown op names; unknown fields **inside** an
op's variant (`deny_unknown_fields`, which is actually effective under
external tagging and silently ineffective under internal tagging — a serde
limitation, and part of why the wire form is external; ADR-0005). No
best-effort reads.

**No input-equality branching.** The DSL has no conditionals and no equality
test against the input, so a memo-table — `if input == x₁ then y₁, else if
input == x₂ then y₂, …` — is **inexpressible by construction**. A program
that fits N distinct observations is structurally forced to compute its
outputs, not replay them. The one deliberate exception is `const_out`, for
genuinely constant behavior; the search proposes it only under the condition
in §3.

## 3. the search

Bottom-up enumerative synthesis, shortest-first:

- **Enumeration.** Programs are enumerated by increasing length (depth 1,
  then 2, …). The first program that reproduces every observation wins, so
  results are minimal-length by construction.
- **Observational equivalence.** A candidate's identity is its **canonical
  value-vector**: the canonical-JSON outputs (or typed failures) it produces
  across *all* distinct observed inputs. Two candidates with equal vectors
  are the same state; only one representative is extended. This is the
  standard equivalence-class pruning of enumerative synthesis — the space
  collapses to behaviors, not syntax.
- **Parameter mining.** Parameterized ops draw their constants from the
  observations, not from an open universe: `get_field` keys from observed
  object keys, separators and trim sets from observed text, counts and
  offsets from observed list sizes and numeric deltas. What was never
  witnessed is never proposed.
- **Budgets.** The search is bounded by a maximum depth and a maximum number
  of explored states (defaults: depth 8, 300,000 states). Hitting either
  bound returns BudgetExhausted with the counts reached — honest exhaustion,
  never a hang and never a fabricated result.
- **`const_out`.** Proposed only through one path: a depth-1 candidate when
  **all** observed outputs are identical. It is never enumerated as a
  general op, so a constant program can only ever be the answer when the
  evidence is literally constant.
- **Determinism.** Fixed enumeration order, no randomness: the same
  observations under the same budget produce the same outcome, including the
  same program.

## 4. honesty — evidence-bounded generalization

What a synthesis result proves, exactly: **the program reproduces every
witnessed distinct observation.** Nothing more. Generalization to
unwitnessed inputs is a hypothesis backed by the DSL's structural bias (§2:
memo-tables are inexpressible), not a verified claim.

The boundary case is instructive: with **one** distinct input, constant
behavior is indistinguishable from computation — no evidence separates
"always answers `42`" from "computes `42` from this input" — and the search,
shortest-first, returns the constant. That is the correct reading of the
evidence, honestly stated. Confidence grows with distinct inputs, which is
why `distinct_inputs` travels in the synthesis provenance and into the
artifact's manifest notes.

Two further rules keep this honest:

- **The emit gate remains the arbiter.** A synthesized program is not
  trusted because it was synthesized: the candidate artifact still passes
  the full gate — differential replay over every distinct recorded input
  plus contract verification (spec/artifact.md §7) — and Fail or
  Inconclusive still blocks emit.
- **The v0 search is enumerative, not LLM-guided.** The constitution names
  LLM-guided CEGIS as the extraction pass; LLM proposal generation is the
  intended upgrade and requires authorized model spend (CLAUDE.md: hard cap,
  no paid runs beyond it). Nothing in this pass calls a model, and nothing
  pretends to.

## 5. artifact integration

A synthesized artifact is **program-as-data**: the container carries

- `program.json` — the program in the wire form of §2, and
- `module.wasm` — the **generic DSL interpreter**, auto-dsl's evaluator
  compiled to wasm (`crates/auto-passes/dsl-interpreter`, nested-built by
  `auto-passes`' build.rs and embedded in the crate).

**One implementation, two compilations.** The evaluator that scored every
candidate natively during the search and the interpreter inside the artifact
are the same code compiled twice. The differential gate executes the wasm
side on every distinct recorded input, so native/wasm drift shows up as a
gate failure — it cannot silently diverge.

At execution, the runtime feeds `program.json` to the module through the
**`init` ABI extension** (spec/artifact.md §4): one `init(ptr, len)` call
per instance, before `run`. A program without an `init` export, or an `init`
export without a program, is refused. The interpreter has zero imports; the
v0 purity rule is unchanged, and because `program.json` is canonical JSON,
equal programs are equal bytes and content addressing covers them.

## 6. versioning

`dsl_version` is read exact-match: this build accepts exactly **0**. Any
change to the op set or to an op's semantics bumps the version with an ADR —
the same policy as `manifest_version`, `contract_version`, and the IR schema
version. Rationale and alternatives: spec/adr/0005-symbolic-extraction.md.

## 7. LLM-guided proposals (CEGIS)

ADR-0005's recorded upgrade, now real: **proposal generation only**. A
frontier model — behind the spend-capped client (ADR-0010; fail-closed cap-0
default, append-only ledger) — is shown the closed DSL (§2) and the deduped
witnesses, and asked for candidate programs as bare JSON. The **checker is
unchanged**: every proposal runs through the same `auto_dsl::eval` the
artifact interprets (§5) against every witness; the first witness a
candidate gets wrong (value mismatch, typed eval failure, or parse failure)
is fed back as a counterexample the next round, up to a round budget.

Honesty properties, unchanged from §1/§4: conflicting observations refuse
before any paid call; zero observations refuse (a program verified against
nothing is vacuous); a spend-cap or key refusal mid-loop ends the run as a
refusal, never a fabricated program. **Proposal generation is
nondeterministic (model sampling); acceptance is not** — a found program is
evidence-bounded exactly as in §4, and the emit gate (differential replay +
contract) still decides whether it may exist. The model can suggest; it can
never admit.

## 8. regions — compiling a chain of spans (ADR-0015)

A **region** contract (`spec/contract.md`, scope `region`) binds a recorded
CHAIN of spans, `from`..`to` inclusive by seq order. Its interface is
(from-span input) → (to-span output), and **every arrow in the chain is its
own synthesis problem** over witnessed value pairs:

- each **stage** — a span's input → output;
- each **glue edge** — one span's output → the next span's input. Glue is
  the agent code between calls, which traces never record as code but
  always record as values. Glue whose witnessed pairs are all identical is
  identity and is **omitted** (the DSL has no identity program on purpose);
  anything else must synthesize, or the region refuses naming the exact
  edge that failed.

Structure rules, all loud: exactly one `from` and one `to` effectful span
per trace, `from` before `to`, unique names inside the window, and an
identical (kind, name) sequence across every recorded trace. Chain spans
may be `model_call` (synthesized stages) or `tool_call` (**declared
capability boundaries**, ADR-0017: the stage becomes a `tool_call` pipeline
stage, the tool name a manifest capability, and the artifact imports
exactly `auto.tool_call` — verified hermetically from recorded pairs, run
live via `--tool name=command`). env_read / memory_op / branch inside the
window still refuse.

The assembled artifact carries a **pipeline** payload,
`{"pipeline_version":0,"programs":[…]}` (strict parse, ≥ 1 programs, each a
full versioned program object; `auto-dsl::Pipeline`); the generic
interpreter accepts either payload form by version-key sniff and folds
programs left-to-right. One implementation, two compilations, unchanged
(§5). The emit gate is also unchanged: differential replay covers every
recorded **end-to-end chain**, examples and properties run against the
assembled pipeline, and the artifact's `graph.air` lowers one transform
node per stage. Each edge gets the full enumerative budget (§3) —
synthesis cost for a region is per-edge, and the manifest notes say so.
