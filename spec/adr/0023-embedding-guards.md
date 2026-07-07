# ADR-0023: guard wire v2 — dense lexical trigram-hash embeddings with cosine OOD distance, conformally calibrated, honestly named

status: accepted · scope: `crates/auto-runtime` (guard),
`spec/runtime.md` §2. Builds on ADR-0014 (calibration) and ADR-0007
(distance, fail-closed evaluation); changes neither for existing wire.

## context

The constitution names the guard design "embedding-distance OOD +
conformal prediction". ADR-0014 landed the conformal half and recorded the
embedding half as deferred, because "embedding" was read as a *model* — an
onnx encoder in the runtime, weights distributed with artifacts. That
dependency decision still has no owner, and the gates run with **no
network**, so nothing that must download weights can be the tested path.

Meanwhile v0/v1's distance is trigram-set Jaccard: binary membership over
hashed char trigrams. Two things about it are geometrically crude in ways
that need no model to fix: sets ignore repetition (a trigram appearing
once and fifty times weigh the same), and set overlap is a blunter
similarity than an inner product over weighted features. Feature hashing
(the "hashing trick") gives a dense, fixed-dimension, dependency-free
embedding of exactly the same lexical evidence — deterministic, no
vocabulary to ship, no floats that must round-trip a wire.

Requirements: deterministic across platforms (a guard decision must never
depend on where it runs); no f64 on the wire and no float comparisons in
the decision; the SAME split-conformal machinery as v1 (one quantile rule,
not two); v0/v1 artifacts byte-stable — read exactly as today, never
rewritten; v2 strictly opt-in; and the name must not lie: this is a
**lexical** geometry upgrade, and every artifact of the work says so.

## decision

Six coupled choices:

1. **Featurizer: signed byte-trigram feature hashing, frozen.** Slide a
   3-byte window over the guard-field string's raw utf-8 bytes; hash each
   window with FNV-1a 64 (offset 14695981039346656037, prime
   1099511628211); bucket = `hash % 256` (dim **256**, pinned for v1 of
   this method); sign = bit 32 of the hash, zero-indexed (set = +1, clear
   = −1); accumulate signed counts in i64 (exact); L2-normalize through
   f64 into an f32 vector. Order-independent (a bag of trigrams),
   dependency-free, deterministic: integer accumulation is exact and
   sqrt/divide/casts are IEEE correctly rounded. Byte trigrams, not v1's
   lowercased char trigrams — a different featurizer is a different wire
   version, never a silent reinterpretation of an old one. A doc under 3
   bytes, or a full signed cancellation, embeds to the zero vector (no
   fake normalization); zero-vector distance edges mirror v1's
   empty-sketch rules (both zero → 0.0, exactly one → 1.0).
2. **Score: min over witnesses of cosine distance, in fixed point.**
   Distance is `1 − dot` over unit vectors (range `[0, 2]`), the same
   nearest-witness min as v1's Jaccard. Every distance crosses f32 →
   integer exactly once, at one pinned boundary function: micros = round
   half up of `distance × 1e6`, clamped to `[0, 2_000_000]` (NaN — off
   the cosine path but the function is total — clamps to the max: fail
   toward a trip). The stored threshold is u32 micros and the decision
   compares u32: no f64 in the wire, no platform-dependent float
   comparison decides admission. Outcomes report `micros / 1e6` so what
   callers see is exactly what was compared, rescaled.
3. **Calibration: the SAME split-conformal rule as v1, shared not
   forked.** Leave-one-out scores (each witness's micros distance to its
   nearest other witness), threshold = the k-th smallest at
   `k = ceil((n+1)(1−alpha))`. The k computation is one shared function
   used by both the v1 f64 path and the v2 micros path; the k > n
   truncation to the max score and the lone-witness → 0 rule carry over
   unchanged. The exchangeability caveat carries over **verbatim**
   (ADR-0014): the ≥ 1−alpha pass rate is conditional on future inputs
   being exchangeable with the witnesses; OOD inputs are exactly the
   non-exchangeable case; no coverage bound exists for them and none is
   claimed. alpha remains a knob on the in-distribution deopt rate.
4. **Wire v2: raw docs, integer threshold, strict shape.**
   `{"guard_version":2,"field":…,"witnesses":[raw doc strings, sorted
   (byte order) + deduplicated],"embedding":{"method":
   "trigram_hash_cosine","dim":256,"threshold_distance_micros":u32,
   "calibration":{"method":"split_conformal","alpha_milli":u32,
   "scores_n":usize}}}` (canonical JSON, keys sorted). Witness **docs**
   travel, vectors are recomputed at build and at load — floats never
   serialize, so there is no float round-trip to drift and nothing
   platform-shaped in the artifact. `calibration` is v1's exact
   three-field object.
5. **Version rules: byte-stability is a hard invariant.** v0 and v1
   documents parse, evaluate, and re-serialize exactly as before v2
   existed — byte-identically, pinned by tests; they are never upgraded
   or rewritten. v2 is emitted only by the new opt-in constructor
   (`Guard::build_embedding`; CLI `--guard-embedding`);
   `build`/`build_conformal` still emit v1. Reading a v2 document with an
   unknown `embedding.method` or a `dim` other than 256 is a loud
   refusal, **never a silent fallback to Jaccard**; empty witnesses,
   non-canonical doc lists, out-of-range thresholds/alphas, and unknown
   fields all refuse. Fail-closed evaluation (wrong-shaped input trips
   with no distance) is unchanged.
6. **The honest name, everywhere.** Code docs, spec §2, and this ADR all
   state it the same way: **this is a lexical geometry upgrade, not
   semantic understanding**. The vector is built from the byte trigrams
   the text is spelled with, so a paraphrase with disjoint vocabulary
   still trips. What improves over Jaccard sets: repetition counts, and
   similarity is weighted trigram mass (an inner product) instead of
   binary set overlap. What does not improve: anything about meaning.

## alternatives considered

**Semantic embeddings — onnx MiniLM (or peer) in-process.** The full
reading of the constitution's "embedding-distance OOD", and it stays the
recorded upgrade, not this ADR: it needs an inference stack in the runtime
(ort/onnx or similar) and, harder, a distribution story for the encoder
weights — inside artifacts (tens of MB per `.cbin`, and the encoder
version becomes load-bearing provenance) or alongside them (a registry
story that does not exist). Gates run with no network, so a
download-on-first-use encoder cannot be the tested path. When the
distribution story exists, semantic embeddings arrive as a new
`embedding.method` — the v2 wire already makes an unknown method a loud
refusal on old readers, which is the versioning working as designed.

**Raw trigram-count vectors without hashing.** Cosine over exact count
vectors keyed by trigram — no collision noise. Rejected for the wire and
the dimension: the feature space is unbounded, so either the wire carries
a per-guard vocabulary (bigger than the docs it came from) or the
dimension floats per artifact. Feature hashing pins dim 256, adds no
dependency, and its collision behavior is well-characterized (signed
hashing keeps the hashed inner product an unbiased estimate of the true
one — Weinberger et al.). The raw docs on the wire mean a future method
change recomputes from source, losing nothing.

**Keeping Jaccard only.** Zero risk, and ADR-0014 already deferred
embeddings once — but that deferral was about the *model* dependency,
which the lexical rung does not have. Declining to build a
dependency-free, provably-inert-by-default upgrade would leave
"embedding-distance" meaning nothing in the tree for another wave with no
blocking reason. The opt-in flag keeps Jaccard the default; nothing
deployed changes behavior without an operator choosing it.

**f64 distances on the wire / in the decision.** Simpler code, and v0/v1
already carry one f64 threshold. Rejected for v2: cosine distances are
computed (not just carried), and comparing computed floats invites
platform-and-optimization-shaped admission flips at the boundary. One
rounding at one pinned function, u32 comparisons everywhere after —
admission is bit-identical everywhere.

## consequences

- Two guard geometries now exist and the wire declares which one gates an
  artifact; `guard.json` alone still audits the threshold, method, alpha,
  and score count. Readers older than this ADR reject v2 loudly by
  version — as designed.
- v0/v1 byte-stability is pinned by read-only re-emit tests; `Guard::build`
  callers (`auto-cli` compile/distill) are untouched and keep emitting v1.
  `auto run`, `auto serve`, and the resident runner load v2 through the
  same `guard.json` seam with zero code changes (proven by a runner test).
- Operators who opt in get repetition-weighted cosine geometry and the
  same calibrated-abstention contract; **nothing about semantic OOD
  improves**, and spec §2 says so in bold. No parity or deopt-rate claim
  is made here beyond what the tests measure; comparative
  Jaccard-vs-cosine trip behavior on real traffic is future measurement,
  not an assertion of this ADR.
- The zero-vector rule means all sub-3-byte inputs are mutually distance
  0 (as in v1's empty-sketch rule) — stated, not hidden; guards over
  fields that can be that short calibrate to thresholds that only admit
  other degenerate inputs.
- `EMBEDDING_DIM` (256) and bit-32 sign are frozen constants of method
  `trigram_hash_cosine`; changing either is a new method string, not a
  parameter.

## sources (retrieved)

- Weinberger, Dasgupta, Langford, Smola, Attenberg, "Feature Hashing for
  Large Scale Multitask Learning" (signed feature hashing; unbiased
  hashed inner products): <https://arxiv.org/abs/0902.2206>
- Fowler–Noll–Vo FNV-1a and its 64-bit parameters:
  <https://datatracker.ietf.org/doc/html/draft-eastlake-fnv>
- Angelopoulos & Bates, split-conformal quantile with the
  ceil((n+1)(1−alpha)) correction (via ADR-0014):
  <https://arxiv.org/abs/2107.07511>
- ADR-0007 (v0 guard), ADR-0014 (split-conformal calibration, embedding
  deferral); spec/runtime.md §2.
