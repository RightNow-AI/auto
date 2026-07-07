//! The runtime guard: nearest-witness OOD distance with split-conformal
//! calibration (wire v1 Jaccard, wire v2 embedding; wire v0 read-compatible).
//!
//! Before tier-1 code runs on an input, the guard measures how far that
//! input's text sits from the witnesses — the verified inputs the artifact
//! was compiled against. Within calibrated distance: proceed on the compiled
//! path. Beyond it, or no text to measure at all: trip. A trip is the
//! runtime's deopt-to-tier-0 signal, never an error, and a wrong-shaped
//! input always trips — nothing proceeds unguarded.
//!
//! The distance is deliberately crude and claims nothing more. v0/v1: a
//! text is sketched as the set of its char-trigram hashes
//! ([`auto_model::trigram_hashes`]), distance is Jaccard set distance to the
//! nearest witness. What v1 changes is how the threshold is derived: a
//! split-conformal quantile over the witnesses' leave-one-out
//! nearest-neighbor distances at a declared nominal miscoverage rate alpha
//! ([`Guard::build_conformal`]). The guarantee that buys is conditional and
//! stated precisely in spec/runtime.md §2 and ADR-0014: **if** future
//! inputs were exchangeable with the witnesses, in-distribution inputs
//! would pass with probability >= 1-alpha; OOD inputs are exactly the
//! non-exchangeable case, and they fail toward a trip.
//!
//! Wire v2 (ADR-0023, [`Guard::build_embedding`], opt-in) upgrades the
//! *geometry*, not the meaning: each text embeds as a dense
//! [`EMBEDDING_DIM`]-dimensional L2-normalized vector of signed, hashed
//! byte-trigram counts ([`trigram_embedding`]), distance is cosine distance
//! to the nearest witness, compared in fixed-point u32 micros
//! ([`distance_micros`]), calibrated by the SAME split-conformal quantile
//! rule as v1 ([`conformal_k`], shared, not forked). **This is a lexical
//! embedding, not semantic understanding**: the vector is built from the
//! same byte trigrams, so a paraphrase with disjoint vocabulary still trips.
//! Semantic embeddings (an in-process onnx encoder) stay a recorded upgrade
//! — they need a model-distribution story and there is no network in gates.
//!
//! Wire form: canonical JSON, carried as the artifact's `guard.json` entry
//! ([`auto_backend::container::GUARD_ENTRY`]), strict-parsed on load.
//! Newly built guards serialize as `guard_version` 1
//! ([`Guard::build`] / [`Guard::build_conformal`]) or 2 (only
//! [`Guard::build_embedding`], opt-in); readers accept 0 (leave-one-out-max
//! calibration, ADR-0007), 1, and 2. v0 and v1 documents parse, evaluate,
//! and re-serialize exactly as before v2 existed — byte-identically, never
//! silently upgraded. A v2 document with an unknown method or a dim other
//! than [`EMBEDDING_DIM`] is refused loudly, never read as Jaccard.

use auto_trace::model::canonical_json;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Guard wire-format version written for newly built Jaccard guards
/// ([`Guard::build`] / [`Guard::build_conformal`]); readers accept 0, 1,
/// and [`EMBEDDING_GUARD_VERSION`]. Bump with an ADR (v1: ADR-0014).
pub const GUARD_VERSION: u32 = 1;

/// Guard wire-format version written by [`Guard::build_embedding`] only —
/// embedding guards are opt-in and never the silent default (ADR-0023).
pub const EMBEDDING_GUARD_VERSION: u32 = 2;

/// The only guard kind this build writes or reads on wire v0/v1.
const KIND: &str = "trigram_jaccard_nn";

/// The only v1/v2 calibration method this build writes or reads.
const METHOD_SPLIT_CONFORMAL: &str = "split_conformal";

/// The only v2 embedding method this build writes or reads. An unknown
/// method is a loud refusal, never a silent fallback to Jaccard.
const METHOD_TRIGRAM_HASH_COSINE: &str = "trigram_hash_cosine";

/// Embedding dimension, pinned for v1 of the `trigram_hash_cosine` method
/// (ADR-0023). A v2 document declaring any other dim is refused.
pub const EMBEDDING_DIM: usize = 256;

/// Cosine-distance ceiling in micros: unit vectors bound `1 - dot` to
/// `[0, 2]`, so micros live in `[0, 2_000_000]`.
const MAX_DISTANCE_MICROS: u32 = 2_000_000;

/// Default nominal miscoverage rate in thousandths (alpha = 0.1) for
/// callers of [`Guard::build_conformal`] with no reason to choose otherwise.
pub const DEFAULT_ALPHA_MILLI: u32 = 100;

/// The alpha [`Guard::build`] delegates at: the smallest expressible
/// miscoverage. Its conformal quantile is the maximum leave-one-out score
/// for every witness count n <= 1998 (see [`conformal_threshold`]) — exactly
/// the v0 leave-one-out-max threshold.
const MAX_QUANTILE_ALPHA_MILLI: u32 = 1;

/// How a guard's threshold was derived. Recorded on the wire (v1) or
/// implied by it (v0). Calibration never changes how [`Guard::evaluate`]
/// decides — `distance <= threshold` proceeds either way.
#[derive(Debug, Clone, PartialEq)]
pub enum Calibration {
    /// Wire v0 (ADR-0007): the threshold is the maximum over witnesses of
    /// the distance to their nearest other witness. No declared miscoverage
    /// rate. Read-compatible; never written for newly built guards.
    LeaveOneOutMax,
    /// Wire v1 (ADR-0014): the threshold is the split-conformal quantile of
    /// the witnesses' leave-one-out nearest-neighbor distances at nominal
    /// miscoverage `alpha_milli`/1000.
    SplitConformal {
        /// Nominal miscoverage rate in thousandths; in (0, 1000) exclusive.
        alpha_milli: u32,
    },
}

/// A calibrated tier-1 admission guard: witnesses plus the calibrated
/// threshold. Built by [`Guard::build`] / [`Guard::build_conformal`]
/// (Jaccard, wire v0/v1) or [`Guard::build_embedding`] (cosine, wire v2) at
/// compile time, carried in the artifact as `guard.json`, evaluated per
/// input at run time.
#[derive(Debug, Clone, PartialEq)]
pub struct Guard {
    /// Object field the guarded text lives in; `None` means the input value
    /// itself must be the text.
    pub input_field: Option<String>,
    /// One trigram-hash sketch per witness — sorted, deduplicated, possibly
    /// empty (a witness text under 3 chars has no trigrams). Empty (as a
    /// list) for a v2 embedding guard: its witnesses are the raw docs in
    /// [`Guard::embedding`].
    pub witnesses: Vec<Vec<u32>>,
    /// Trip boundary: nearest-witness distance <= threshold proceeds,
    /// anything greater trips. Derived per [`Calibration`]. For v0/v1 a
    /// Jaccard distance in `[0, 1]`; for a v2 guard this is the derived
    /// mirror `threshold_distance_micros / 1e6` in `[0, 2]`, kept only so
    /// outcomes report the boundary — the v2 decision itself compares u32
    /// micros, never floats.
    pub threshold: f64,
    /// How `threshold` was derived (metadata; evaluation ignores it).
    pub calibration: Calibration,
    /// Wire-v2 embedding payload (ADR-0023); `None` for v0/v1 guards. When
    /// present it is authoritative: evaluation scores cosine distance in
    /// micros against its docs and serialization emits wire v2.
    pub embedding: Option<EmbeddingGuard>,
}

/// The v2 embedding payload: raw witness docs (the wire form) plus their
/// recomputed unit vectors and the fixed-point trip boundary. Fields are
/// private so the canonical invariants (docs sorted + deduplicated, vectors
/// = [`trigram_embedding`] of the docs, alpha in range) cannot be
/// hand-broken; construct via [`Guard::build_embedding`] or
/// [`Guard::from_json`].
#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddingGuard {
    /// Raw witness docs, sorted and deduplicated — exactly the wire
    /// `witnesses` array. Vectors are recomputed from these at build and at
    /// load, so no floats ever cross the wire.
    docs: Vec<String>,
    /// One [`trigram_embedding`] vector per doc, same order (derived, never
    /// serialized).
    vectors: Vec<Vec<f32>>,
    /// Trip boundary in cosine-distance micros: nearest-witness distance
    /// (micros) <= this proceeds, anything greater trips.
    threshold_distance_micros: u32,
    /// Nominal miscoverage rate in thousandths; in (0, 1000) exclusive.
    alpha_milli: u32,
}

impl EmbeddingGuard {
    /// The raw witness docs, sorted and deduplicated.
    pub fn docs(&self) -> &[String] {
        &self.docs
    }

    /// The trip boundary in cosine-distance micros.
    pub fn threshold_distance_micros(&self) -> u32 {
        self.threshold_distance_micros
    }

    /// The declared nominal miscoverage rate in thousandths.
    pub fn alpha_milli(&self) -> u32 {
        self.alpha_milli
    }

    /// Fixed-point nearest-witness score: the minimum over witness vectors
    /// of the cosine distance to `text`'s embedding, in u32 micros — the
    /// same min-over-witnesses shape as the v1 Jaccard score. No docs (not
    /// constructible through the public API, but stated) scores the maximum
    /// distance: fail toward a trip.
    fn distance_micros_to_nearest(&self, text: &str) -> u32 {
        let embedded = trigram_embedding(text);
        self.vectors
            .iter()
            .map(|witness| distance_micros(cosine_distance(&embedded, witness)))
            .min()
            .unwrap_or(MAX_DISTANCE_MICROS)
    }
}

/// Build- or parse-time failure. Evaluation never fails — a bad input at
/// run time trips the guard instead.
#[derive(Debug, thiserror::Error)]
pub enum GuardError {
    /// Not a valid guard document: JSON syntax, missing or unknown fields,
    /// wrong types, a threshold outside [0, 1], or a malformed v1
    /// `calibration` object.
    #[error("invalid guard json: {0}")]
    BadJson(String),
    #[error("unsupported guard_version {found}; this build reads 0, 1, and 2")]
    UnsupportedVersion { found: u32 },
    #[error("unknown guard kind `{0}`; this build reads exactly `trigram_jaccard_nn`")]
    UnknownKind(String),
    /// No witnesses to calibrate against (empty build inputs or an empty
    /// `witnesses` array on parse).
    #[error("guard has no witnesses; calibration needs at least one")]
    NoWitnesses,
    /// A witness input did not yield text at build time.
    #[error("witness input did not yield text: {detail}")]
    NotText { detail: String },
    /// A parsed witness is not canonical (sorted + deduplicated): a v0/v1
    /// hash sketch, or the v2 raw-doc list itself.
    #[error("non-canonical witness sketch: {detail}")]
    BadWitness { detail: String },
    /// `alpha_milli` outside (0, 1000) exclusive, at build or parse time.
    #[error(
        "alpha_milli {0} is not in (0, 1000); alpha is the nominal miscoverage rate in thousandths"
    )]
    BadAlpha(u32),
}

/// The guard's verdict on one input. `Trip` is the deopt-to-tier-0 signal,
/// not an error.
#[derive(Debug, Clone, PartialEq)]
pub enum GuardOutcome {
    /// Within calibrated distance of the nearest witness: tier-1 may run.
    Proceed { distance: f64, threshold: f64 },
    /// Beyond calibration, or the input had no text to measure
    /// (`distance: None`): deopt to tier-0.
    Trip {
        reason: String,
        distance: Option<f64>,
        threshold: f64,
    },
}

/// Wire form of `guard.json`. Serialized through [`serde_json::Value`], so
/// keys come out sorted; parsed strictly (`deny_unknown_fields`).
///
/// `calibration` is double-`Option` (with [`present_calibration`]) to
/// distinguish absent (valid v0) from an explicit `null` (never valid — v0
/// rejects the field, v1 requires the object).
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    #[serde(
        default,
        deserialize_with = "present_calibration",
        skip_serializing_if = "Option::is_none"
    )]
    calibration: Option<Option<CalibrationWire>>,
    guard_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    input_field: Option<String>,
    kind: String,
    threshold: f64,
    witnesses: Vec<Vec<u32>>,
}

/// v1/v2 calibration metadata. Strict shape: exactly these three fields.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CalibrationWire {
    alpha_milli: u32,
    method: String,
    scores_n: usize,
}

/// Wire form of a v2 `guard.json` (ADR-0023). Serialized through
/// [`serde_json::Value`], so keys come out sorted; parsed strictly. The
/// witnesses are the RAW doc strings — vectors are recomputed at load, so
/// no float ever crosses the wire.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireV2 {
    embedding: EmbeddingWire,
    #[serde(skip_serializing_if = "Option::is_none")]
    field: Option<String>,
    guard_version: u32,
    witnesses: Vec<String>,
}

/// v2 embedding metadata. Strict shape: exactly these four fields;
/// `calibration` is required (serde rejects a missing or `null` object).
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmbeddingWire {
    calibration: CalibrationWire,
    dim: u32,
    method: String,
    threshold_distance_micros: u32,
}

/// Marks a present `calibration` field as `Some(..)` even when its value is
/// `null` — plain serde would collapse a present `null` into the same
/// `None` as an absent field, silently loosening the strict parse.
fn present_calibration<'de, D>(de: D) -> Result<Option<Option<CalibrationWire>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Some(Option::<CalibrationWire>::deserialize(de)?))
}

/// Human name of a JSON value's type, for refusal details.
fn json_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a bool",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// The guarded text of `input`, or a plain-words detail of why there is
/// none. `input_field: None` requires the input itself to be a string;
/// `Some(f)` requires an object whose field `f` is a string.
fn extract_text<'a>(input: &'a Value, input_field: Option<&str>) -> Result<&'a str, String> {
    match input_field {
        None => input
            .as_str()
            .ok_or_else(|| format!("input is {}, not a string", json_type(input))),
        Some(field) => {
            let object = input
                .as_object()
                .ok_or_else(|| format!("input is {}, not an object", json_type(input)))?;
            let value = object
                .get(field)
                .ok_or_else(|| format!("input object has no field `{field}`"))?;
            value.as_str().ok_or_else(|| {
                format!(
                    "input field `{field}` is {}, not a string",
                    json_type(value)
                )
            })
        }
    }
}

/// Jaccard set distance `1 - |A ∩ B| / |A ∪ B|` over two sorted, deduped
/// hash sets. Both empty => 0.0 (identical, if vacuous, sketches); exactly
/// one empty => 1.0 (nothing shared).
fn jaccard_distance(a: &[u32], b: &[u32]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let mut inter = 0usize;
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                inter += 1;
                i += 1;
                j += 1;
            }
        }
    }
    let union = a.len() + b.len() - inter;
    1.0 - (inter as f64) / (union as f64)
}

/// FNV-1a 64-bit over bytes (offset 14695981039346656037, prime
/// 1099511628211). Frozen for the v2 featurizer: a drift here silently
/// reshuffles every embedding bucket and sign.
pub fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The frozen v2 featurizer (ADR-0023): dense lexical embedding of a text.
///
/// Slide a 3-byte window over the text's raw utf-8 bytes (byte trigrams —
/// deliberately not v1's lowercased char trigrams; a different featurizer
/// is a different wire version, never a silent reinterpretation). Hash each
/// trigram with [`fnv1a_64`]; bucket = `hash % 256` ([`EMBEDDING_DIM`]);
/// sign = bit 32 of the hash, zero-indexed (`(hash >> 32) & 1`: set = +1,
/// clear = -1); accumulate signed counts; L2-normalize to f32. Counts
/// accumulate in i64 (exact) and normalize through f64 (sqrt and divide are
/// IEEE correctly rounded), so the result is deterministic across platforms
/// and independent of trigram order — the same bag of trigrams always
/// embeds identically.
///
/// A text under 3 bytes has no trigrams; it embeds — as does a full signed
/// cancellation, the same "no measurable direction" — to the zero vector,
/// never fake-normalized. **Lexical, not semantic**: the vector is built
/// from the same byte trigrams the text is spelled with, so a paraphrase in
/// disjoint vocabulary lands far away by construction.
pub fn trigram_embedding(text: &str) -> Vec<f32> {
    let mut counts = [0i64; EMBEDDING_DIM];
    let bytes = text.as_bytes();
    for window in bytes.windows(3) {
        let hash = fnv1a_64(window);
        let bucket = (hash % EMBEDDING_DIM as u64) as usize;
        if (hash >> 32) & 1 == 1 {
            counts[bucket] += 1;
        } else {
            counts[bucket] -= 1;
        }
    }
    let norm_squared: f64 = counts.iter().map(|&c| (c as f64) * (c as f64)).sum();
    if norm_squared == 0.0 {
        return vec![0.0; EMBEDDING_DIM];
    }
    let norm = norm_squared.sqrt();
    counts.iter().map(|&c| ((c as f64) / norm) as f32).collect()
}

/// Cosine distance `1 - dot` over two [`trigram_embedding`] vectors
/// (already unit-norm, so the dot IS the cosine; range `[0, 2]`). The dot
/// accumulates in f64 in fixed index order — deterministic. Edge case
/// pinned to mirror v1's empty-sketch rule: two zero vectors (both texts
/// trigramless) are distance 0.0 — identical, if vacuous; one zero vector
/// falls out of the formula at exactly 1.0 (dot 0, nothing shared).
fn cosine_distance(a: &[f32], b: &[f32]) -> f32 {
    let a_zero = a.iter().all(|&x| x == 0.0);
    let b_zero = b.iter().all(|&x| x == 0.0);
    if a_zero && b_zero {
        return 0.0;
    }
    let mut dot = 0.0f64;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += f64::from(*x) * f64::from(*y);
    }
    (1.0 - dot) as f32
}

/// THE fixed-point boundary (ADR-0023): f32 cosine distance -> u32 micros,
/// round half up, clamped to `[0, 2_000_000]`. Every v2 distance crosses
/// f32 -> integer exactly once, here, so comparisons happen on u32 — no
/// f64 in the wire, no platform-dependent float comparisons in the
/// decision. The arithmetic is exact-then-correctly-rounded: f32 -> f64 is
/// exact, `* 1e6` on a 24-bit mantissa fits f64 exactly, `+ 0.5` and
/// `floor` are IEEE-defined. NaN (impossible from [`cosine_distance`],
/// but the function is total) clamps to the maximum: fail toward a trip.
pub fn distance_micros(distance: f32) -> u32 {
    if distance.is_nan() {
        return MAX_DISTANCE_MICROS;
    }
    let scaled = (f64::from(distance) * 1_000_000.0 + 0.5).floor();
    if scaled <= 0.0 {
        0
    } else if scaled >= f64::from(MAX_DISTANCE_MICROS) {
        MAX_DISTANCE_MICROS
    } else {
        // in (0, 2_000_000): exact integer-valued f64, cast is lossless
        scaled as u32
    }
}

/// The witnesses' leave-one-out nonconformity scores in cosine-distance
/// micros: for each witness vector, the distance to its nearest OTHER
/// witness — the v2 parallel of [`loo_scores`], already fixed-point. Empty
/// for fewer than two witnesses.
fn embedding_loo_scores(vectors: &[Vec<f32>]) -> Vec<u32> {
    if vectors.len() < 2 {
        return Vec::new();
    }
    vectors
        .iter()
        .enumerate()
        .map(|(i, vector)| {
            vectors
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, other)| distance_micros(cosine_distance(vector, other)))
                .min()
                .expect("n >= 2 leaves at least one other witness")
        })
        .collect()
}

/// The witnesses' leave-one-out nonconformity scores: for each witness, the
/// Jaccard distance to its nearest OTHER witness. Empty for fewer than two
/// witnesses — a lone witness has no other witness to score against.
fn loo_scores(witnesses: &[Vec<u32>]) -> Vec<f64> {
    if witnesses.len() < 2 {
        return Vec::new();
    }
    witnesses
        .iter()
        .enumerate()
        .map(|(i, witness)| {
            let mut nearest = f64::INFINITY;
            for (j, other) in witnesses.iter().enumerate() {
                if i != j {
                    nearest = nearest.min(jaccard_distance(witness, other));
                }
            }
            nearest
        })
        .collect()
}

/// The split-conformal threshold over nonconformity scores at nominal
/// miscoverage `alpha_milli`/1000.
///
/// The standard split-conformal quantile with the finite-sample correction
/// (Angelopoulos & Bates, arXiv:2107.07511; ADR-0014): the k-th smallest of
/// the n scores, k = ceil((n+1)(1-alpha)), computed exactly on thousandths
/// as k = ceil((n+1)(1000-alpha_milli)/1000). Two deliberate departures
/// where the textbook quantile is undefined or unsafe:
///
/// - **k > n** (the finite-sample correction exceeds the sample; textbook
///   conformal returns +inf, i.e. admit everything): truncated to the
///   maximum score — exactly the v0 leave-one-out max, so small calibration
///   sets keep v0 behavior and the failure direction stays "trip more",
///   never "admit everything". Since the k-th smallest at k = n is already
///   the max, threshold == max score iff k >= n, which holds iff
///   alpha_milli * (n+1) < 2000: for alpha_milli 1 up to n = 1998, for the
///   default 100 up to n = 18.
/// - **no scores** (a lone witness): 0.0, maximally conservative.
///
/// The result is clamped to [0, 1]. Jaccard distances are already in range;
/// the clamp is defensive against synthetic scores and keeps the wire's
/// threshold invariant total.
fn conformal_threshold(scores: &[f64], alpha_milli: u32) -> f64 {
    if scores.is_empty() {
        return 0.0;
    }
    let mut sorted = scores.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[conformal_k(sorted.len(), alpha_milli) - 1].clamp(0.0, 1.0)
}

/// The split-conformal quantile INDEX, shared by both calibrations so the
/// rule exists exactly once: `k = ceil((n+1)(1-alpha))`, computed exactly
/// on thousandths as `ceil((n+1)(1000-alpha_milli)/1000)`, then clamped to
/// `[1, n]` (the k > n truncation-to-max documented on
/// [`conformal_threshold`]; the saturating_sub keeps this total even for an
/// out-of-range alpha — callers validate, and the clamp then picks the min
/// or max score rather than indexing out of bounds). Requires n >= 1.
fn conformal_k(n: usize, alpha_milli: u32) -> usize {
    ((n as u64 + 1) * 1000u64.saturating_sub(u64::from(alpha_milli)))
        .div_ceil(1000)
        .clamp(1, n as u64) as usize
}

/// [`conformal_threshold`] in fixed point, for the v2 calibration: the same
/// [`conformal_k`]-th smallest score, over micros. Scores from
/// [`embedding_loo_scores`] are already clamped to `[0, 2_000_000]` by
/// [`distance_micros`], so no further clamp exists to disagree with v1's.
/// No scores (a lone witness) calibrates to 0 — maximally conservative,
/// v1's rule.
fn conformal_threshold_micros(scores: &[u32], alpha_milli: u32) -> u32 {
    if scores.is_empty() {
        return 0;
    }
    let mut sorted = scores.to_vec();
    sorted.sort_unstable();
    sorted[conformal_k(sorted.len(), alpha_milli) - 1]
}

/// Number of leave-one-out scores a witness set yields: one per witness
/// when there are at least two, none for a lone witness.
fn scores_n_for(witness_count: usize) -> usize {
    if witness_count >= 2 { witness_count } else { 0 }
}

impl Guard {
    /// Build a guard from the verified witness inputs, calibrated at the
    /// most conservative expressible miscoverage (`alpha_milli` = 1,
    /// alpha = 0.001).
    ///
    /// Kept as the compatibility constructor: for every witness count
    /// n <= 1998 the conformal quantile at alpha_milli 1 IS the maximum
    /// leave-one-out score (see [`conformal_threshold`]) — the same
    /// threshold value v0's leave-one-out-max calibration produced, so
    /// existing callers see unchanged thresholds. For n >= 1999 the
    /// quantile sits at or below that max: strictly more conservative
    /// (more trips), never less. The wire form is v1 either way.
    pub fn build(inputs: &[Value], input_field: Option<&str>) -> Result<Guard, GuardError> {
        Self::build_conformal(inputs, input_field, MAX_QUANTILE_ALPHA_MILLI)
    }

    /// Build a guard from the verified witness inputs with split-conformal
    /// calibration at nominal miscoverage `alpha_milli`/1000. `alpha_milli`
    /// must be in (0, 1000) exclusive; [`DEFAULT_ALPHA_MILLI`] (100, alpha
    /// 0.1) is the default choice.
    ///
    /// Each input's text is sketched with [`auto_model::trigram_hashes`];
    /// texts under 3 chars sketch to the empty set and are kept (empty vs
    /// empty is distance 0). Nonconformity scores are the witnesses'
    /// leave-one-out nearest-neighbor distances ([`loo_scores`]); the
    /// threshold is their split-conformal quantile
    /// ([`conformal_threshold`]). One witness yields no scores and
    /// calibrates to 0.0 — maximally conservative; only inputs whose
    /// trigram set exactly matches the witness proceed.
    ///
    /// What this calibration does and does not promise is stated in
    /// spec/runtime.md §2: the >= 1-alpha pass rate is conditional on
    /// exchangeability with the witnesses. OOD inputs violate that
    /// condition by definition, and they fail toward a trip.
    pub fn build_conformal(
        inputs: &[Value],
        input_field: Option<&str>,
        alpha_milli: u32,
    ) -> Result<Guard, GuardError> {
        if !(1..=999).contains(&alpha_milli) {
            return Err(GuardError::BadAlpha(alpha_milli));
        }
        if inputs.is_empty() {
            return Err(GuardError::NoWitnesses);
        }
        let mut witnesses = Vec::with_capacity(inputs.len());
        for (index, input) in inputs.iter().enumerate() {
            let text = extract_text(input, input_field).map_err(|detail| GuardError::NotText {
                detail: format!("witness {index}: {detail}"),
            })?;
            witnesses.push(auto_model::trigram_hashes(text));
        }
        let threshold = conformal_threshold(&loo_scores(&witnesses), alpha_milli);
        Ok(Guard {
            input_field: input_field.map(str::to_owned),
            witnesses,
            threshold,
            calibration: Calibration::SplitConformal { alpha_milli },
            embedding: None,
        })
    }

    /// Build a wire-v2 embedding guard (ADR-0023) from the verified witness
    /// inputs, split-conformally calibrated at nominal miscoverage
    /// `alpha_milli`/1000 — the opt-in parallel of
    /// [`Guard::build_conformal`], same argument order, same alpha domain.
    ///
    /// Each input's text is embedded with [`trigram_embedding`] (dense
    /// signed byte-trigram hashing, L2-normalized, dim [`EMBEDDING_DIM`]).
    /// The raw docs — sorted, deduplicated (byte order; the wire form) —
    /// are the witnesses; nonconformity scores are the witnesses'
    /// leave-one-out nearest-neighbor **cosine distances in u32 micros**
    /// ([`embedding_loo_scores`]); the threshold is their split-conformal
    /// quantile under the same [`conformal_k`] rule as v1
    /// ([`conformal_threshold_micros`]). One distinct doc yields no scores
    /// and calibrates to 0 micros — maximally conservative.
    ///
    /// **What this upgrades and what it does not.** Cosine over dense
    /// vectors weights shared trigram mass instead of set overlap — a
    /// geometry upgrade only. The features are still the bytes the text is
    /// spelled with: **lexical, not semantic**. A paraphrase in disjoint
    /// vocabulary still trips; no meaning is measured. The exchangeability
    /// caveat on [`Guard::build_conformal`] carries over verbatim: the pass
    /// rate of at least 1-alpha is conditional on future inputs being
    /// exchangeable with the witnesses, and OOD inputs are exactly the
    /// non-exchangeable case (spec/runtime.md §2, ADR-0014, ADR-0023).
    pub fn build_embedding(
        inputs: &[Value],
        input_field: Option<&str>,
        alpha_milli: u32,
    ) -> Result<Guard, GuardError> {
        if !(1..=999).contains(&alpha_milli) {
            return Err(GuardError::BadAlpha(alpha_milli));
        }
        if inputs.is_empty() {
            return Err(GuardError::NoWitnesses);
        }
        let mut docs = Vec::with_capacity(inputs.len());
        for (index, input) in inputs.iter().enumerate() {
            let text = extract_text(input, input_field).map_err(|detail| GuardError::NotText {
                detail: format!("witness {index}: {detail}"),
            })?;
            docs.push(text.to_owned());
        }
        docs.sort();
        docs.dedup();
        let vectors: Vec<Vec<f32>> = docs.iter().map(|d| trigram_embedding(d)).collect();
        let threshold_distance_micros =
            conformal_threshold_micros(&embedding_loo_scores(&vectors), alpha_milli);
        Ok(Guard {
            input_field: input_field.map(str::to_owned),
            witnesses: Vec::new(),
            threshold: f64::from(threshold_distance_micros) / 1_000_000.0,
            calibration: Calibration::SplitConformal { alpha_milli },
            embedding: Some(EmbeddingGuard {
                docs,
                vectors,
                threshold_distance_micros,
                alpha_milli,
            }),
        })
    }

    /// Canonical `guard.json` body: sorted keys, compact, the field name
    /// omitted when `None`. Deterministic — the same guard always serializes
    /// to the same bytes.
    ///
    /// Conformally calibrated Jaccard guards (everything [`Guard::build`]
    /// and [`Guard::build_conformal`] produce) serialize as wire v1. A guard
    /// parsed from a v0 document re-serializes as v0, byte-identically: it
    /// carries no declared alpha, and inventing one for the wire would be a
    /// fabricated number. An embedding guard ([`Guard::build_embedding`])
    /// serializes as wire v2 — raw docs, integer threshold, no floats. v0
    /// and v1 guards are never rewritten as v2.
    pub fn to_json(&self) -> String {
        if let Some(embedding) = &self.embedding {
            let wire = WireV2 {
                embedding: EmbeddingWire {
                    calibration: CalibrationWire {
                        alpha_milli: embedding.alpha_milli,
                        method: METHOD_SPLIT_CONFORMAL.to_owned(),
                        scores_n: scores_n_for(embedding.docs.len()),
                    },
                    dim: EMBEDDING_DIM as u32,
                    method: METHOD_TRIGRAM_HASH_COSINE.to_owned(),
                    threshold_distance_micros: embedding.threshold_distance_micros,
                },
                field: self.input_field.clone(),
                guard_version: EMBEDDING_GUARD_VERSION,
                witnesses: embedding.docs.clone(),
            };
            let value = serde_json::to_value(&wire).expect("guard wire serialization cannot fail");
            return canonical_json(&value);
        }
        let (guard_version, calibration) = match &self.calibration {
            Calibration::LeaveOneOutMax => (0, None),
            Calibration::SplitConformal { alpha_milli } => (
                GUARD_VERSION,
                Some(Some(CalibrationWire {
                    alpha_milli: *alpha_milli,
                    method: METHOD_SPLIT_CONFORMAL.to_owned(),
                    scores_n: scores_n_for(self.witnesses.len()),
                })),
            ),
        };
        let wire = Wire {
            calibration,
            guard_version,
            input_field: self.input_field.clone(),
            kind: KIND.to_owned(),
            threshold: self.threshold,
            witnesses: self.witnesses.clone(),
        };
        let value = serde_json::to_value(&wire).expect("guard wire serialization cannot fail");
        canonical_json(&value)
    }

    /// Strict parse of a `guard.json` body. Readers accept wire v0
    /// (ADR-0007: leave-one-out max, no `calibration` field — semantics
    /// unchanged), wire v1 (ADR-0014: a required, exactly-shaped
    /// `calibration` object), and wire v2 (ADR-0023: embedding guards —
    /// raw docs plus a required, exactly-shaped `embedding` object).
    ///
    /// v0/v1 refusals: a kind other than `trigram_jaccard_nn`; unknown
    /// fields (top level or inside `calibration`); zero witnesses;
    /// non-canonical (unsorted or duplicated) sketches; a threshold outside
    /// [0, 1]; a `calibration` field on v0 or a missing/null `calibration`
    /// on v1; a method other than `split_conformal`; `alpha_milli` outside
    /// (0, 1000); a `scores_n` that does not match the witness count.
    ///
    /// v2 refusals: an embedding method other than
    /// `trigram_hash_cosine`; a dim other than
    /// [`EMBEDDING_DIM`]; a missing/malformed `threshold_distance_micros`
    /// (serde: u32 only — no floats, no negatives) or one above 2_000_000;
    /// zero witness docs; a non-sorted/deduplicated doc list; the same
    /// calibration strictness as v1. Any other version is
    /// [`GuardError::UnsupportedVersion`]. A guard that cannot be trusted
    /// must not gate tier-1 — and an unreadable v2 never silently falls
    /// back to Jaccard.
    pub fn from_json(text: &str) -> Result<Guard, GuardError> {
        // the version peek (over a Value) only decides WHICH strict typed
        // parse runs; both parses then run over the original text, so
        // v0/v1 strictness — duplicate-field rejection included — is
        // exactly what it was before v2 existed
        let value: Value =
            serde_json::from_str(text).map_err(|e| GuardError::BadJson(e.to_string()))?;
        match value.get("guard_version").and_then(Value::as_u64) {
            Some(v) if v == u64::from(EMBEDDING_GUARD_VERSION) => Self::from_v2_text(text),
            Some(v) if v > u64::from(GUARD_VERSION) => Err(GuardError::UnsupportedVersion {
                found: u32::try_from(v).unwrap_or(u32::MAX),
            }),
            // 0, 1, or no readable version: the strict v0/v1 wire parse
            // decides — and names — the refusal
            _ => Self::from_v01_text(text),
        }
    }

    /// The v0/v1 half of [`Guard::from_json`]: the pre-v2 parse, behavior
    /// unchanged.
    fn from_v01_text(text: &str) -> Result<Guard, GuardError> {
        let wire: Wire =
            serde_json::from_str(text).map_err(|e| GuardError::BadJson(e.to_string()))?;
        if wire.guard_version != 0 && wire.guard_version != GUARD_VERSION {
            return Err(GuardError::UnsupportedVersion {
                found: wire.guard_version,
            });
        }
        if wire.kind != KIND {
            return Err(GuardError::UnknownKind(wire.kind));
        }
        if wire.witnesses.is_empty() {
            return Err(GuardError::NoWitnesses);
        }
        for (index, witness) in wire.witnesses.iter().enumerate() {
            if let Some(pair) = witness.windows(2).find(|pair| pair[0] >= pair[1]) {
                let what = if pair[0] == pair[1] {
                    "duplicate"
                } else {
                    "unsorted"
                };
                return Err(GuardError::BadWitness {
                    detail: format!("witness {index} is {what} at value {}", pair[1]),
                });
            }
        }
        // JSON text cannot encode NaN/Infinity, so the range check is total.
        if !(0.0..=1.0).contains(&wire.threshold) {
            return Err(GuardError::BadJson(format!(
                "threshold {} is not in [0, 1]",
                wire.threshold
            )));
        }
        let calibration = match (wire.guard_version, wire.calibration) {
            (0, None) => Calibration::LeaveOneOutMax,
            (0, Some(_)) => {
                return Err(GuardError::BadJson(
                    "guard_version 0 does not carry a calibration field".to_owned(),
                ));
            }
            (_, Some(Some(c))) => {
                if c.method != METHOD_SPLIT_CONFORMAL {
                    return Err(GuardError::BadJson(format!(
                        "unknown calibration method `{}`; guard_version 1 reads exactly \
                         `{METHOD_SPLIT_CONFORMAL}`",
                        c.method
                    )));
                }
                if !(1..=999).contains(&c.alpha_milli) {
                    return Err(GuardError::BadAlpha(c.alpha_milli));
                }
                let expected = scores_n_for(wire.witnesses.len());
                if c.scores_n != expected {
                    return Err(GuardError::BadJson(format!(
                        "calibration scores_n {} does not match {} witness(es) (expected \
                         {expected})",
                        c.scores_n,
                        wire.witnesses.len()
                    )));
                }
                Calibration::SplitConformal {
                    alpha_milli: c.alpha_milli,
                }
            }
            (_, None | Some(None)) => {
                return Err(GuardError::BadJson(
                    "guard_version 1 requires a calibration object".to_owned(),
                ));
            }
        };
        Ok(Guard {
            input_field: wire.input_field,
            witnesses: wire.witnesses,
            threshold: wire.threshold,
            calibration,
            embedding: None,
        })
    }

    /// The v2 half of [`Guard::from_json`]: strict parse of an embedding
    /// guard document. Witness vectors are recomputed from the raw docs
    /// here — the wire carries no floats to drift.
    fn from_v2_text(text: &str) -> Result<Guard, GuardError> {
        let wire: WireV2 =
            serde_json::from_str(text).map_err(|e| GuardError::BadJson(e.to_string()))?;
        // the dispatch peeked the LAST `guard_version` occurrence and the
        // strict parse rejects duplicates, so success pins the value
        debug_assert_eq!(wire.guard_version, EMBEDDING_GUARD_VERSION);
        if wire.embedding.method != METHOD_TRIGRAM_HASH_COSINE {
            return Err(GuardError::BadJson(format!(
                "unknown embedding method `{}`; guard_version 2 reads exactly \
                 `{METHOD_TRIGRAM_HASH_COSINE}` (never a silent fallback to Jaccard)",
                wire.embedding.method
            )));
        }
        if wire.embedding.dim != EMBEDDING_DIM as u32 {
            return Err(GuardError::BadJson(format!(
                "embedding dim {} is unsupported; `{METHOD_TRIGRAM_HASH_COSINE}` pins dim \
                 {EMBEDDING_DIM}",
                wire.embedding.dim
            )));
        }
        if wire.witnesses.is_empty() {
            return Err(GuardError::NoWitnesses);
        }
        if let Some(pair) = wire.witnesses.windows(2).find(|pair| pair[0] >= pair[1]) {
            let what = if pair[0] == pair[1] {
                "duplicated"
            } else {
                "unsorted"
            };
            return Err(GuardError::BadWitness {
                detail: format!("v2 witness docs are {what} at {:?}", pair[1]),
            });
        }
        let calibration = wire.embedding.calibration;
        if calibration.method != METHOD_SPLIT_CONFORMAL {
            return Err(GuardError::BadJson(format!(
                "unknown calibration method `{}`; guard_version 2 reads exactly \
                 `{METHOD_SPLIT_CONFORMAL}`",
                calibration.method
            )));
        }
        if !(1..=999).contains(&calibration.alpha_milli) {
            return Err(GuardError::BadAlpha(calibration.alpha_milli));
        }
        let expected = scores_n_for(wire.witnesses.len());
        if calibration.scores_n != expected {
            return Err(GuardError::BadJson(format!(
                "calibration scores_n {} does not match {} witness(es) (expected {expected})",
                calibration.scores_n,
                wire.witnesses.len()
            )));
        }
        if wire.embedding.threshold_distance_micros > MAX_DISTANCE_MICROS {
            return Err(GuardError::BadJson(format!(
                "threshold_distance_micros {} is not in [0, {MAX_DISTANCE_MICROS}]",
                wire.embedding.threshold_distance_micros
            )));
        }
        let vectors: Vec<Vec<f32>> = wire
            .witnesses
            .iter()
            .map(|d| trigram_embedding(d))
            .collect();
        Ok(Guard {
            input_field: wire.field,
            witnesses: Vec::new(),
            threshold: f64::from(wire.embedding.threshold_distance_micros) / 1_000_000.0,
            calibration: Calibration::SplitConformal {
                alpha_milli: calibration.alpha_milli,
            },
            embedding: Some(EmbeddingGuard {
                docs: wire.witnesses,
                vectors,
                threshold_distance_micros: wire.embedding.threshold_distance_micros,
                alpha_milli: calibration.alpha_milli,
            }),
        })
    }

    /// Guard one input. Never fails and never passes an unmeasured input
    /// through: a wrong-shaped input (no text where text is required) trips
    /// with `distance: None`.
    ///
    /// v0/v1: distance is the minimum Jaccard distance from the input's
    /// trigram sketch to any witness; `distance <= threshold` (inclusive)
    /// proceeds, anything greater trips. Calibration only changed how
    /// `threshold` was derived — evaluation semantics are identical for v0
    /// and v1 guards. v2: distance is the minimum cosine distance from the
    /// input's [`trigram_embedding`] to any witness vector, DECIDED in u32
    /// micros (`micros <= threshold_distance_micros` proceeds, greater
    /// trips); the outcome reports both sides scaled to distance units
    /// (`micros / 1e6`) so callers see the exact compared quantities.
    pub fn evaluate(&self, input: &Value) -> GuardOutcome {
        let text = match extract_text(input, self.input_field.as_deref()) {
            Ok(text) => text,
            Err(detail) => {
                return GuardOutcome::Trip {
                    reason: format!("input has no text to guard on ({detail})"),
                    distance: None,
                    threshold: self.threshold,
                };
            }
        };
        if let Some(embedding) = &self.embedding {
            // not constructible empty through the public API, but the
            // fail-closed rule holds for a hand-cloned-and-degraded guard
            // too: no witnesses, no admission
            if embedding.docs.is_empty() {
                return GuardOutcome::Trip {
                    reason: "guard has no witnesses".to_owned(),
                    distance: None,
                    threshold: self.threshold,
                };
            }
            let micros = embedding.distance_micros_to_nearest(text);
            let distance = f64::from(micros) / 1_000_000.0;
            let threshold = f64::from(embedding.threshold_distance_micros) / 1_000_000.0;
            return if micros <= embedding.threshold_distance_micros {
                GuardOutcome::Proceed {
                    distance,
                    threshold,
                }
            } else {
                GuardOutcome::Trip {
                    reason: "distance beyond calibration".to_owned(),
                    distance: Some(distance),
                    threshold,
                }
            };
        }
        // fields are public: a hand-built witness-less guard still trips
        // rather than proceeding unguarded
        if self.witnesses.is_empty() {
            return GuardOutcome::Trip {
                reason: "guard has no witnesses".to_owned(),
                distance: None,
                threshold: self.threshold,
            };
        }
        let sketch = auto_model::trigram_hashes(text);
        let mut distance = f64::INFINITY;
        for witness in &self.witnesses {
            distance = distance.min(jaccard_distance(&sketch, witness));
        }
        if distance <= self.threshold {
            GuardOutcome::Proceed {
                distance,
                threshold: self.threshold,
            }
        } else {
            GuardOutcome::Trip {
                reason: "distance beyond calibration".to_owned(),
                distance: Some(distance),
                threshold: self.threshold,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    // ---- jaccard edge cases ----

    #[test]
    fn jaccard_both_empty_is_zero() {
        assert_eq!(jaccard_distance(&[], &[]), 0.0);
    }

    #[test]
    fn jaccard_one_empty_is_one() {
        assert_eq!(jaccard_distance(&[], &[7]), 1.0);
        assert_eq!(jaccard_distance(&[7], &[]), 1.0);
    }

    #[test]
    fn jaccard_identical_is_zero() {
        assert_eq!(jaccard_distance(&[1, 2, 3], &[1, 2, 3]), 0.0);
    }

    #[test]
    fn jaccard_disjoint_is_one() {
        assert_eq!(jaccard_distance(&[1, 2], &[3, 4]), 1.0);
    }

    #[test]
    fn jaccard_half_overlap() {
        // inter {2,3} = 2, union {1,2,3,4} = 4 -> 1 - 2/4 = 0.5
        assert_eq!(jaccard_distance(&[1, 2, 3], &[2, 3, 4]), 0.5);
    }

    // ---- conformal calibration ----

    #[test]
    fn loo_scores_below_two_witnesses_is_empty() {
        assert!(loo_scores(&[]).is_empty());
        assert!(loo_scores(&[vec![1, 2]]).is_empty());
    }

    /// Same sketches as the three-witness build test: pairwise distances
    /// d01 = 1/3, d02 = 3/4, d12 = 1/2; nearest-other per witness 1/3, 1/3,
    /// 1/2.
    #[test]
    fn loo_scores_three_witnesses_hand_computed() {
        let w = [
            auto_model::trigram_hashes("abcd"),
            auto_model::trigram_hashes("abcde"),
            auto_model::trigram_hashes("bcdef"),
        ];
        // written as 1 - 2/3, the same f64 expression the distance computes
        assert_eq!(loo_scores(&w), vec![1.0 - 2.0 / 3.0, 1.0 - 2.0 / 3.0, 0.5]);
    }

    #[test]
    fn conformal_threshold_no_scores_is_zero() {
        assert_eq!(conformal_threshold(&[], 100), 0.0);
        assert_eq!(conformal_threshold(&[], 999), 0.0);
    }

    /// k = ceil((n+1)(1-alpha)) > n truncates to the max score (never
    /// "admit everything"): n = 3 at alpha 0.1 gives k = ceil(3.6) = 4 > 3.
    #[test]
    fn conformal_threshold_small_n_is_the_max_score() {
        assert_eq!(conformal_threshold(&[0.2, 0.5, 0.3], 100), 0.5);
    }

    /// n = 10 scores 0.01..=0.10 at alpha 0.2: k = ceil(11 * 0.8) = 9, so
    /// the 9th smallest (0.09) — strictly below the max (0.10). Input
    /// deliberately unsorted: the quantile sorts.
    #[test]
    fn conformal_threshold_picks_the_exact_quantile_index() {
        let scores = [0.10, 0.01, 0.09, 0.02, 0.08, 0.03, 0.07, 0.04, 0.06, 0.05];
        assert_eq!(conformal_threshold(&scores, 200), 0.09);
    }

    /// alpha 0.999 (the largest expressible): k = ceil((n+1)/1000) = 1 for
    /// small n — the smallest score.
    #[test]
    fn conformal_threshold_alpha_999_is_the_smallest_score() {
        assert_eq!(conformal_threshold(&[0.4, 0.1, 0.7], 999), 0.1);
    }

    /// Clamping, exercised with synthetic out-of-range scores. Jaccard
    /// distances cannot produce these; the clamp is defensive.
    #[test]
    fn conformal_threshold_clamps_synthetic_scores_into_unit_range() {
        assert_eq!(conformal_threshold(&[1.5, 2.5], 500), 1.0);
        assert_eq!(conformal_threshold(&[-0.5, -0.25], 500), 0.0);
    }

    #[test]
    fn build_conformal_rejects_alpha_out_of_range() {
        for bad in [0u32, 1000, 1001] {
            match Guard::build_conformal(&[json!("abcd")], None, bad) {
                Err(GuardError::BadAlpha(a)) => assert_eq!(a, bad),
                other => panic!("expected BadAlpha for {bad}, got {other:?}"),
            }
        }
    }

    /// The documented delegation: `build` == `build_conformal` at
    /// alpha_milli 1.
    #[test]
    fn build_equals_build_conformal_at_alpha_one() {
        let inputs = [json!("abcd"), json!("abcde"), json!("bcdef")];
        assert_eq!(
            Guard::build(&inputs, None).unwrap(),
            Guard::build_conformal(&inputs, None, 1).unwrap()
        );
    }

    // ---- build ----

    #[test]
    fn build_empty_inputs_is_no_witnesses() {
        assert!(matches!(
            Guard::build(&[], None),
            Err(GuardError::NoWitnesses)
        ));
        assert!(matches!(
            Guard::build_conformal(&[], None, 100),
            Err(GuardError::NoWitnesses)
        ));
    }

    #[test]
    fn build_single_witness_calibrates_to_zero() {
        let guard = Guard::build(&[json!("hello world")], None).unwrap();
        assert_eq!(guard.threshold, 0.0);
        assert_eq!(guard.witnesses.len(), 1);
        assert_eq!(
            guard.witnesses[0],
            auto_model::trigram_hashes("hello world")
        );
        assert_eq!(guard.input_field, None);
        assert_eq!(
            guard.calibration,
            Calibration::SplitConformal { alpha_milli: 1 }
        );
        // no leave-one-out scores exist for one witness at any alpha
        let at_default = Guard::build_conformal(&[json!("hello world")], None, 100).unwrap();
        assert_eq!(at_default.threshold, 0.0);
    }

    /// Hand-computed calibration on three tiny texts.
    ///
    /// Sketches: "abcd" = {abc,bcd}, "abcde" = {abc,bcd,cde},
    /// "bcdef" = {bcd,cde,def}. Pairwise distances: d01 = 1-2/3 = 1/3,
    /// d02 = 1-1/4 = 3/4, d12 = 1-2/4 = 1/2. Leave-one-out scores:
    /// 1/3, 1/3, 1/2. `build` calibrates at alpha_milli 1: k =
    /// ceil(4 * 0.999) = 4 > 3, so the max score = 0.5 exactly — the v0
    /// leave-one-out-max threshold.
    #[test]
    fn build_three_witnesses_max_quantile_is_leave_one_out_max() {
        let guard = Guard::build(&[json!("abcd"), json!("abcde"), json!("bcdef")], None).unwrap();
        assert_eq!(guard.threshold, 0.5);
    }

    #[test]
    fn build_non_string_input_is_not_text() {
        match Guard::build(&[json!("abc"), json!(5)], None) {
            Err(GuardError::NotText { detail }) => {
                assert!(detail.contains("witness 1"), "{detail}");
                assert!(detail.contains("a number, not a string"), "{detail}");
            }
            other => panic!("expected NotText, got {other:?}"),
        }
    }

    #[test]
    fn build_field_mode_wrong_shapes_are_not_text() {
        // not an object
        match Guard::build(&[json!("plain")], Some("text")) {
            Err(GuardError::NotText { detail }) => {
                assert!(detail.contains("witness 0"), "{detail}");
                assert!(detail.contains("not an object"), "{detail}");
            }
            other => panic!("expected NotText, got {other:?}"),
        }
        // field absent
        match Guard::build(
            &[json!({"text": "abcd"}), json!({"other": 1})],
            Some("text"),
        ) {
            Err(GuardError::NotText { detail }) => {
                assert!(detail.contains("witness 1"), "{detail}");
                assert!(detail.contains("no field `text`"), "{detail}");
            }
            other => panic!("expected NotText, got {other:?}"),
        }
        // field not a string
        match Guard::build(&[json!({"text": 7})], Some("text")) {
            Err(GuardError::NotText { detail }) => {
                assert!(detail.contains("`text` is a number"), "{detail}");
            }
            other => panic!("expected NotText, got {other:?}"),
        }
    }

    #[test]
    fn build_is_deterministic() {
        let inputs = [
            json!({"q": "abcd"}),
            json!({"q": "abcde"}),
            json!({"q": "bcdef"}),
        ];
        let a = Guard::build(&inputs, Some("q")).unwrap();
        let b = Guard::build(&inputs, Some("q")).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.to_json(), b.to_json());
    }

    // ---- evaluate ----

    #[test]
    fn single_witness_identical_text_proceeds() {
        let guard = Guard::build(&[json!("hello world")], None).unwrap();
        assert_eq!(
            guard.evaluate(&json!("hello world")),
            GuardOutcome::Proceed {
                distance: 0.0,
                threshold: 0.0
            }
        );
    }

    #[test]
    fn single_witness_near_identical_text_trips() {
        let guard = Guard::build(&[json!("hello world")], None).unwrap();
        // "hello worlds" adds trigram {lds}: distance 1 - 9/10 > 0
        match guard.evaluate(&json!("hello worlds")) {
            GuardOutcome::Trip {
                reason,
                distance: Some(d),
                threshold,
            } => {
                assert_eq!(reason, "distance beyond calibration");
                assert!(d > 0.0 && d < 1.0, "{d}");
                assert_eq!(threshold, 0.0);
            }
            other => panic!("expected Trip with a distance, got {other:?}"),
        }
    }

    #[test]
    fn near_input_proceeds_within_calibration() {
        let guard = Guard::build(&[json!("abcd"), json!("abcde"), json!("bcdef")], None).unwrap();
        // "bcde" = {bcd,cde}: nearest witness "abcde" (or "bcdef") at
        // 1 - 2/3, which is under the 0.5 threshold
        assert_eq!(
            guard.evaluate(&json!("bcde")),
            GuardOutcome::Proceed {
                distance: 1.0 - 2.0 / 3.0,
                threshold: 0.5
            }
        );
    }

    #[test]
    fn far_input_trips() {
        let guard = Guard::build(&[json!("abcd"), json!("abcde"), json!("bcdef")], None).unwrap();
        assert_eq!(
            guard.evaluate(&json!("zzzz qqqq")),
            GuardOutcome::Trip {
                reason: "distance beyond calibration".to_owned(),
                distance: Some(1.0),
                threshold: 0.5
            }
        );
    }

    #[test]
    fn wrong_shape_input_trips_with_no_distance() {
        let plain = Guard::build(&[json!("abcd")], None).unwrap();
        match plain.evaluate(&json!({"a": 1})) {
            GuardOutcome::Trip {
                reason,
                distance: None,
                ..
            } => {
                assert!(
                    reason.starts_with("input has no text to guard on"),
                    "{reason}"
                );
                assert!(reason.contains("an object, not a string"), "{reason}");
            }
            other => panic!("expected Trip without distance, got {other:?}"),
        }

        let fielded = Guard::build(&[json!({"text": "abcd"})], Some("text")).unwrap();
        for bad in [json!("bare"), json!({"other": "x"}), json!({"text": 3})] {
            match fielded.evaluate(&bad) {
                GuardOutcome::Trip {
                    reason,
                    distance: None,
                    ..
                } => assert!(
                    reason.starts_with("input has no text to guard on"),
                    "{reason}"
                ),
                other => panic!("expected Trip without distance on {bad}, got {other:?}"),
            }
        }
    }

    #[test]
    fn field_mode_extracts_and_proceeds() {
        let guard = Guard::build(&[json!({"text": "hello world"})], Some("text")).unwrap();
        assert_eq!(
            guard.evaluate(&json!({"text": "hello world", "extra": 1})),
            GuardOutcome::Proceed {
                distance: 0.0,
                threshold: 0.0
            }
        );
    }

    /// A witness text under 3 chars sketches to the empty set and is kept.
    /// Honest quirk of set semantics: EVERY trigramless input (< 3 chars)
    /// matches it at distance 0; any input with trigrams is at distance 1.
    #[test]
    fn empty_text_witness_matches_only_trigramless_inputs() {
        let guard = Guard::build(&[json!("")], None).unwrap();
        assert_eq!(guard.witnesses, vec![Vec::<u32>::new()]);
        assert_eq!(guard.threshold, 0.0);
        assert_eq!(
            guard.evaluate(&json!("")),
            GuardOutcome::Proceed {
                distance: 0.0,
                threshold: 0.0
            }
        );
        assert_eq!(
            guard.evaluate(&json!("ab")),
            GuardOutcome::Proceed {
                distance: 0.0,
                threshold: 0.0
            }
        );
        assert_eq!(
            guard.evaluate(&json!("abc")),
            GuardOutcome::Trip {
                reason: "distance beyond calibration".to_owned(),
                distance: Some(1.0),
                threshold: 0.0
            }
        );
    }

    #[test]
    fn hand_built_guard_without_witnesses_trips() {
        let guard = Guard {
            input_field: None,
            witnesses: vec![],
            threshold: 1.0,
            calibration: Calibration::SplitConformal { alpha_milli: 100 },
            embedding: None,
        };
        assert_eq!(
            guard.evaluate(&json!("anything")),
            GuardOutcome::Trip {
                reason: "guard has no witnesses".to_owned(),
                distance: None,
                threshold: 1.0
            }
        );
    }

    // ---- wire ----

    #[test]
    fn to_json_is_canonical_v1_without_input_field() {
        let guard = Guard::build(&[json!("abc")], None).unwrap();
        let h = auto_model::fnv1a_32(b"abc");
        assert_eq!(
            guard.to_json(),
            format!(
                "{{\"calibration\":{{\"alpha_milli\":1,\"method\":\"split_conformal\",\
                 \"scores_n\":0}},\"guard_version\":1,\"kind\":\"trigram_jaccard_nn\",\
                 \"threshold\":0.0,\"witnesses\":[[{h}]]}}"
            )
        );
    }

    #[test]
    fn to_json_is_canonical_v1_with_input_field() {
        let guard = Guard::build_conformal(&[json!({"q": "abc"})], Some("q"), 100).unwrap();
        let h = auto_model::fnv1a_32(b"abc");
        assert_eq!(
            guard.to_json(),
            format!(
                "{{\"calibration\":{{\"alpha_milli\":100,\"method\":\"split_conformal\",\
                 \"scores_n\":0}},\"guard_version\":1,\"input_field\":\"q\",\
                 \"kind\":\"trigram_jaccard_nn\",\"threshold\":0.0,\"witnesses\":[[{h}]]}}"
            )
        );
    }

    #[test]
    fn roundtrip_preserves_guard_and_verdicts() {
        let inputs = [
            json!({"q": "abcd"}),
            json!({"q": "abcde"}),
            json!({"q": "bcdef"}),
        ];
        let built = Guard::build(&inputs, Some("q")).unwrap();
        let parsed = Guard::from_json(&built.to_json()).unwrap();
        assert_eq!(parsed, built);
        assert_eq!(parsed.to_json(), built.to_json());
        for probe in [
            json!({"q": "abcd"}),
            json!({"q": "bcde"}),
            json!({"q": "zzzz"}),
            json!(9),
        ] {
            assert_eq!(parsed.evaluate(&probe), built.evaluate(&probe));
        }
    }

    /// A v0 document (no `calibration` field) still parses, still evaluates
    /// with unchanged semantics, and re-serializes byte-identically — the
    /// wire compatibility pin.
    #[test]
    fn v0_document_parses_and_reserializes_byte_identically() {
        let v0 = wire(0, "trigram_jaccard_nn", 0.5, "[[1,2],[2,3,4]]");
        let guard = Guard::from_json(&v0).unwrap();
        assert_eq!(guard.calibration, Calibration::LeaveOneOutMax);
        assert_eq!(guard.threshold, 0.5);
        assert_eq!(guard.to_json(), v0);
    }

    fn wire(version: u32, kind: &str, threshold: f64, witnesses: &str) -> String {
        format!(
            "{{\"guard_version\":{version},\"kind\":\"{kind}\",\
             \"threshold\":{threshold},\"witnesses\":{witnesses}}}"
        )
    }

    /// A v1 document body with the given calibration object.
    fn wire_v1(calibration: &str, witnesses: &str) -> String {
        format!(
            "{{\"calibration\":{calibration},\"guard_version\":1,\
             \"kind\":\"trigram_jaccard_nn\",\"threshold\":0.5,\"witnesses\":{witnesses}}}"
        )
    }

    #[test]
    fn from_json_rejects_unknown_version() {
        match Guard::from_json(&wire(3, "trigram_jaccard_nn", 0.0, "[[1]]")) {
            Err(GuardError::UnsupportedVersion { found }) => assert_eq!(found, 3),
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
    }

    /// guard_version 2 with the v0/v1 shape is a v2 parse failure (unknown
    /// fields, missing `embedding`) — never read as a Jaccard guard.
    #[test]
    fn from_json_version_two_with_jaccard_shape_is_rejected() {
        assert!(matches!(
            Guard::from_json(&wire(2, "trigram_jaccard_nn", 0.0, "[[1]]")),
            Err(GuardError::BadJson(_))
        ));
    }

    /// Duplicate JSON fields refuse on every version — the version peek
    /// only dispatches, the strict typed parse still sees the raw text
    /// (pre-v2 strictness, preserved).
    #[test]
    fn from_json_rejects_duplicate_fields_on_every_version() {
        for text in [
            // duplicated threshold on v0
            "{\"guard_version\":0,\"kind\":\"trigram_jaccard_nn\",\"threshold\":0.0,\
             \"threshold\":1.0,\"witnesses\":[[1]]}",
            // duplicated, disagreeing guard_version (peek sees the last:
            // dispatches v0/v1, whose parse rejects the duplicate)
            "{\"guard_version\":2,\"guard_version\":0,\"kind\":\"trigram_jaccard_nn\",\
             \"threshold\":0.0,\"witnesses\":[[1]]}",
            // duplicated guard_version on v2
            "{\"embedding\":{\"calibration\":{\"alpha_milli\":100,\
             \"method\":\"split_conformal\",\"scores_n\":0},\"dim\":256,\
             \"method\":\"trigram_hash_cosine\",\"threshold_distance_micros\":0},\
             \"guard_version\":2,\"guard_version\":2,\"witnesses\":[\"abc\"]}",
        ] {
            match Guard::from_json(text) {
                Err(GuardError::BadJson(detail)) => {
                    assert!(detail.contains("duplicate field"), "{detail}");
                }
                other => panic!("expected duplicate-field BadJson, got {other:?}"),
            }
        }
    }

    #[test]
    fn from_json_rejects_unknown_kind() {
        match Guard::from_json(&wire(0, "embedding_ood", 0.0, "[[1]]")) {
            Err(GuardError::UnknownKind(kind)) => assert_eq!(kind, "embedding_ood"),
            other => panic!("expected UnknownKind, got {other:?}"),
        }
    }

    #[test]
    fn from_json_rejects_unknown_field() {
        let text = "{\"guard_version\":0,\"kind\":\"trigram_jaccard_nn\",\
                     \"threshold\":0.0,\"witnesses\":[[1]],\"extra\":true}";
        assert!(matches!(
            Guard::from_json(text),
            Err(GuardError::BadJson(_))
        ));
    }

    #[test]
    fn from_json_rejects_missing_field() {
        let text = "{\"guard_version\":0,\"kind\":\"trigram_jaccard_nn\",\"witnesses\":[[1]]}";
        assert!(matches!(
            Guard::from_json(text),
            Err(GuardError::BadJson(_))
        ));
    }

    #[test]
    fn from_json_rejects_syntactic_garbage() {
        assert!(matches!(
            Guard::from_json("not json at all"),
            Err(GuardError::BadJson(_))
        ));
    }

    #[test]
    fn from_json_rejects_zero_witnesses() {
        assert!(matches!(
            Guard::from_json(&wire(0, "trigram_jaccard_nn", 0.0, "[]")),
            Err(GuardError::NoWitnesses)
        ));
    }

    #[test]
    fn from_json_rejects_unsorted_witness() {
        match Guard::from_json(&wire(0, "trigram_jaccard_nn", 0.0, "[[2,1]]")) {
            Err(GuardError::BadWitness { detail }) => {
                assert!(detail.contains("witness 0"), "{detail}");
                assert!(detail.contains("unsorted"), "{detail}");
            }
            other => panic!("expected BadWitness, got {other:?}"),
        }
    }

    #[test]
    fn from_json_rejects_duplicated_witness_value() {
        match Guard::from_json(&wire(0, "trigram_jaccard_nn", 0.0, "[[1],[3,3]]")) {
            Err(GuardError::BadWitness { detail }) => {
                assert!(detail.contains("witness 1"), "{detail}");
                assert!(detail.contains("duplicate"), "{detail}");
            }
            other => panic!("expected BadWitness, got {other:?}"),
        }
    }

    #[test]
    fn from_json_rejects_threshold_out_of_range() {
        for bad in ["1.5", "-0.5"] {
            let text = format!(
                "{{\"guard_version\":0,\"kind\":\"trigram_jaccard_nn\",\
                 \"threshold\":{bad},\"witnesses\":[[1]]}}"
            );
            match Guard::from_json(&text) {
                Err(GuardError::BadJson(detail)) => {
                    assert!(detail.contains("threshold"), "{detail}");
                }
                other => panic!("expected BadJson for threshold {bad}, got {other:?}"),
            }
        }
    }

    #[test]
    fn from_json_accepts_the_empty_sketch() {
        let guard = Guard::from_json(&wire(0, "trigram_jaccard_nn", 0.0, "[[]]")).unwrap();
        assert_eq!(guard.witnesses, vec![Vec::<u32>::new()]);
        assert!(matches!(
            guard.evaluate(&json!("")),
            GuardOutcome::Proceed { .. }
        ));
    }

    // ---- wire v1 strictness ----

    #[test]
    fn v1_round_trips_canonically() {
        let text = wire_v1(
            "{\"alpha_milli\":250,\"method\":\"split_conformal\",\"scores_n\":2}",
            "[[1,2],[2,3]]",
        );
        let guard = Guard::from_json(&text).unwrap();
        assert_eq!(
            guard.calibration,
            Calibration::SplitConformal { alpha_milli: 250 }
        );
        assert_eq!(guard.to_json(), text);
    }

    #[test]
    fn v0_with_calibration_field_is_rejected() {
        for calibration in [
            "{\"alpha_milli\":100,\"method\":\"split_conformal\",\"scores_n\":0}",
            "null",
        ] {
            let text = format!(
                "{{\"calibration\":{calibration},\"guard_version\":0,\
                 \"kind\":\"trigram_jaccard_nn\",\"threshold\":0.0,\"witnesses\":[[1]]}}"
            );
            match Guard::from_json(&text) {
                Err(GuardError::BadJson(detail)) => {
                    assert!(detail.contains("guard_version 0"), "{detail}");
                }
                other => panic!("expected BadJson, got {other:?}"),
            }
        }
    }

    #[test]
    fn v1_without_calibration_is_rejected() {
        for text in [
            wire(1, "trigram_jaccard_nn", 0.0, "[[1]]"),
            // explicit null is not an object either
            "{\"calibration\":null,\"guard_version\":1,\"kind\":\"trigram_jaccard_nn\",\
             \"threshold\":0.0,\"witnesses\":[[1]]}"
                .to_owned(),
        ] {
            match Guard::from_json(&text) {
                Err(GuardError::BadJson(detail)) => {
                    assert!(detail.contains("requires a calibration object"), "{detail}");
                }
                other => panic!("expected BadJson, got {other:?}"),
            }
        }
    }

    #[test]
    fn v1_rejects_unknown_method() {
        let text = wire_v1(
            "{\"alpha_milli\":100,\"method\":\"full_conformal\",\"scores_n\":2}",
            "[[1],[2]]",
        );
        match Guard::from_json(&text) {
            Err(GuardError::BadJson(detail)) => {
                assert!(detail.contains("full_conformal"), "{detail}");
                assert!(detail.contains("split_conformal"), "{detail}");
            }
            other => panic!("expected BadJson, got {other:?}"),
        }
    }

    #[test]
    fn v1_rejects_alpha_zero_and_thousand() {
        for bad in [0u32, 1000] {
            let text = wire_v1(
                &format!("{{\"alpha_milli\":{bad},\"method\":\"split_conformal\",\"scores_n\":2}}"),
                "[[1],[2]]",
            );
            match Guard::from_json(&text) {
                Err(GuardError::BadAlpha(a)) => assert_eq!(a, bad),
                other => panic!("expected BadAlpha for {bad}, got {other:?}"),
            }
        }
    }

    #[test]
    fn v1_rejects_unknown_field_inside_calibration() {
        let text = wire_v1(
            "{\"alpha_milli\":100,\"method\":\"split_conformal\",\"scores_n\":2,\"extra\":1}",
            "[[1],[2]]",
        );
        assert!(matches!(
            Guard::from_json(&text),
            Err(GuardError::BadJson(_))
        ));
    }

    #[test]
    fn v1_rejects_scores_n_mismatch() {
        // two witnesses yield two leave-one-out scores, not seven
        let text = wire_v1(
            "{\"alpha_milli\":100,\"method\":\"split_conformal\",\"scores_n\":7}",
            "[[1],[2]]",
        );
        match Guard::from_json(&text) {
            Err(GuardError::BadJson(detail)) => {
                assert!(detail.contains("scores_n 7"), "{detail}");
                assert!(detail.contains("expected 2"), "{detail}");
            }
            other => panic!("expected BadJson, got {other:?}"),
        }
        // a lone witness yields zero scores
        let lone = wire_v1(
            "{\"alpha_milli\":100,\"method\":\"split_conformal\",\"scores_n\":1}",
            "[[1]]",
        );
        match Guard::from_json(&lone) {
            Err(GuardError::BadJson(detail)) => {
                assert!(detail.contains("expected 0"), "{detail}");
            }
            other => panic!("expected BadJson, got {other:?}"),
        }
    }

    #[test]
    fn v1_rejects_threshold_out_of_range() {
        let text = "{\"calibration\":{\"alpha_milli\":100,\"method\":\"split_conformal\",\
                     \"scores_n\":2},\"guard_version\":1,\"kind\":\"trigram_jaccard_nn\",\
                     \"threshold\":1.5,\"witnesses\":[[1],[2]]}";
        match Guard::from_json(text) {
            Err(GuardError::BadJson(detail)) => {
                assert!(detail.contains("threshold"), "{detail}");
            }
            other => panic!("expected BadJson, got {other:?}"),
        }
    }

    // ---- v2 featurizer (ADR-0023) ----

    #[test]
    fn fnv1a_64_pinned_vectors() {
        // the offset basis (empty input) and the published FNV-1a 64 test
        // vector for "a" — a drift here reshuffles every bucket and sign
        assert_eq!(fnv1a_64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a_64(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn trigram_embedding_is_unit_norm_dim_256_and_deterministic() {
        let v = trigram_embedding("Compilers translate agent cognition.");
        assert_eq!(v.len(), EMBEDDING_DIM);
        let norm_squared: f64 = v.iter().map(|&x| f64::from(x) * f64::from(x)).sum();
        assert!((norm_squared - 1.0).abs() < 1e-6, "{norm_squared}");
        assert_eq!(v, trigram_embedding("Compilers translate agent cognition."));
    }

    /// The accumulation is a bag of trigrams: "abcab" and "bcabc" have the
    /// same trigram multiset {abc, bca, cab}, so they embed identically.
    #[test]
    fn trigram_embedding_is_order_independent_over_the_trigram_bag() {
        assert_eq!(trigram_embedding("abcab"), trigram_embedding("bcabc"));
    }

    /// Under 3 bytes there are no byte trigrams: the zero vector, never a
    /// fake normalization.
    #[test]
    fn trigram_embedding_under_three_bytes_is_the_zero_vector() {
        for text in ["", "a", "ab"] {
            assert_eq!(
                trigram_embedding(text),
                vec![0.0; EMBEDDING_DIM],
                "{text:?}"
            );
        }
    }

    /// One trigram lands on exactly one bucket at magnitude 1 — and the
    /// bucket is the frozen hash's.
    #[test]
    fn trigram_embedding_single_trigram_is_a_signed_unit_axis() {
        let v = trigram_embedding("abc");
        let nonzero: Vec<f32> = v.iter().copied().filter(|&x| x != 0.0).collect();
        assert_eq!(nonzero.len(), 1);
        assert!(nonzero[0] == 1.0 || nonzero[0] == -1.0, "{}", nonzero[0]);
        let bucket = (fnv1a_64(b"abc") % EMBEDDING_DIM as u64) as usize;
        assert_eq!(v[bucket].abs(), 1.0);
    }

    /// Byte trigrams, no lowercasing — deliberately not v1's char-trigram
    /// rule. Case changes the bytes, so it changes the vector.
    #[test]
    fn trigram_embedding_is_case_sensitive_unlike_v1_sketches() {
        assert_ne!(trigram_embedding("ABC DEF"), trigram_embedding("abc def"));
        assert_eq!(
            auto_model::trigram_hashes("ABC DEF"),
            auto_model::trigram_hashes("abc def")
        );
    }

    // ---- v2 cosine + fixed point ----

    #[test]
    fn cosine_distance_edges_mirror_v1_empty_sketch_rules() {
        let zero = vec![0.0f32; EMBEDDING_DIM];
        let unit = trigram_embedding("hello world");
        // both trigramless: identical, if vacuous — distance 0
        assert_eq!(cosine_distance(&zero, &zero), 0.0);
        // exactly one trigramless: nothing shared — distance exactly 1
        assert_eq!(cosine_distance(&zero, &unit), 1.0);
        assert_eq!(cosine_distance(&unit, &zero), 1.0);
        // self-distance rounds to 0 micros
        assert_eq!(distance_micros(cosine_distance(&unit, &unit)), 0);
    }

    /// The ONE f32 -> u32 boundary, pinned row by row. Every input is an
    /// exact dyadic f32, so each row is bit-reproducible; 1/128 scales to
    /// exactly 7812.5 micros and must round UP (round half up).
    #[test]
    fn distance_micros_rounding_pinned_by_table() {
        let table: [(f32, u32); 10] = [
            (0.0, 0),
            (0.5, 500_000),
            (1.0, 1_000_000),
            (2.0, 2_000_000),
            (2.5, 2_000_000),      // clamp: above the cosine ceiling
            (-1.0, 0),             // clamp: no negative distance
            (0.007_812_5, 7_813),  // 1/128 -> 7812.5 -> half rounds up
            (0.023_437_5, 23_438), // 3/128 -> 23437.5 -> half rounds up
            (0.003_906_25, 3_906), // 1/256 -> 3906.25 -> rounds down
            (0.015_625, 15_625),   // 1/64 -> exact
        ];
        for (input, expected) in table {
            assert_eq!(distance_micros(input), expected, "input {input}");
        }
        // total even off the cosine path: NaN fails toward a trip
        assert_eq!(distance_micros(f32::NAN), 2_000_000);
    }

    /// The v2 micros calibration picks the SAME element as the v1 f64
    /// calibration on the same score multiset — the quantile rule is the
    /// shared [`conformal_k`], not a fork.
    #[test]
    fn micros_threshold_matches_the_v1_quantile_rule_on_the_same_multiset() {
        let micros: Vec<u32> = vec![
            100_000, 10_000, 90_000, 20_000, 80_000, 30_000, 70_000, 40_000, 60_000, 50_000,
        ];
        let as_distance: Vec<f64> = micros.iter().map(|&m| f64::from(m) / 1_000_000.0).collect();
        for alpha_milli in [1u32, 100, 200, 500, 999] {
            assert_eq!(
                f64::from(conformal_threshold_micros(&micros, alpha_milli)) / 1_000_000.0,
                conformal_threshold(&as_distance, alpha_milli),
                "alpha_milli {alpha_milli}"
            );
        }
        // and the documented v1 anchor: alpha 0.2 over these 10 scores picks
        // the 9th smallest (k = ceil(11 * 0.8) = 9)
        assert_eq!(conformal_threshold_micros(&micros, 200), 90_000);
        assert!(conformal_threshold_micros(&[], 100) == 0);
    }

    // ---- v2 build ----

    #[test]
    fn build_embedding_rejects_alpha_out_of_range() {
        for bad in [0u32, 1000, 1001] {
            match Guard::build_embedding(&[json!("abcd")], None, bad) {
                Err(GuardError::BadAlpha(a)) => assert_eq!(a, bad),
                other => panic!("expected BadAlpha for {bad}, got {other:?}"),
            }
        }
    }

    #[test]
    fn build_embedding_empty_inputs_is_no_witnesses() {
        assert!(matches!(
            Guard::build_embedding(&[], None, 100),
            Err(GuardError::NoWitnesses)
        ));
    }

    #[test]
    fn build_embedding_non_string_input_is_not_text() {
        match Guard::build_embedding(&[json!("abc"), json!(5)], None, 100) {
            Err(GuardError::NotText { detail }) => {
                assert!(detail.contains("witness 1"), "{detail}");
                assert!(detail.contains("a number, not a string"), "{detail}");
            }
            other => panic!("expected NotText, got {other:?}"),
        }
    }

    /// The v2 witness list is the raw docs, sorted and deduplicated — the
    /// canonical wire form.
    #[test]
    fn build_embedding_sorts_and_dedups_docs() {
        let guard = Guard::build_embedding(
            &[
                json!("beta doc one"),
                json!("alpha doc two"),
                json!("beta doc one"),
            ],
            None,
            100,
        )
        .unwrap();
        let embedding = guard.embedding.as_ref().unwrap();
        assert_eq!(embedding.docs(), ["alpha doc two", "beta doc one"]);
        assert!(guard.witnesses.is_empty());
        assert!(
            guard.to_json().contains("\"scores_n\":2"),
            "{}",
            guard.to_json()
        );
    }

    /// A lone distinct doc has no leave-one-out scores: 0 micros, maximally
    /// conservative — only inputs at rounded distance 0 proceed.
    #[test]
    fn build_embedding_single_doc_calibrates_to_zero_micros() {
        let guard = Guard::build_embedding(&[json!("hello world")], None, 100).unwrap();
        let embedding = guard.embedding.as_ref().unwrap();
        assert_eq!(embedding.threshold_distance_micros(), 0);
        assert_eq!(embedding.alpha_milli(), 100);
        assert_eq!(guard.threshold, 0.0);
        assert_eq!(
            guard.evaluate(&json!("hello world")),
            GuardOutcome::Proceed {
                distance: 0.0,
                threshold: 0.0
            }
        );
        assert!(matches!(
            guard.evaluate(&json!("hello worlds")),
            GuardOutcome::Trip { distance: Some(d), .. } if d > 0.0
        ));
    }

    #[test]
    fn build_embedding_is_deterministic() {
        let inputs = [
            json!({"q": "abcd"}),
            json!({"q": "abcde"}),
            json!({"q": "bcdef"}),
        ];
        let a = Guard::build_embedding(&inputs, Some("q"), 100).unwrap();
        let b = Guard::build_embedding(&inputs, Some("q"), 100).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.to_json(), b.to_json());
    }

    // ---- v2 wire ----

    #[test]
    fn to_json_is_canonical_v2_with_field() {
        let guard = Guard::build_embedding(&[json!({"q": "abc"})], Some("q"), 100).unwrap();
        assert_eq!(
            guard.to_json(),
            "{\"embedding\":{\"calibration\":{\"alpha_milli\":100,\
             \"method\":\"split_conformal\",\"scores_n\":0},\"dim\":256,\
             \"method\":\"trigram_hash_cosine\",\"threshold_distance_micros\":0},\
             \"field\":\"q\",\"guard_version\":2,\"witnesses\":[\"abc\"]}"
        );
    }

    #[test]
    fn to_json_is_canonical_v2_without_field() {
        let guard = Guard::build_embedding(&[json!("abc")], None, 250).unwrap();
        assert_eq!(
            guard.to_json(),
            "{\"embedding\":{\"calibration\":{\"alpha_milli\":250,\
             \"method\":\"split_conformal\",\"scores_n\":0},\"dim\":256,\
             \"method\":\"trigram_hash_cosine\",\"threshold_distance_micros\":0},\
             \"guard_version\":2,\"witnesses\":[\"abc\"]}"
        );
    }

    #[test]
    fn v2_roundtrip_preserves_guard_and_verdicts() {
        let inputs = [
            json!({"q": "abcd"}),
            json!({"q": "abcde"}),
            json!({"q": "bcdef"}),
        ];
        let built = Guard::build_embedding(&inputs, Some("q"), 100).unwrap();
        let parsed = Guard::from_json(&built.to_json()).unwrap();
        assert_eq!(parsed, built);
        assert_eq!(parsed.to_json(), built.to_json());
        for probe in [
            json!({"q": "abcd"}),
            json!({"q": "bcde"}),
            json!({"q": "zzzz"}),
            json!(9),
        ] {
            assert_eq!(parsed.evaluate(&probe), built.evaluate(&probe));
        }
    }
}
