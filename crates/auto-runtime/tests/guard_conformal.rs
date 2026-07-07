//! Guard wire v1 (split-conformal calibration) through the public API.
//!
//! The load-bearing proofs live here, computed BOTH ways — the production
//! path versus an independent test-local implementation of Jaccard distance
//! and the leave-one-out maximum:
//!
//! - v0 documents still parse, evaluate identically, and re-serialize
//!   byte-identically (the compatibility pin);
//! - for small witness sets (n <= 9 at alpha 0.1, and `Guard::build`'s
//!   alpha 0.001 at any tested n) the conformal threshold IS the v0
//!   leave-one-out max;
//! - for a 50-witness set with a fully known score distribution at
//!   alpha 0.2, the conformal quantile sits strictly below the max and a
//!   witness-like input still proceeds.

use auto_runtime::guard::Calibration;
use auto_runtime::{Guard, GuardOutcome};
use auto_trace::model::canonical_json;
use serde_json::json;

/// Test-local Jaccard set distance over sorted, deduplicated sketches —
/// independent of the production implementation.
fn jaccard(a: &[u32], b: &[u32]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let inter = a.iter().filter(|x| b.binary_search(x).is_ok()).count();
    let union = a.len() + b.len() - inter;
    1.0 - (inter as f64) / (union as f64)
}

/// Test-local v0 calibration: the maximum over witnesses of the distance to
/// their nearest other witness; 0.0 for a lone witness.
fn loo_max(sketches: &[Vec<u32>]) -> f64 {
    if sketches.len() < 2 {
        return 0.0;
    }
    sketches
        .iter()
        .enumerate()
        .map(|(i, s)| {
            sketches
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, o)| jaccard(s, o))
                .fold(f64::INFINITY, f64::min)
        })
        .fold(0.0, f64::max)
}

/// JSON array text of one sketch, e.g. `[1,2,3]`.
fn sketch_json(sketch: &[u32]) -> String {
    format!(
        "[{}]",
        sketch
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",")
    )
}

// ---- v0 compatibility pin ----

/// A v0 document built from real sketches parses, evaluates with unchanged
/// v0 semantics, and re-serializes to the exact input bytes.
#[test]
fn v0_fixture_parses_evaluates_and_reserializes_byte_identically() {
    let sketches: Vec<Vec<u32>> = ["abcd", "abcde", "bcdef"]
        .iter()
        .map(|t| auto_model::trigram_hashes(t))
        .collect();
    let fixture = format!(
        "{{\"guard_version\":0,\"kind\":\"trigram_jaccard_nn\",\"threshold\":0.5,\
         \"witnesses\":[{},{},{}]}}",
        sketch_json(&sketches[0]),
        sketch_json(&sketches[1]),
        sketch_json(&sketches[2]),
    );

    let guard = Guard::from_json(&fixture).unwrap();
    assert_eq!(guard.calibration, Calibration::LeaveOneOutMax);
    assert_eq!(guard.threshold, 0.5);
    assert_eq!(guard.input_field, None);

    // v0 evaluation semantics, hand-computed (same values as the v0 tests)
    assert_eq!(
        guard.evaluate(&json!("abcd")),
        GuardOutcome::Proceed {
            distance: 0.0,
            threshold: 0.5
        }
    );
    assert_eq!(
        guard.evaluate(&json!("bcde")),
        GuardOutcome::Proceed {
            distance: 1.0 - 2.0 / 3.0,
            threshold: 0.5
        }
    );
    assert_eq!(
        guard.evaluate(&json!("zzzz qqqq")),
        GuardOutcome::Trip {
            reason: "distance beyond calibration".to_owned(),
            distance: Some(1.0),
            threshold: 0.5
        }
    );
    assert!(matches!(
        guard.evaluate(&json!(9)),
        GuardOutcome::Trip { distance: None, .. }
    ));

    // byte-identical: a v0 guard never re-serializes with an invented alpha
    assert_eq!(guard.to_json(), fixture);
}

// ---- small-n equivalence proof ----

/// For n <= 9 witnesses at alpha_milli 100, the split-conformal threshold
/// equals the v0 leave-one-out max — computed both ways: production
/// `build_conformal` versus the test-local `loo_max`. `Guard::build`
/// (alpha_milli 1) matches too. Small-n behavior is unchanged from v0.
#[test]
fn small_n_conformal_threshold_equals_leave_one_out_max() {
    let base = "abcdefghijklmnopqrst";
    for n in 1..=9usize {
        let texts: Vec<&str> = (0..n).map(|i| &base[i..i + 4 + (i % 3)]).collect();
        let inputs: Vec<serde_json::Value> = texts.iter().map(|t| json!(t)).collect();
        let sketches: Vec<Vec<u32>> = texts
            .iter()
            .map(|t| auto_model::trigram_hashes(t))
            .collect();
        let expected = loo_max(&sketches);

        let conformal = Guard::build_conformal(&inputs, None, 100).unwrap();
        assert_eq!(
            conformal.threshold, expected,
            "n = {n}: conformal(alpha 0.1) != leave-one-out max"
        );
        let built = Guard::build(&inputs, None).unwrap();
        assert_eq!(
            built.threshold, expected,
            "n = {n}: build (alpha 0.001) != leave-one-out max"
        );
    }
}

// ---- 50 witnesses with a known score distribution ----

/// A character from a per-pair-disjoint CJK block (no case mapping, so the
/// trigram rule's lowercasing is the identity).
fn pair_char(pair: u32, offset: u32) -> char {
    char::from_u32(0x4E00 + pair * 64 + offset).expect("CJK block char")
}

/// 25 disjoint pairs of texts. Pair p: text A is 52 distinct chars (50
/// distinct trigrams); text B is A's first 52-(p+1) chars, so B's sketch is
/// a subset of A's and d(A, B) = (p+1)/50 exactly. Across pairs the
/// alphabets are disjoint, so cross-pair distance is 1.0 and every text's
/// nearest other witness is its partner. The 50 leave-one-out scores are
/// therefore each (p+1)/50, twice — fully known.
///
/// At alpha_milli 200: k = ceil(51 * 0.8) = 41, the 41st smallest score =
/// 21/50 = 0.42 — strictly below the leave-one-out max of 25/50 = 0.5. A
/// near-witness input still proceeds; a fresh-alphabet input still trips.
#[test]
fn fifty_witnesses_alpha_200_quantile_sits_strictly_below_the_max() {
    let mut texts = Vec::new();
    let mut expected_scores = Vec::new();
    for p in 0..25u32 {
        let a: String = (0..52).map(|o| pair_char(p, o)).collect();
        let j = (p + 1) as usize;
        let b: String = a.chars().take(52 - j).collect();

        // construction check (also catches any trigram-hash collision)
        let sketch_a = auto_model::trigram_hashes(&a);
        let sketch_b = auto_model::trigram_hashes(&b);
        assert_eq!(sketch_a.len(), 50, "pair {p}: A must have 50 trigrams");
        assert_eq!(sketch_b.len(), 50 - j, "pair {p}: B must subset A");
        let d = jaccard(&sketch_a, &sketch_b);
        assert!(
            (d - (j as f64) / 50.0).abs() < 1e-12,
            "pair {p}: d = {d}, expected {}",
            (j as f64) / 50.0
        );

        texts.push(a);
        texts.push(b);
        expected_scores.push(d);
        expected_scores.push(d);
    }
    expected_scores.sort_by(f64::total_cmp);
    let expected_threshold = expected_scores[40]; // k = ceil(51*0.8) = 41
    let expected_max = expected_scores[49];
    assert!((expected_threshold - 0.42).abs() < 1e-12);
    assert!((expected_max - 0.5).abs() < 1e-12);

    let inputs: Vec<serde_json::Value> = texts.iter().map(|t| json!(t)).collect();
    let guard = Guard::build_conformal(&inputs, None, 200).unwrap();

    // the quantile, computed both ways — and strictly below the v0 max
    assert_eq!(guard.threshold, expected_threshold);
    assert!(guard.threshold < expected_max);
    let sketches: Vec<Vec<u32>> = texts
        .iter()
        .map(|t| auto_model::trigram_hashes(t))
        .collect();
    assert!(guard.threshold < loo_max(&sketches));
    assert_eq!(
        guard.calibration,
        Calibration::SplitConformal { alpha_milli: 200 }
    );

    // a witness-like input still passes: A_0 minus its first char shares
    // 49 of A_0's 50 trigrams -> distance 0.02 <= 0.42
    let near: String = texts[0].chars().skip(1).collect();
    match guard.evaluate(&json!(near)) {
        GuardOutcome::Proceed {
            distance,
            threshold,
        } => {
            assert!((distance - 0.02).abs() < 1e-12, "{distance}");
            assert_eq!(threshold, expected_threshold);
        }
        other => panic!("expected Proceed, got {other:?}"),
    }

    // a fresh-alphabet input is at distance 1.0: trip
    let far: String = (0..10).map(|o| pair_char(30, o)).collect();
    assert!(matches!(
        guard.evaluate(&json!(far)),
        GuardOutcome::Trip {
            distance: Some(d),
            ..
        } if d == 1.0
    ));

    // wire: v1, alpha and scores count carried; canonical round-trip
    let text = guard.to_json();
    assert!(text.contains("\"guard_version\":1"), "{text}");
    assert!(text.contains("\"alpha_milli\":200"), "{text}");
    assert!(text.contains("\"scores_n\":50"), "{text}");
    let parsed = Guard::from_json(&text).unwrap();
    assert_eq!(parsed, guard);
    assert_eq!(parsed.to_json(), text);
}

// ---- v1 canonical form ----

/// The v1 wire body equals the canonical JSON of its documented shape,
/// field for field.
#[test]
fn v1_wire_is_the_canonical_json_of_its_documented_shape() {
    let inputs = [json!({"q": "abcd"}), json!({"q": "abcde"})];
    let guard = Guard::build_conformal(&inputs, Some("q"), 100).unwrap();
    // two witnesses at distance 1/3 of each other: scores {1/3, 1/3};
    // k = ceil(3 * 0.9) = 3 > 2 truncates to the max = 1/3
    let expected = json!({
        "calibration": {
            "alpha_milli": 100,
            "method": "split_conformal",
            "scores_n": 2,
        },
        "guard_version": 1,
        "input_field": "q",
        "kind": "trigram_jaccard_nn",
        "threshold": 1.0 - 2.0 / 3.0,
        "witnesses": [
            auto_model::trigram_hashes("abcd"),
            auto_model::trigram_hashes("abcde"),
        ],
    });
    assert_eq!(guard.to_json(), canonical_json(&expected));
}
