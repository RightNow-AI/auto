//! Pure decode + policy logic for the napi surface — free of every napi type,
//! so it unit-tests without a Node runtime (the FFI glue in `bindings` is the
//! only part that needs one, and it is feature-gated and cfg'd out of the
//! test binary).
//!
//! This is a deliberate small duplication of the auto-py twin
//! (`crates/auto-py/src/logic.rs`, ADR-0024) rather than a dependency on it:
//! depending on the pyo3 bindings crate would drag CPython FFI into this
//! crate's build graph for nothing. Two twins, one contract — both decode the
//! SAME `auto_runtime::Runner::answer` envelope (spec/runtime.md §9), and
//! their unit tests pin the same shapes. Divergences: v0 here is PURE-ONLY
//! (the frozen ADR-0024 refusal below; auto-py's `tools=` table, ADR-0027, is
//! a recorded follow-up for this twin), so there is no `tool_table_mismatch`.
//!
//! Two responsibilities:
//!
//! - [`capability_refusal_message`] — the LOAD-time gate. Pure artifacts
//!   load; a capability-bearing artifact is refused with the frozen ADR-0024
//!   v0 message, at load, not as a surprise at call time.
//! - [`decode_answer`] — map an `auto_runtime::Runner::answer` envelope
//!   (spec/runtime.md §9: `{"output"}` | `{"abstained",…}` | `{"error"}`) to
//!   the three outcomes the binding turns into a return value, a thrown
//!   error with `code === "AutoAbstained"` (message + structured fields),
//!   and a thrown error with `code === "AutoError"`.

use auto_trace::model::canonical_json;
use serde_json::{Map, Value};

/// The LOAD gate (ADR-0024 decision 4, mirrored for the twin per ADR-0026).
/// Returns the refusal message for a capability-bearing artifact (nonempty
/// declared capabilities), or `None` for a pure artifact that may load. The
/// message is the frozen ADR-0024 v0 pattern — the same loud, honest refusal
/// auto-py shipped with, naming what is recorded rather than a vague
/// "unsupported". (`auto_runtime::Runner::new` would itself refuse through
/// the loader, but with a generic missing-tools message; we refuse first,
/// with the honest reason.)
pub fn capability_refusal_message(capabilities: &[String]) -> Option<String> {
    if capabilities.is_empty() {
        None
    } else {
        Some(
            "capability artifacts are not supported embedded in v0 \
             (recorded: per-request tool policy + host callbacks)"
                .to_owned(),
        )
    }
}

/// One decoded abstention: the composed human message plus the raw guard
/// fields, so the binding exposes `reason`/`distance`/`threshold` as
/// structured properties on the thrown `AutoAbstained`-coded error without
/// re-parsing its own message. Twin of auto-py's `logic::Abstention`.
#[derive(Debug, Clone, PartialEq)]
pub struct Abstention {
    /// human-readable guard detail — the thrown error's `message`
    pub message: String,
    /// the guard's stated reason; `None` only for a malformed envelope
    pub reason: Option<String>,
    /// measured distance; `None` for a wrong-shaped input with no text to
    /// measure (the runner emits `distance: null`)
    pub distance: Option<f64>,
    /// the calibrated threshold; `None` only for a malformed envelope
    pub threshold: Option<f64>,
}

/// The outcome decoded from one `Runner::answer` line.
#[derive(Debug, PartialEq)]
pub enum Decoded {
    /// tier-1 produced this output value, rendered as canonical JSON text
    Output(String),
    /// the guard tripped; message plus the structured guard fields
    Abstained(Abstention),
    /// load / parse / execution error; this is the detail
    Error(String),
}

/// Decode a `Runner::answer` line. `Runner::answer` is contracted to emit a
/// JSON OBJECT of exactly one of three shapes; anything else (which this
/// build of the runner never produces) is surfaced as a [`Decoded::Error`]
/// rather than a panic — the binding must never crash the host Node process.
pub fn decode_answer(envelope: &str) -> Decoded {
    let value: Value = match serde_json::from_str(envelope) {
        Ok(value) => value,
        Err(e) => return Decoded::Error(format!("runner produced non-JSON output: {e}")),
    };
    let Some(object) = value.as_object() else {
        return Decoded::Error(format!("runner envelope is not a JSON object: {envelope}"));
    };
    // Precedence mirrors runner.rs `answer_value`: output, then abstain, then
    // error. Exactly one key is ever present, so the order only matters as a
    // defensive tie-break.
    if let Some(output) = object.get("output") {
        return Decoded::Output(canonical_json(output));
    }
    if object.get("abstained").and_then(Value::as_bool) == Some(true) {
        return Decoded::Abstained(decode_abstention(object));
    }
    if let Some(error) = object.get("error") {
        let detail = error
            .as_str()
            .map_or_else(|| canonical_json(error), str::to_owned);
        return Decoded::Error(detail);
    }
    Decoded::Error(format!("unrecognized runner envelope: {envelope}"))
}

/// Decode the abstention envelope: raw fields plus one composed human string
/// carrying the guard detail — the reason, the measured distance (absent —
/// `null` — for a wrong-shaped input with no text to measure), and the
/// calibrated threshold. Byte-identical composition to the auto-py twin, so
/// the two embeddings report one abstention the same way.
fn decode_abstention(object: &Map<String, Value>) -> Abstention {
    let reason = object
        .get("reason")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let shown_reason = reason.as_deref().unwrap_or("guard tripped");
    let shown_threshold = match object.get("threshold") {
        Some(threshold) => threshold.to_string(),
        None => "unknown".to_owned(),
    };
    let message = match object.get("distance") {
        Some(distance) if !distance.is_null() => {
            format!("{shown_reason} (distance {distance}, threshold {shown_threshold})")
        }
        _ => format!("{shown_reason} (no measurable distance; threshold {shown_threshold})"),
    };
    Abstention {
        message,
        reason,
        distance: object.get("distance").and_then(Value::as_f64),
        threshold: object.get("threshold").and_then(Value::as_f64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- the load gate (pure-only v0) ----

    #[test]
    fn pure_artifact_is_not_refused() {
        assert_eq!(capability_refusal_message(&[]), None);
    }

    #[test]
    fn capability_artifact_refusal_is_the_frozen_adr_0024_message() {
        let message = capability_refusal_message(&["lookup".to_owned()])
            .expect("a capability artifact is refused at load");
        // the frozen ADR-0024 v0 pattern, verbatim — the refusal names what
        // is recorded, not a vague "unsupported"
        assert_eq!(
            message,
            "capability artifacts are not supported embedded in v0 \
             (recorded: per-request tool policy + host callbacks)"
        );
    }

    #[test]
    fn multiple_capabilities_still_refuse() {
        assert!(capability_refusal_message(&["a".to_owned(), "b".to_owned()]).is_some());
    }

    // ---- envelope decode (same contract-pinning shapes as the auto-py twin) ----

    #[test]
    fn output_object_decodes_to_canonical_text() {
        // keys come back sorted (canonical), byte-identical to what serve emits
        assert_eq!(
            decode_answer(r#"{"output":{"b":2,"a":1}}"#),
            Decoded::Output(r#"{"a":1,"b":2}"#.to_owned())
        );
    }

    #[test]
    fn output_string_scalar_and_null_decode() {
        assert_eq!(
            decode_answer(r#"{"output":"hi"}"#),
            Decoded::Output(r#""hi""#.to_owned())
        );
        assert_eq!(
            decode_answer(r#"{"output":42}"#),
            Decoded::Output("42".to_owned())
        );
        assert_eq!(
            decode_answer(r#"{"output":null}"#),
            Decoded::Output("null".to_owned())
        );
    }

    #[test]
    fn abstention_with_distance_carries_message_and_fields() {
        let envelope = json!({
            "abstained": true,
            "reason": "distance beyond calibration",
            "distance": 0.5,
            "threshold": 0.25
        })
        .to_string();
        match decode_answer(&envelope) {
            Decoded::Abstained(abstention) => {
                assert!(
                    abstention.message.contains("distance beyond calibration"),
                    "{}",
                    abstention.message
                );
                assert!(abstention.message.contains("0.5"), "{}", abstention.message);
                assert!(
                    abstention.message.contains("0.25"),
                    "{}",
                    abstention.message
                );
                // the structured fields the binding exposes as properties
                assert_eq!(
                    abstention.reason.as_deref(),
                    Some("distance beyond calibration")
                );
                assert_eq!(abstention.distance, Some(0.5));
                assert_eq!(abstention.threshold, Some(0.25));
            }
            other => panic!("expected Abstained, got {other:?}"),
        }
    }

    #[test]
    fn abstention_without_distance_has_a_none_distance() {
        // wrong-shaped input: runner emits distance: null (guard.rs Trip{distance: None})
        let envelope = json!({
            "abstained": true,
            "reason": "input has no text to guard on (an object, not a string)",
            "distance": null,
            "threshold": 0.0
        })
        .to_string();
        match decode_answer(&envelope) {
            Decoded::Abstained(abstention) => {
                assert!(
                    abstention.message.contains("no text to guard on"),
                    "{}",
                    abstention.message
                );
                assert!(
                    abstention.message.contains("no measurable distance"),
                    "{}",
                    abstention.message
                );
                assert_eq!(abstention.distance, None);
                assert_eq!(abstention.threshold, Some(0.0));
            }
            other => panic!("expected Abstained, got {other:?}"),
        }
    }

    #[test]
    fn error_object_decodes_to_its_detail() {
        assert_eq!(
            decode_answer(r#"{"error":"tier-1 execution failed: trap"}"#),
            Decoded::Error("tier-1 execution failed: trap".to_owned())
        );
    }

    #[test]
    fn non_json_is_an_error_not_a_panic() {
        match decode_answer("this is not json") {
            Decoded::Error(detail) => assert!(detail.contains("non-JSON"), "{detail}"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn non_object_json_is_an_error() {
        match decode_answer("[1,2,3]") {
            Decoded::Error(detail) => assert!(detail.contains("not a JSON object"), "{detail}"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn empty_object_is_an_unrecognized_error() {
        match decode_answer("{}") {
            Decoded::Error(detail) => assert!(detail.contains("unrecognized"), "{detail}"),
            other => panic!("expected Error, got {other:?}"),
        }
    }
}
