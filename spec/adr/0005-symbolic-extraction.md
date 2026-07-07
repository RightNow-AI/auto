# ADR-0005: symbolic extraction — enumerative synthesis over a closed DSL, program-as-data artifacts

status: accepted · scope: `crates/auto-dsl`, `crates/auto-passes`, `crates/auto-runtime`, `spec/synthesis.md`, `spec/artifact.md`

## context

S4 automates the symbolic extraction pass: given the distinct recorded
observations of a deterministic span signature, produce a program that
reproduces them, package it as an executable artifact, and let the existing
emit gate (differential replay + contract verification, ADR-0004) decide
whether it may exist. Requirements: results must be evidence-bounded and
honestly reported (a search that finds nothing says so; a constant fit on
one input is a constant, not a discovery); the search runs sandboxed with no
network; results must be deterministic (same observations, same budget, same
program); the thing verified must be the thing shipped — a native evaluator
that differs from the artifact's execution path would make the gate check
the wrong subject. The constitution names **LLM-guided CEGIS** as this pass,
and separately imposes a hard frontier-spend cap with owner authorization —
no authorized spend exists for an in-loop model today.

## decision

Five coupled choices:

1. **A closed, total extraction DSL** (`auto-dsl`): straight-line pipelines
   of typed ops over one value register. No arbitrary code, no I/O, no
   randomness, and — deliberately — no input-equality branching, so
   memo-tables (`if input == x then …`) are inexpressible and a fitting
   program is structurally forced to compute (`const_out` is the audited
   exception, proposed only when all observed outputs are identical).
2. **Externally tagged wire form**, `{"dsl_version":0,"ops":[…]}`, canonical
   JSON, strict parse. External tagging (serde's default) is where
   strictness is real: `deny_unknown_fields` rejects stray fields inside op
   variants there, and is silently ineffective under internal tagging — a
   documented serde limitation.
3. **Bottom-up enumerative search**: shortest-first enumeration with
   observational-equivalence pruning over canonical value-vectors, op
   parameters mined from the observations, explicit depth/state budgets,
   and three honest outcomes (found / budget exhausted / conflicting
   observations). Not LLM-guided; the CEGIS upgrade is recorded, below.
4. **Program-as-data artifacts**: the container carries `program.json` plus
   a generic interpreter module — auto-dsl's evaluator compiled to wasm —
   loaded per instance through an additive `init` ABI extension
   (spec/artifact.md §4). One implementation, two compilations: the search's
   native evaluator and the artifact's interpreter are the same code, so
   native/wasm drift surfaces as a differential-gate failure.
5. **The interpreter is nested-built from source** by `auto-passes`'
   build.rs (wasm32-unknown-unknown), never committed as a binary.

Synthesized artifacts pass the *same* emit gate as S3's hand-written module;
synthesis changed what proposes candidates, not what admits them.

## alternatives considered

**LLM-guided CEGIS.** The constitution's named design: a frontier model
proposes candidates, a checker verifies against observations, counterexamples
refine the prompt. Deferred, not rejected: it requires authorized frontier
spend (hard cap, owner auth — CLAUDE.md guardrails), and none is authorized
for an in-loop model today. Enumerative search fills the gate machinery
honestly meanwhile — the observation plumbing, the sandboxed evaluator, and
the emit gate are exactly the checker half CEGIS needs, so the upgrade path
is proposal generation only. Tradeoff accepted: enumeration caps reachable
program depth and op-set richness well below what guided proposals could
reach.

**SMT/sketch-based synthesis (z3 et al.).** Complete search over encoded
semantics, counterexamples for free. Rejected: a z3 dependency is heavy
weight for the core toolchain, and the target space is string-pipeline
shaped — split/trim/dedup/join over lists of text — where SMT string/sequence
theories are weak and frequently non-terminating; encoding the DSL's list
semantics is the hard problem, not the search. Enumeration with
observational-equivalence pruning handles this space directly.

**E-graph saturation (egg).** Excellent rewrite machinery. Rejected as the
synthesis engine: equality saturation transforms a *starting program* into
equivalent ones, and synthesis-from-examples has no starting program — it is
the wrong direction. Worth revisiting later as an optimizer over already-
synthesized programs.

**Per-program rust→wasm codegen.** Generate a rust crate per synthesized
program and compile it to a standalone module. Rejected for v0: a rustc
invocation per compile is slow (seconds to minutes) and brittle (temp
crates, toolchain state) on every emit, while data+interpreter is instant
per compile and byte-reproducible — the same program is the same
`program.json` bytes, and content addressing covers it. Revisit for hot
artifacts where interpreter overhead is measured to matter
(spec/adr/open-questions.md).

**Committing a prebuilt interpreter binary.** No nested build, no
wasm32 target requirement for contributors. Rejected: binaries in git bloat
history and — worse — drift from source: the committed module stops being
provably the compiled form of `auto-dsl`'s evaluator, which breaks the
one-implementation-two-compilations argument the differential gate rests on.
The chosen build.rs nested build costs build time and requires the
wasm32-unknown-unknown target (README; CI installs it), and keeps the
interpreter derivable from the sources in the tree.

## consequences

- Extractable behavior is capped by the DSL: what the op set cannot spell
  cannot be synthesized. Op-set growth is versioned (`dsl_version` bump +
  ADR) and tracked in open-questions (records, branching, regex).
- Enumerative scaling is real: states grow combinatorially with ops and
  mined parameters; budgets keep runs bounded and exhaustion honest. Deeper
  programs wait for guided proposal generation (the recorded upgrade).
- Evidence-boundedness is documented behavior: with one distinct input the
  search returns a constant, correctly (spec/synthesis.md §4); synthesis
  provenance (distinct inputs, states explored) travels in manifest `notes`
  until manifest v1 has fields for it.
- `auto-passes` now carries a nested wasm build; build time and a target
  prerequisite are the price of a source-derived interpreter.
- The `init` ABI extension is additive (optional export, checked both
  ways); S3 artifacts and their execution path are untouched.
- Interpretation costs per call vs. hypothetical codegen; acceptable for
  string-pipeline workloads at verification volumes, revisit on measurement.

## sources

- SyGuS — syntax-guided synthesis problem formulation and enumerative
  solvers: <https://sygus.org/>; Alur et al., "Syntax-Guided Synthesis",
  FMCAD 2013.
- Bottom-up enumeration with observational-equivalence pruning: Udupa et
  al., "TRANSIT: Specifying Protocols with Concolic Snippets", PLDI 2013;
  Albarghouthi et al., "Recursive Program Synthesis" (ESCHER), CAV 2013.
- serde enum representations (external tagging is the default):
  <https://serde.rs/enum-representations.html>; `deny_unknown_fields`
  container attribute: <https://serde.rs/container-attrs.html>; its
  internal-tagging ineffectiveness:
  <https://github.com/serde-rs/serde/issues/1600>.
- wasmtime 46.0.1 (typed function calls, `Instance::get_func` — the host
  side of the `init` extension):
  <https://docs.rs/wasmtime/46.0.1/wasmtime/struct.Instance.html#method.get_func>
