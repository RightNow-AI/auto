//! Guard wire v2 (embedding guards, ADR-0023) through the public API.
//!
//! The load-bearing proofs live here, computed BOTH ways — the production
//! path versus an independent test-local implementation of the frozen
//! featurizer (byte trigrams -> fnv1a-64 -> signed 256-bucket hashing ->
//! L2 norm), cosine distance in u32 micros, and the split-conformal
//! leave-one-out quantile:
//!
//! - v0 and v1 documents still parse, evaluate identically, and
//!   re-serialize byte-identically — v2's existence changes zero bytes of
//!   existing wire (the compatibility pin);
//! - the v2 threshold IS the ceil((n+1)(1-alpha)) quantile of the
//!   leave-one-out cosine micros scores, the same rule as v1 over the same
//!   score-multiset shape;
//! - near-variant witness docs (the wave-4 fixture style: each witness has
//!   a real one-word-apart neighbor, so the calibration is meaningful)
//!   admit a near-variant input and trip a disjoint-vocabulary one;
//! - **lexical, not semantic**: the admitted probe shares spelling with a
//!   witness, not meaning — a disjoint-vocabulary paraphrase still trips.
//!
//! Everything here is offline: no network, no model downloads (that is why
//! the embedding is trigram hashing and not a semantic encoder).

use auto_runtime::guard::{EMBEDDING_DIM, trigram_embedding};
use auto_runtime::{Guard, GuardOutcome};
use serde_json::json;

// ---- test-local independent implementation ----

/// Test-local FNV-1a 64 — the frozen constants, written independently.
fn fnv64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(14_695_981_039_346_656_037u64, |hash, b| {
        (hash ^ u64::from(*b)).wrapping_mul(1_099_511_628_211)
    })
}

/// Test-local featurizer: signed byte-trigram counts into 256 buckets,
/// L2-normalized through f64, cast to f32.
fn embed(text: &str) -> Vec<f32> {
    let mut counts = vec![0i64; 256];
    for window in text.as_bytes().windows(3) {
        let hash = fnv64(window);
        let sign = if (hash >> 32) & 1 == 1 { 1 } else { -1 };
        counts[(hash % 256) as usize] += sign;
    }
    let norm = counts
        .iter()
        .map(|&c| (c as f64) * (c as f64))
        .sum::<f64>()
        .sqrt();
    if norm == 0.0 {
        return vec![0.0; 256];
    }
    counts.iter().map(|&c| ((c as f64) / norm) as f32).collect()
}

/// Test-local cosine distance in micros: f64 dot in index order,
/// 1 - dot as f32, times 1e6, round half up, clamped to [0, 2_000_000].
/// Both-zero vectors are distance 0 (the pinned "identical, if vacuous"
/// rule).
fn micros(a: &[f32], b: &[f32]) -> u32 {
    if a.iter().all(|&x| x == 0.0) && b.iter().all(|&x| x == 0.0) {
        return 0;
    }
    let mut dot = 0.0f64;
    for i in 0..a.len() {
        dot += f64::from(a[i]) * f64::from(b[i]);
    }
    let distance = (1.0 - dot) as f32;
    let scaled = (f64::from(distance) * 1_000_000.0 + 0.5).floor();
    scaled.clamp(0.0, 2_000_000.0) as u32
}

/// Test-local leave-one-out scores in micros.
fn loo_micros(vectors: &[Vec<f32>]) -> Vec<u32> {
    if vectors.len() < 2 {
        return Vec::new();
    }
    (0..vectors.len())
        .map(|i| {
            (0..vectors.len())
                .filter(|&j| j != i)
                .map(|j| micros(&vectors[i], &vectors[j]))
                .min()
                .unwrap()
        })
        .collect()
}

/// Test-local split-conformal quantile: the k-th smallest score,
/// k = ceil((n+1)(1000-alpha_milli)/1000), clamped to [1, n]; 0 for no
/// scores.
fn quantile_micros(scores: &[u32], alpha_milli: u32) -> u32 {
    if scores.is_empty() {
        return 0;
    }
    let mut sorted = scores.to_vec();
    sorted.sort_unstable();
    let n = sorted.len() as u64;
    let k = ((n + 1) * (1000 - u64::from(alpha_milli)))
        .div_ceil(1000)
        .clamp(1, n) as usize;
    sorted[k - 1]
}

// ---- the near-variant fixture (wave-4 style, spec/runtime.md §2) ----
//
// Each witness has a real neighbor one word away; disjoint witnesses would
// calibrate the threshold to the far quantile and admit everything, so a
// meaningful calibration needs neighbors — same reasoning as the toy e2e's
// DOC_B/DOC_D pair.
const DOC_A: &str = "Compilers translate agent cognition into fast deterministic binaries.";
const DOC_B: &str = "Compilers translate agent cognition into fast deterministic artifacts.";
const DOC_C: &str = "Compilers translate agent cognition into small deterministic binaries.";
/// One word swapped against DOC_A/DOC_C's shared frame — a near variant in
/// SPELLING, which is all the guard measures.
const NEAR_PROBE: &str = "Compilers translate agent cognition into fast deterministic binaries";
/// Shares no 3-byte window with any witness: lexically disjoint. (A
/// disjoint-vocabulary *paraphrase* of DOC_A would land here too — the
/// guard is lexical, not semantic, and that is the honest limit.)
const FAR_PROBE: &str = "zzz qqq vvv jjj xxx kkk www yyy";

fn witnesses() -> Vec<serde_json::Value> {
    vec![json!(DOC_A), json!(DOC_B), json!(DOC_C)]
}

// ---- featurizer, both ways ----

/// The production featurizer equals the independent one on the fixture
/// docs, and every non-degenerate vector is unit-norm at dim 256.
#[test]
fn featurizer_matches_independent_implementation_and_is_unit_norm() {
    for text in [DOC_A, DOC_B, DOC_C, NEAR_PROBE, FAR_PROBE, "", "ab", "abc"] {
        let production = trigram_embedding(text);
        assert_eq!(production, embed(text), "{text:?}");
        assert_eq!(production.len(), EMBEDDING_DIM);
        let norm_squared: f64 = production
            .iter()
            .map(|&x| f64::from(x) * f64::from(x))
            .sum();
        if text.len() >= 3 {
            assert!(
                (norm_squared - 1.0).abs() < 1e-6,
                "{text:?}: {norm_squared}"
            );
        } else {
            assert_eq!(norm_squared, 0.0, "{text:?} embeds to the zero vector");
        }
    }
}

// ---- calibration, both ways ----

/// The v2 threshold is the split-conformal leave-one-out quantile of the
/// cosine micros scores — computed both ways at every expressible-corner
/// alpha (max-quantile 1, default-ish 100/200, median 500, min-quantile
/// 999). Same rule as v1, same multiset shape, fixed point.
#[test]
fn v2_threshold_is_the_conformal_loo_quantile_computed_both_ways() {
    let vectors: Vec<Vec<f32>> = [DOC_A, DOC_B, DOC_C].iter().map(|d| embed(d)).collect();
    let scores = loo_micros(&vectors);
    assert_eq!(scores.len(), 3);
    // the fixture is meaningful: every witness has a genuinely near
    // neighbor (nonzero, well under the 1e6 micros of "nothing shared")
    for score in &scores {
        assert!(*score > 0 && *score < 500_000, "loo score {score}");
    }
    for alpha_milli in [1u32, 100, 200, 500, 999] {
        let guard = Guard::build_embedding(&witnesses(), None, alpha_milli).unwrap();
        let embedding = guard
            .embedding
            .as_ref()
            .expect("v2 guard carries the payload");
        assert_eq!(
            embedding.threshold_distance_micros(),
            quantile_micros(&scores, alpha_milli),
            "alpha_milli {alpha_milli}"
        );
        assert_eq!(embedding.alpha_milli(), alpha_milli);
    }
}

// ---- evaluation ----

/// An identical doc is at distance 0 exactly — through the fixed-point
/// boundary, not approximately.
#[test]
fn identical_doc_is_distance_zero() {
    let guard = Guard::build_embedding(&witnesses(), None, 100).unwrap();
    for doc in [DOC_A, DOC_B, DOC_C] {
        match guard.evaluate(&json!(doc)) {
            GuardOutcome::Proceed { distance, .. } => assert_eq!(distance, 0.0, "{doc:?}"),
            other => panic!("expected Proceed for {doc:?}, got {other:?}"),
        }
    }
}

/// The wave-4-style behavioral pair, with distances pinned against the
/// independent implementation: the near-variant probe admits (its distance
/// to DOC_A is far under every witness's leave-one-out score), the
/// disjoint-vocabulary probe trips at any sane alpha. Lexical geometry
/// only: NEAR_PROBE is near because it *shares spelling* with DOC_A.
#[test]
fn near_variant_admits_and_disjoint_vocabulary_trips() {
    let vectors: Vec<Vec<f32>> = [DOC_A, DOC_B, DOC_C].iter().map(|d| embed(d)).collect();
    let near_micros = vectors
        .iter()
        .map(|w| micros(&embed(NEAR_PROBE), w))
        .min()
        .unwrap();
    let far_micros = vectors
        .iter()
        .map(|w| micros(&embed(FAR_PROBE), w))
        .min()
        .unwrap();

    for alpha_milli in [1u32, 100, 500, 999] {
        let guard = Guard::build_embedding(&witnesses(), None, alpha_milli).unwrap();
        let threshold = guard
            .embedding
            .as_ref()
            .unwrap()
            .threshold_distance_micros();
        // the probes sit where the independent computation says they sit
        assert!(
            near_micros <= threshold,
            "alpha {alpha_milli}: {near_micros} vs {threshold}"
        );
        assert!(
            far_micros > threshold,
            "alpha {alpha_milli}: {far_micros} vs {threshold}"
        );

        match guard.evaluate(&json!(NEAR_PROBE)) {
            GuardOutcome::Proceed {
                distance,
                threshold: t,
            } => {
                assert_eq!(distance, f64::from(near_micros) / 1e6);
                assert_eq!(t, f64::from(threshold) / 1e6);
            }
            other => panic!("expected Proceed at alpha {alpha_milli}, got {other:?}"),
        }
        match guard.evaluate(&json!(FAR_PROBE)) {
            GuardOutcome::Trip {
                distance: Some(d),
                threshold: t,
                reason,
            } => {
                assert_eq!(d, f64::from(far_micros) / 1e6);
                assert_eq!(t, f64::from(threshold) / 1e6);
                assert_eq!(reason, "distance beyond calibration");
            }
            other => panic!("expected Trip at alpha {alpha_milli}, got {other:?}"),
        }
    }
}

/// Field mode plus fail-closed shapes: no text, no admission — v2 keeps
/// v0/v1's total-evaluation rule.
#[test]
fn v2_wrong_shape_inputs_trip_with_no_distance() {
    let inputs: Vec<serde_json::Value> = [DOC_A, DOC_B]
        .iter()
        .map(|d| json!({ "text": d }))
        .collect();
    let guard = Guard::build_embedding(&inputs, Some("text"), 100).unwrap();
    assert!(matches!(
        guard.evaluate(&json!({ "text": DOC_A })),
        GuardOutcome::Proceed { .. }
    ));
    for bad in [
        json!("bare"),
        json!({ "other": "x" }),
        json!({ "text": 3 }),
        json!(9),
    ] {
        match guard.evaluate(&bad) {
            GuardOutcome::Trip {
                distance: None,
                reason,
                ..
            } => {
                assert!(
                    reason.starts_with("input has no text to guard on"),
                    "{reason}"
                );
            }
            other => panic!("expected no-distance Trip on {bad}, got {other:?}"),
        }
    }
}

/// The zero-vector rule mirrors v1's empty-sketch quirk, stated not hidden:
/// a sub-3-byte witness embeds to the zero vector, every sub-3-byte input
/// matches it at distance 0, and any embeddable input is at exactly 1.0.
#[test]
fn trigramless_witness_matches_only_trigramless_inputs() {
    let guard = Guard::build_embedding(&[json!("ab")], None, 100).unwrap();
    assert_eq!(
        guard
            .embedding
            .as_ref()
            .unwrap()
            .threshold_distance_micros(),
        0
    );
    for near in ["ab", "", "xy"] {
        assert!(
            matches!(guard.evaluate(&json!(near)), GuardOutcome::Proceed { distance, .. } if distance == 0.0),
            "{near:?}"
        );
    }
    assert!(matches!(
        guard.evaluate(&json!("abc")),
        GuardOutcome::Trip { distance: Some(d), .. } if d == 1.0
    ));
}

// ---- wire: v2 round trip, v0/v1 byte-compatibility re-pins ----

#[test]
fn v2_wire_round_trips_byte_identically_and_verdicts_survive() {
    let inputs: Vec<serde_json::Value> = [DOC_B, DOC_A, DOC_A, DOC_C]
        .iter()
        .map(|d| json!({ "q": d }))
        .collect();
    let built = Guard::build_embedding(&inputs, Some("q"), 200).unwrap();
    // docs deduplicated and sorted on the wire
    assert_eq!(built.embedding.as_ref().unwrap().docs().len(), 3);
    let text = built.to_json();
    assert!(text.contains("\"guard_version\":2"), "{text}");
    assert!(
        text.contains("\"method\":\"trigram_hash_cosine\""),
        "{text}"
    );
    assert!(text.contains("\"dim\":256"), "{text}");
    assert!(text.contains("\"alpha_milli\":200"), "{text}");
    assert!(text.contains("\"scores_n\":3"), "{text}");

    let parsed = Guard::from_json(&text).unwrap();
    assert_eq!(parsed, built);
    assert_eq!(parsed.to_json(), text);
    for probe in [
        json!({ "q": DOC_A }),
        json!({ "q": NEAR_PROBE }),
        json!({ "q": FAR_PROBE }),
        json!(9),
    ] {
        assert_eq!(parsed.evaluate(&probe), built.evaluate(&probe));
    }
}

/// v0 and v1 documents built from real sketches read and re-emit
/// byte-identically after v2 exists — never upgraded, never rewritten.
/// (Read-only re-pin of the ADR-0014 golden shape, guarding against v2
/// leaking into old wire.)
#[test]
fn v0_and_v1_documents_reemit_byte_identically_never_as_v2() {
    let sketch: Vec<u32> = auto_model::trigram_hashes("hello world");
    let sketch_json = format!(
        "[{}]",
        sketch
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",")
    );

    let v0 = format!(
        "{{\"guard_version\":0,\"kind\":\"trigram_jaccard_nn\",\"threshold\":0.25,\
         \"witnesses\":[{sketch_json}]}}"
    );
    let v1 = format!(
        "{{\"calibration\":{{\"alpha_milli\":100,\"method\":\"split_conformal\",\
         \"scores_n\":0}},\"guard_version\":1,\"kind\":\"trigram_jaccard_nn\",\
         \"threshold\":0.25,\"witnesses\":[{sketch_json}]}}"
    );
    for doc in [v0, v1] {
        let guard = Guard::from_json(&doc).unwrap();
        assert_eq!(guard.to_json(), doc);
        assert!(guard.embedding.is_none());
        assert!(!guard.to_json().contains("embedding"));
        // and evaluation is still the v0/v1 Jaccard path
        assert!(matches!(
            guard.evaluate(&json!("hello world")),
            GuardOutcome::Proceed { distance, .. } if distance == 0.0
        ));
    }
}

// ---- malformed v2: loud refusals, never a fallback ----

/// A valid v2 embedding body / doc, for tests to mutate.
fn embedding_body(method: &str, dim: u32, threshold: &str, calibration: &str) -> String {
    format!(
        "{{\"calibration\":{calibration},\"dim\":{dim},\"method\":\"{method}\",\
         \"threshold_distance_micros\":{threshold}}}"
    )
}

fn v2_doc(embedding: &str, witnesses: &str) -> String {
    format!("{{\"embedding\":{embedding},\"guard_version\":2,\"witnesses\":{witnesses}}}")
}

const CAL_N2: &str = "{\"alpha_milli\":100,\"method\":\"split_conformal\",\"scores_n\":2}";
const TWO_DOCS: &str = "[\"alpha doc two\",\"beta doc one\"]";

#[test]
fn the_valid_v2_fixture_parses_and_the_mutations_below_start_valid() {
    let good = v2_doc(
        &embedding_body("trigram_hash_cosine", 256, "40000", CAL_N2),
        TWO_DOCS,
    );
    let guard = Guard::from_json(&good).unwrap();
    assert_eq!(guard.to_json(), good);
    assert_eq!(
        guard
            .embedding
            .as_ref()
            .unwrap()
            .threshold_distance_micros(),
        40_000
    );
}

#[test]
fn v2_rejects_unknown_embedding_method() {
    // a semantic method name must refuse loudly — never silently fall back
    // to Jaccard, never pretend minilm is available offline
    let doc = v2_doc(
        &embedding_body("minilm_cosine", 256, "40000", CAL_N2),
        TWO_DOCS,
    );
    match Guard::from_json(&doc) {
        Err(auto_runtime::GuardError::BadJson(detail)) => {
            assert!(detail.contains("minilm_cosine"), "{detail}");
            assert!(detail.contains("trigram_hash_cosine"), "{detail}");
        }
        other => panic!("expected BadJson, got {other:?}"),
    }
}

#[test]
fn v2_rejects_wrong_dim() {
    let doc = v2_doc(
        &embedding_body("trigram_hash_cosine", 512, "40000", CAL_N2),
        TWO_DOCS,
    );
    match Guard::from_json(&doc) {
        Err(auto_runtime::GuardError::BadJson(detail)) => {
            assert!(detail.contains("512"), "{detail}");
            assert!(detail.contains("256"), "{detail}");
        }
        other => panic!("expected BadJson, got {other:?}"),
    }
}

#[test]
fn v2_rejects_missing_or_non_integer_threshold() {
    // missing
    let missing = v2_doc(
        &format!("{{\"calibration\":{CAL_N2},\"dim\":256,\"method\":\"trigram_hash_cosine\"}}"),
        TWO_DOCS,
    );
    // float and negative: the wire is u32 micros, no f64 sneaks in
    let float = v2_doc(
        &embedding_body("trigram_hash_cosine", 256, "0.5", CAL_N2),
        TWO_DOCS,
    );
    let negative = v2_doc(
        &embedding_body("trigram_hash_cosine", 256, "-1", CAL_N2),
        TWO_DOCS,
    );
    for doc in [missing, float, negative] {
        assert!(
            matches!(
                Guard::from_json(&doc),
                Err(auto_runtime::GuardError::BadJson(_))
            ),
            "{doc}"
        );
    }
}

#[test]
fn v2_rejects_threshold_above_the_cosine_ceiling() {
    let doc = v2_doc(
        &embedding_body("trigram_hash_cosine", 256, "2000001", CAL_N2),
        TWO_DOCS,
    );
    match Guard::from_json(&doc) {
        Err(auto_runtime::GuardError::BadJson(detail)) => {
            assert!(detail.contains("2000001"), "{detail}");
        }
        other => panic!("expected BadJson, got {other:?}"),
    }
}

#[test]
fn v2_rejects_empty_witnesses() {
    let cal0 = "{\"alpha_milli\":100,\"method\":\"split_conformal\",\"scores_n\":0}";
    let doc = v2_doc(&embedding_body("trigram_hash_cosine", 256, "0", cal0), "[]");
    assert!(matches!(
        Guard::from_json(&doc),
        Err(auto_runtime::GuardError::NoWitnesses)
    ));
}

#[test]
fn v2_rejects_unsorted_or_duplicated_docs() {
    for (witnesses, what) in [
        ("[\"beta doc one\",\"alpha doc two\"]", "unsorted"),
        ("[\"alpha doc two\",\"alpha doc two\"]", "duplicated"),
    ] {
        let doc = v2_doc(
            &embedding_body("trigram_hash_cosine", 256, "40000", CAL_N2),
            witnesses,
        );
        match Guard::from_json(&doc) {
            Err(auto_runtime::GuardError::BadWitness { detail }) => {
                assert!(detail.contains(what), "{detail}");
            }
            other => panic!("expected BadWitness ({what}), got {other:?}"),
        }
    }
}

#[test]
fn v2_rejects_bad_calibration() {
    // wrong method
    let wrong_method = v2_doc(
        &embedding_body(
            "trigram_hash_cosine",
            256,
            "40000",
            "{\"alpha_milli\":100,\"method\":\"full_conformal\",\"scores_n\":2}",
        ),
        TWO_DOCS,
    );
    assert!(matches!(
        Guard::from_json(&wrong_method),
        Err(auto_runtime::GuardError::BadJson(_))
    ));
    // alpha at the exclusive bounds
    for alpha in [0u32, 1000] {
        let doc = v2_doc(
            &embedding_body(
                "trigram_hash_cosine",
                256,
                "40000",
                &format!(
                    "{{\"alpha_milli\":{alpha},\"method\":\"split_conformal\",\"scores_n\":2}}"
                ),
            ),
            TWO_DOCS,
        );
        match Guard::from_json(&doc) {
            Err(auto_runtime::GuardError::BadAlpha(a)) => assert_eq!(a, alpha),
            other => panic!("expected BadAlpha({alpha}), got {other:?}"),
        }
    }
    // scores_n disagreeing with the doc count
    let mismatch = v2_doc(
        &embedding_body(
            "trigram_hash_cosine",
            256,
            "40000",
            "{\"alpha_milli\":100,\"method\":\"split_conformal\",\"scores_n\":7}",
        ),
        TWO_DOCS,
    );
    match Guard::from_json(&mismatch) {
        Err(auto_runtime::GuardError::BadJson(detail)) => {
            assert!(detail.contains("scores_n 7"), "{detail}");
            assert!(detail.contains("expected 2"), "{detail}");
        }
        other => panic!("expected BadJson, got {other:?}"),
    }
}

#[test]
fn v2_rejects_unknown_fields_null_embedding_and_v1_shapes() {
    let good_embedding = embedding_body("trigram_hash_cosine", 256, "40000", CAL_N2);
    let cases = [
        // unknown top-level field
        format!(
            "{{\"embedding\":{good_embedding},\"extra\":1,\"guard_version\":2,\
             \"witnesses\":{TWO_DOCS}}}"
        ),
        // unknown field inside embedding
        v2_doc(
            "{\"calibration\":{\"alpha_milli\":100,\"method\":\"split_conformal\",\
             \"scores_n\":2},\"dim\":256,\"extra\":1,\"method\":\"trigram_hash_cosine\",\
             \"threshold_distance_micros\":40000}",
            TWO_DOCS,
        ),
        // null / missing embedding object
        format!("{{\"embedding\":null,\"guard_version\":2,\"witnesses\":{TWO_DOCS}}}"),
        format!("{{\"guard_version\":2,\"witnesses\":{TWO_DOCS}}}"),
        // the v1 field name: the v2 wire pins `field`, not `input_field`
        format!(
            "{{\"embedding\":{good_embedding},\"guard_version\":2,\"input_field\":\"q\",\
             \"witnesses\":{TWO_DOCS}}}"
        ),
        // v0/v1-style hash-sketch witnesses under v2
        v2_doc(&good_embedding, "[[1,2],[3,4]]"),
    ];
    for doc in cases {
        assert!(
            matches!(
                Guard::from_json(&doc),
                Err(auto_runtime::GuardError::BadJson(_))
            ),
            "{doc}"
        );
    }
}
