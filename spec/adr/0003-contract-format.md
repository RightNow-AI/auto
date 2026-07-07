# ADR-0003: contract format — TOML + closed properties + three-valued verdicts

status: accepted · scope: `crates/auto-contract`, `spec/contract.md`, `evals/`

## context

S2 needs a contract language: examples, properties, budgets, eval sets — the
type system of compiled cognition (CLAUDE.md: "the contract IS the type
system"). Requirements: contracts are **authored by hand** (a human writes
down what an agent must do, with comments explaining why); they are
**checked mechanically** against trace stores today and executable artifacts
from S3 on; verification must never execute contract-supplied code; verdicts
must be honest about what was and was not actually checked (honesty is
load-bearing — a failing eval blocks emit, an unchecked claim must not pass
silently); and manifests must be able to cite both contracts and eval runs
by stable ids.

## decision

Four coupled choices:

1. **TOML contract files** (`*.contract.toml`, toml crate 1.x), read
   strictly: unknown keys rejected everywhere, `contract_version` read
   exact-match. Bulk cases live in sibling JSONL files, not inline.
2. **A closed property set** — `len_range`, `regex` (rust `regex`, search
   semantics), `num_range`, `json_has_keys`, `one_of` — total,
   machine-checkable predicates only. A property applied to a wrong-shaped
   value fails, never skips.
3. **Three-valued verdicts** — Pass | Fail | Inconclusive. Fail iff anything
   checked was violated; Inconclusive iff nothing was violated but a
   normative claim went unchecked (unwitnessed example, zero observations,
   declared-but-unmeasurable budget); Pass only when every claim was checked
   and held.
4. **Content-addressed records**: contract id = sha-256 of the contract's
   canonical JSON; each harness run writes an eval run record whose id is
   the sha-256 of its canonical body. Manifests cite these ids from S3 on.

Details and exact semantics: `spec/contract.md`.

## alternatives considered

**JSON contract files.** Zero new parser surface (serde_json is already
load-bearing). Rejected: no comments — contracts are hand-authored normative
documents, and the first line a contract author writes is *why this example
is normative*; JSON also gets noisy fast for nested example values. JSON
stays as the value space; it loses as the authoring surface.

**YAML.** Comments plus terser nesting. Rejected: implicit-typing footguns
(the norway problem — unquoted `no`/`on`/`3.0` silently become
bool/bool/float in 1.1-lineage parsers) are exactly the class of silent
value mutation a contract format cannot tolerate, and a YAML parser is a
needless additional surface when TOML is already in the workspace
(Cargo.toml tooling).

**A custom DSL.** Maximal expressiveness, first-class syntax for properties.
Rejected at v0: a parser plus a language spec plus editor tooling is a
standing burden not justified by five property kinds and a handful of
fields. Revisit when properties outgrow the closed set — the DSL question
returns with the S4 sandbox (below).

**Arbitrary-code predicates (rhai/lua/wasm snippets in the contract).**
Maximal property power today. **Rejected on principle for v0:** a contract
file must not be able to execute arbitrary code at verify time — contracts
arrive from outside (registry, other teams) and verification runs
everywhere, so an executable contract is an injection surface pointed at
CI. Predicates beyond the closed set arrive only with the S4 sandbox
(wasmtime, no network — the confinement the constitution already mandates
for synthesis), never as unsandboxed code in a data file.

**Single-valued pass/fail verdicts.** Simpler to consume. Rejected: a binary
verdict forces unmeasurable-but-declared claims (cost/token budgets in v0,
unwitnessed examples against a trace store) to round to one side — rounding
to Fail makes honest declarations unusable, rounding to Pass fabricates a
guarantee, violating the honesty norm. Inconclusive is the verdict that
keeps declarations from becoming lies.

## consequences

- Property expressiveness is deliberately narrow; real behavioral richness
  lives in examples and eval sets until sandboxed predicates exist. Some
  contracts will be Inconclusive-by-construction (task scope, cost budgets)
  until S3+ closes the measurement gaps — visible, tracked in
  open-questions, not papered over.
- Contracts are content-addressed, so manifests (S7) can cite the exact
  contract an artifact was verified against; a semantic edit changes the id
  and invalidates nothing silently.
- Three-valued verdicts force coverage to be visible: a Pass carries the
  claim "everything declared was checked", and the eval run record is the
  evidence.
- One more strict-parse surface to version (`contract_version`,
  exact-match), same policy as the IR schema version and the trace `v`
  field.

## sources

- `toml` crate 1.1.2: <https://crates.io/crates/toml>
- `regex` crate 1.12.4: <https://crates.io/crates/regex> — search-semantics
  `is_match`, anchors required for full-match.
- TOML v1.1.0 spec: <https://toml.io/>
- YAML 1.1 implicit boolean resolution (the norway problem):
  <https://yaml.org/type/bool.html>
