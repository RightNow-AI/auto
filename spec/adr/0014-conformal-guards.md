# ADR-0014: guard calibration v1 — split-conformal quantile over leave-one-out scores, additive wire, honest coverage claim

status: accepted · scope: `crates/auto-runtime` (guard),
`spec/runtime.md` §2. Supersedes the calibration half of ADR-0007; distance,
evaluation, and deopt semantics unchanged.

## context

The constitution names guards first-class: "embedding-distance OOD +
conformal prediction (calibrated abstention)". ADR-0007 shipped the honest
floor — trigram-Jaccard nearest-witness distance with a leave-one-out-max
threshold — and recorded real conformal calibration as an upgrade, with the
spec stating plainly that leave-one-out max "is not conformal prediction:
no coverage guarantee". This ADR lands the conformal half over the existing
distance. Requirements: the threshold derivation must be the standard,
citable split-conformal quantile; the wire must carry the calibration
declaration (a threshold whose miscoverage level is unstated is an
unauditable number); v0 artifacts must keep parsing and evaluating
unchanged; small witness sets must behave exactly as v0 did (today's
artifacts have a handful of witnesses — a silent threshold shift there
would change deployed admission behavior with no one deciding it); and the
guarantee must be stated at exactly its true strength, no more.

## decision

Five coupled choices:

1. **Nonconformity score = the leave-one-out nearest-neighbor distance v0
   already computes.** For witnesses w_1..w_n, s_i = Jaccard distance from
   w_i to its nearest *other* witness. No new distance, no new
   featurization; the upgrade is confined to how the threshold is read off
   the scores.
2. **Threshold = the split-conformal quantile with the finite-sample
   correction.** The k-th smallest score, k = ceil((n+1)(1−alpha)),
   computed exactly on integer thousandths (k = ceil((n+1)(1000−alpha_milli)
   /1000)), clamped to [0, 1]. Two defined departures where the textbook
   quantile is undefined or unsafe: when k > n the textbook quantile is +∞
   (admit everything) — the guard truncates to the **maximum score**
   instead, which is exactly v0's leave-one-out max (threshold = max iff
   k ≥ n iff alpha_milli·(n+1) < 2000), so small-n behavior is unchanged
   and the truncation errs toward trips; a lone witness has no scores and
   calibrates to 0.0, v0's rule. Proven in tests both ways (production path
   vs an independent leave-one-out-max implementation) for n ≤ 9 at
   alpha 0.1, and on a 50-witness set with a fully known score distribution
   where the alpha 0.2 quantile sits strictly below the max.
3. **Wire v1, additive and strict.** `guard_version` 1 adds one object:
   `"calibration": {"alpha_milli": <u32>, "method": "split_conformal",
   "scores_n": <usize>}`. alpha travels as integer thousandths — no new
   floats in wire metadata beyond the threshold v0 already carries. Readers
   accept v0 (no calibration field allowed, semantics unchanged,
   byte-identical re-serialization — a v0 guard declares no alpha and none
   is invented for it) and v1 (strict: method exact, alpha_milli in
   (0, 1000) exclusive, scores_n consistent with the witness count, unknown
   fields rejected, explicit `null` rejected). Newly built guards always
   serialize as v1.
4. **Constructors: `build_conformal` added; `build` delegates at the
   max-quantile alpha.** `Guard::build_conformal(inputs, input_field,
   alpha_milli)` is the new surface (default choice `DEFAULT_ALPHA_MILLI`
   = 100). `Guard::build` keeps its signature and delegates at
   alpha_milli = 1, the most conservative expressible level: for every
   n ≤ 1998 that quantile IS the max leave-one-out score, bit-identical to
   the v0 threshold, so `auto-cli` and `auto-serve` compile and behave
   unchanged without edits; for n ≥ 1999 the quantile sits at or below the
   max — strictly more conservative, never less. Evaluation
   (`distance <= threshold` proceeds) is untouched.
5. **Embedding distance stays recorded, not built.** The other half of the
   constitution's named design needs an embedder in the loop — an inference
   stack in the runtime (ort/onnx or similar) and a story for distributing
   model weights with or alongside artifacts. That is a dependency decision
   with its own blast radius and gets its own ADR; conformal calibration
   does not wait for it.

**The honesty framing (load-bearing).** Split conformal's guarantee is
conditional on exchangeability: *if* a future input were exchangeable with
the witnesses, it would pass with probability ≥ 1−alpha. That conditional
is the entire claim. Arbitrary future inputs are **not** exchangeable with
the witnesses — OOD inputs are precisely the case where the assumption
fails — so no coverage bound exists for OOD traffic and none is claimed,
in spec §2, in code docs, or here. Tripping is the safe direction exactly
because of this asymmetry: a wrongly tripped in-distribution input costs
one deopt, while no statistical statement entitles an OOD input to
admission. alpha is therefore a knob on the expected in-distribution deopt
rate, not on correctness. One nuance stated rather than glossed: the
calibration scores are leave-one-out (each witness scored against n−1
neighbors) while a live input is scored against all n witnesses, which can
only shrink the live distance — the slack is in the passing direction, so
the ≥ 1−alpha reading survives.

## alternatives considered

**Full conformal prediction.** Recompute the score of every witness with
the test point included in the reference set, per input. Statistically the
cleanest (Vovk et al.), and O(n) extra distance computations per input is
affordable at today's n. Rejected for the wire, not the math: the threshold
would no longer be a compile-time constant carried in the artifact — it
becomes input-dependent, which breaks the manifest/guard model of "a
calibrated boundary, fixed at emit, auditable offline". Revisit if
witnesses grow enough that the split quantile's conservatism costs real
deopt volume.

**Jackknife+ (Barber, Candès, Ramdas, Tibshirani).** Designed for
regression prediction intervals built from leave-one-out *model* refits,
with a ≥ 1−2alpha guarantee. Our "model" (nearest-witness distance) is
refit-free, so jackknife+ collapses toward what v1 already does over LOO
scores, at a weaker stated level. Not taken.

**Keep leave-one-out max only.** Zero risk, zero claim — and it leaves
"calibrated abstention" meaning a max heuristic forever, with no declared
miscoverage level an operator can reason about or tune. The constitution
names conformal prediction; deferring it again needs a reason, and there
is none: the change is confined to threshold derivation and provably
inert at small n.

**Calibrate on a held-out split of the witnesses.** Textbook split
conformal separates fitting data from calibration data. With a handful of
witnesses, holding out a calibration set starves both halves; the LOO
construction uses every witness in both roles and its bias direction is
analyzed above (passing-direction slack only). Revisit alongside the
embedding upgrade, when scores come from a learned model that genuinely
needs held-out calibration.

## consequences

- The wire now declares its calibration: threshold, method, alpha, and
  score count are all auditable from `guard.json` alone. `scores_n` is
  redundant with the witness count by construction and checked on parse —
  a document that disagrees with itself is refused.
- v0 artifacts keep working unchanged; v0 fixtures are pinned in tests
  (parse, evaluate, byte-identical re-serialization). Old readers reject
  v1 guards loudly by version — the versioning working as designed.
- `Guard::build` callers (`auto-cli` compile/distill) emit v1 wire with
  thresholds bit-identical to v0 for n ≤ 1998. Consumers pinning guard
  *bytes* (none known in-repo) would see the new fields.
- Operators get a real knob: raising alpha tightens the threshold and
  raises the in-distribution deopt rate; nothing about OOD detection
  improves until the embedding half lands. Spec §2 says both.
- The alpha_milli integer wire caps expressible alphas at {0.001..0.999};
  build's delegation constant (alpha_milli 1) makes the LOO-max equivalence
  break at n ≥ 1999 — documented, more-conservative-only, and revisitable
  by then choosing alpha explicitly at compile time.

## sources (retrieved)

- Angelopoulos & Bates, "A Gentle Introduction to Conformal Prediction and
  Distribution-Free Uncertainty Quantification" — the split-conformal
  quantile with the ceil((n+1)(1−alpha)) finite-sample correction:
  <https://arxiv.org/abs/2107.07511>
- Vovk, Gammerman, Shafer, "Algorithmic Learning in a Random World"
  (Springer; full/transductive conformal prediction and the
  exchangeability assumption): <http://alrw.net/>
- Barber, Candès, Ramdas, Tibshirani, "Predictive inference with the
  jackknife+": <https://arxiv.org/abs/1905.02928>
- ADR-0007 (v0 guard: distance, leave-one-out max, fail-closed
  evaluation); spec/runtime.md §2.
