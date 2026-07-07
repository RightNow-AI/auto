# ADR-0007: runtime tiering — trigram-Jaccard guards, leave-one-out calibration, pluggable tier-0, ingest-and-recompile ratchet

status: accepted · scope: `crates/auto-runtime` (guard),
`crates/auto-model` (`trigram_hashes`), `crates/auto-cli` (`run --tier0
--store`, `compile --guard-field`), `spec/runtime.md`

## context

S6 makes the runtime tiered: a compiled entry must decide, per input,
compiled path vs interpreter. Requirements: a wrong "stay compiled" decision
is a silent correctness failure, so guards are a first-class component,
never an afterthought (CLAUDE.md); the decision must be calibrated from
evidence that exists at compile time — the witnessed inputs the emit gate
verified — with no new heavyweight dependencies and no network; a trip must
have somewhere to go (tier-0) and must produce reusable evidence (the
ratchet: novel solves recompile down); and tier-0 cannot be a frontier model
yet, because the frontier binding requires API access under the spend cap
and no cap plumbing or authorized spend exists. The guard must fail closed:
an input it cannot measure must never pass by default.

## decision

Five coupled choices:

1. **Guard = trigram-set Jaccard nearest-witness distance.** An input's text
   is sketched as the set of its fnv1a-32 char-trigram hashes
   (`auto_model::trigram_hashes` — the same frozen trigram rule distillation
   already ships, unbucketed, set semantics); distance is Jaccard set
   distance to the nearest witness sketch. Zero new dependencies, zero
   weights, deterministic, and honest about being lexical.
2. **Threshold = leave-one-out calibration.** With n ≥ 2 witnesses: the max
   over witnesses of the distance to their nearest other witness — each
   witness would pass a guard built from the others. One witness calibrates
   to 0.0: only trigram-identical inputs proceed.
3. **Total evaluation, fail-closed.** A wrong-shaped input (no text where
   text is required) trips with no distance; a hand-built guard with no
   witnesses trips; nothing proceeds unguarded. Strict wire parse
   (`guard_version` 0, one named kind, canonical sketches, threshold in
   [0, 1]).
4. **Tier-0 = a pluggable command contract.** One command string, split on
   whitespace; the canonical input JSON as the final argument; output JSON
   on stdout, exit 0; the answer conformance-checked against the manifest's
   declared output type. The frontier binding later slots in behind the same
   contract.
5. **Deopt ingestion + recompile = the ratchet.** A tier-0 answer is
   recorded as a synthetic single-span trace (SDK label `auto-cli-deopt`,
   scope from the manifest) into `--store`; `auto compile` over the grown
   store folds it into synthesis evidence *and* the guard's witnesses.
   Recompilation is manual in v0; abstention (trip, no tier-0) is exit 3
   with no override flag.

## alternatives considered

**Embedding-distance OOD.** The constitution's named design (guards =
embedding-distance OOD + conformal prediction). Deferred, not rejected: an
embedding space needs a model in the loop — either weights shipped with
every artifact (size, and a second inference stack in the runtime) or a
paid API call per guarded input (spend, and the cap machinery does not
exist). The trigram sketch is the degenerate, weightless case of the same
shape — sketch, distance, threshold — so the upgrade is a new guard `kind`
under a `guard_version` bump, not a redesign. Tradeoff accepted and
documented: lexical distance cannot see semantics (spec/runtime.md §2).

**Real conformal prediction.** Distribution-free coverage guarantees are
exactly what "calibrated abstention" should eventually mean. Deferred: a
proper conformal guard needs a calibration set meaningfully larger than a
handful of witnesses, a designed nonconformity score, and a declared
coverage level (a contract-format concern). Leave-one-out max over the
witnesses is the degenerate v0 — same ingredients, no coverage claim — and
v0 never calls itself conformal. Known consequence, stated in the spec: a
witness with no trigram overlap with any other calibrates the threshold to
1.0, and that guard admits everything.

**Always-tier-0 shadowing.** Run the interpreter alongside (or instead of)
the compiled path and compare — the strongest possible guard. Rejected as
the admission mechanism: it pays full interpretation cost on every call,
which deletes the very economics the compiled path exists for. Worth
revisiting as *sampled* auditing on a budget, not as the gate.

**No guard (trust the emit gate).** The gate proved witnessed behavior
exactly; unwitnessed inputs were never evidence. Compiled answers beyond the
witness neighborhood would be silent correctness failures — the constitution
forbids this outright, so it was never really an alternative.

## consequences

- Deopts are lexically conservative: paraphrases and fresh vocabulary trip,
  costing a tier-0 call (or an abstention), never a wrong compiled answer.
  The failure the guard cannot catch is in-vocabulary semantic drift; the
  spec says so plainly.
- Disjoint witness sets calibrate to threshold 1.0 (guard admits
  everything); operators need witnesses with neighbors — the toy e2e records
  near-variant documents for exactly this reason.
- Guards apply to one text field; structured and multi-field inputs are
  future work. `compile` guards opt-in (`--guard-field`); `distill` reuses
  its `--input-field` when given.
- The ratchet is manual: deopt ingests, a human recompiles. The
  auto-recompile daemon and incremental resynthesis are open questions.
- Deopt captures the I/O observation, not a full tier-0 execution trace —
  the v0 floor of the constitution's trace-capture-on-deopt, honestly
  labeled.
- Exit 3 becomes ABI: scripts distinguish abstention from failure.

## sources

- Angelopoulos & Bates, "A Gentle Introduction to Conformal Prediction and
  Distribution-Free Uncertainty Quantification":
  <https://arxiv.org/abs/2107.07511>
- Yang et al., "Generalized Out-of-Distribution Detection: A Survey":
  <https://arxiv.org/abs/2110.11334>
- Jaccard index (set similarity/distance):
  <https://en.wikipedia.org/wiki/Jaccard_index>
- FNV-1a constants and the frozen trigram featurization: ADR-0006,
  spec/distillation.md §2 (the guard reuses them unbucketed).
