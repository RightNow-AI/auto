//! Pure decode + policy logic for the pyo3 surface — free of every pyo3 type,
//! so it unit-tests without a Python interpreter (the FFI glue in `bindings`
//! is the only part that needs one, and it is cfg'd out of the test binary).
//!
//! Four responsibilities:
//!
//! - [`capability_refusal_message`] — the LOAD-time gate when NO tools are
//!   provided. Pure artifacts load; a capability-bearing artifact is refused,
//!   naming the remedy (`tools=`, ADR-0027 — which superseded ADR-0024's v0
//!   "host callbacks are a recorded follow-up" refusal).
//! - [`tool_table_mismatch`] — the exactly-declared rule for a provided tool
//!   table (ADR-0017's loader rule, embedded per ADR-0027): the provided
//!   names must equal the declared capabilities — nothing missing, nothing
//!   extra, and a pure artifact takes no tools at all.
//! - [`budgeted`] + [`budget_on_pure_message`] — the per-answer tool-call
//!   budget (ADR-0032, mirroring ADR-0028's serve budget at the embedded
//!   dispatch seam): count executed host-callable invocations, refuse the
//!   `n+1`-th without invoking it, audit executed calls only; and the
//!   LOAD-time refusal of a budget on a pure artifact, which has nothing to
//!   bound.
//! - [`decode_answer`] — map an `auto_runtime::Runner::answer` envelope
//!   (spec/runtime.md §9: `{"output"}` | `{"abstained",…}` | `{"error"}`) to
//!   the three outcomes the binding turns into a return value,
//!   `AutoAbstained` (message + structured fields), and `AutoError`.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use auto_trace::model::canonical_json;
use serde_json::{Map, Value};

/// The no-tools LOAD gate. Returns the refusal message for a
/// capability-bearing artifact (nonempty declared capabilities), or `None`
/// for a pure artifact that may load. The refusal names the declared
/// capabilities and the remedy — the `tools=` mapping ADR-0027 built —
/// rather than a vague "unsupported".
pub fn capability_refusal_message(capabilities: &[String]) -> Option<String> {
    if capabilities.is_empty() {
        None
    } else {
        Some(format!(
            "artifact declares capabilities {capabilities:?}; pass tools= mapping every \
             declared capability to a callable (ADR-0027)"
        ))
    }
}

/// The exactly-declared rule for a PROVIDED tool table (ADR-0017's loader
/// rule, embedded per ADR-0027): `None` when `provided` covers `capabilities`
/// exactly; otherwise a message naming every missing capability and every
/// extra tool. A pure artifact with a nonempty table is the loader's
/// host-on-a-pure-artifact refusal, mirrored here with the honest wording.
/// (`provided` empty on a pure artifact is exact coverage of zero
/// capabilities: allowed, and the caller loads pure.)
pub fn tool_table_mismatch(capabilities: &[String], provided: &[String]) -> Option<String> {
    if capabilities.is_empty() {
        if provided.is_empty() {
            return None;
        }
        return Some(format!(
            "artifact declares no capabilities; tools must not be provided (ADR-0017: a tool \
             host must not be attached to a pure artifact); got {provided:?}"
        ));
    }
    let declared: BTreeSet<&str> = capabilities.iter().map(String::as_str).collect();
    let given: BTreeSet<&str> = provided.iter().map(String::as_str).collect();
    // BTreeSet difference iterates sorted, so the message is deterministic
    let missing: Vec<&&str> = declared.difference(&given).collect();
    let extra: Vec<&&str> = given.difference(&declared).collect();
    if missing.is_empty() && extra.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    if !missing.is_empty() {
        parts.push(format!(
            "missing tools for declared capabilities {missing:?}"
        ));
    }
    if !extra.is_empty() {
        parts.push(format!(
            "extra tools not declared as capabilities {extra:?}"
        ));
    }
    Some(format!(
        "tools must cover the declared capabilities exactly (ADR-0017, embedded per ADR-0027): {}",
        parts.join("; ")
    ))
}

/// The LOAD-time refusal for `max_tool_calls` on a PURE artifact (ADR-0032).
/// A budget bounds host-callable execution and a pure artifact loads with no
/// host — with `tools=None` and with the pure `tools={}` form alike — so
/// there is nothing to bound. Refusing loud beats silently accepting a
/// parameter that can never act: deliberately STRICTER than serve, where a
/// budget with no `--tool` table is vacuously satisfied because one
/// server-wide flag spans many artifacts; an embedded `Runner` holds exactly
/// one artifact, so a meaningless budget here is a caller bug (the ADR-0027
/// decision-3 embedded-strictness precedent).
pub fn budget_on_pure_message(budget: u64) -> String {
    format!(
        "max_tool_calls={budget} on a pure artifact: a tool budget needs tools= — a pure \
         artifact makes no tool calls, so there is nothing to bound (ADR-0032)"
    )
}

/// Wrap the binding's dispatch closure in a per-ANSWER tool-call budget +
/// audit (ADR-0032 — ADR-0028's serve budget, embedded). Per invocation:
/// once `calls` (EXECUTED invocations within the current answer) has reached
/// `budget`, refuse with `tool budget exceeded: {budget} per answer
/// (ADR-0032)` WITHOUT invoking `inner` — the artifact sees the `{"err"}`
/// envelope and traps honestly, which `.answer` surfaces as `AutoError`;
/// otherwise count it, log one audit line `tool audit: <name> call #<k>
/// (embedded)` to stderr, and invoke.
///
/// Only executed invocations are audited (ADR-0028 decision 5): a refused
/// call never ran, so an audit line would claim a side effect that did not
/// happen — the breach is carried by the err envelope and the raised
/// `AutoError`. The counter holds EXECUTED calls (an `inner` that returns
/// `Err` still executed and still counts), so `#k` is the true executed
/// index and a fresh answer's `store(0)` restores the whole budget.
///
/// `calls` is shared with the binding's `Runner`, which resets it to zero at
/// the top of every `.answer`, before the GIL is released. Counting takes no
/// lock and no GIL: the `HostTools::Callback` seam already serializes
/// invocations behind its mutex, so `Relaxed` suffices (ADR-0028's
/// reasoning), and a refused call is turned away BEFORE the dispatch closure
/// would attach to the interpreter — no Python runs for it. Stated hazard,
/// the embedded twin of ADR-0028 decision 3's sequential-server dependency:
/// the counter is per RUNNER, reset per answer, so the budget is exact only
/// while `.answer` calls on one `Runner` do not overlap; overlapping answers
/// from multiple Python threads mix their counts (per-answer isolation needs
/// per-execution host state — a runtime seam change, recorded, not built).
pub fn budgeted<F>(
    budget: u64,
    calls: Arc<AtomicU64>,
    mut inner: F,
) -> impl FnMut(&str, &Value) -> Result<Value, String> + Send + 'static
where
    F: FnMut(&str, &Value) -> Result<Value, String> + Send + 'static,
{
    move |name: &str, input: &Value| {
        let executed = calls.load(Ordering::Relaxed);
        if executed >= budget {
            return Err(format!(
                "tool budget exceeded: {budget} per answer (ADR-0032)"
            ));
        }
        let k = executed + 1;
        calls.store(k, Ordering::Relaxed);
        eprintln!("tool audit: {name} call #{k} (embedded)");
        inner(name, input)
    }
}

/// One decoded abstention: the composed human message (unchanged from the
/// message-only era — additive) plus the raw guard fields, so the binding
/// exposes `reason`/`distance`/`threshold` as structured attributes on
/// `AutoAbstained` (the ADR-0024 recorded follow-up, closed by ADR-0027)
/// without re-parsing its own message.
#[derive(Debug, Clone, PartialEq)]
pub struct Abstention {
    /// human-readable guard detail, exactly what the message-only era carried
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
/// JSON OBJECT of exactly one of three shapes; anything else (which this build
/// of the runner never produces) is surfaced as an [`Decoded::Error`] rather
/// than a panic — the binding must never crash the host interpreter.
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
/// calibrated threshold.
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

    // ---- the no-tools load gate ----

    #[test]
    fn pure_artifact_is_not_refused() {
        assert_eq!(capability_refusal_message(&[]), None);
    }

    #[test]
    fn capability_artifact_refusal_names_capabilities_and_remedy() {
        let message = capability_refusal_message(&["lookup".to_owned()])
            .expect("a capability artifact without tools is refused at load");
        assert!(message.contains("lookup"), "{message}");
        // the refusal names the remedy, not a vague "unsupported"
        assert!(message.contains("tools="), "{message}");
        assert!(message.contains("ADR-0027"), "{message}");
    }

    #[test]
    fn multiple_capabilities_refusal_names_them_all() {
        let message =
            capability_refusal_message(&["a".to_owned(), "b".to_owned()]).expect("refused");
        assert!(message.contains('a') && message.contains('b'), "{message}");
    }

    // ---- the exactly-declared rule for a provided table ----

    #[test]
    fn exact_coverage_is_accepted() {
        assert_eq!(
            tool_table_mismatch(
                &["a".to_owned(), "b".to_owned()],
                &["b".to_owned(), "a".to_owned()]
            ),
            None
        );
    }

    #[test]
    fn empty_table_on_a_pure_artifact_is_exact() {
        assert_eq!(tool_table_mismatch(&[], &[]), None);
    }

    #[test]
    fn missing_capability_is_named() {
        let message = tool_table_mismatch(
            &["lookup".to_owned(), "fetch".to_owned()],
            &["lookup".to_owned()],
        )
        .expect("missing tool refuses");
        assert!(message.contains("missing"), "{message}");
        assert!(message.contains("fetch"), "{message}");
        assert!(!message.contains("extra"), "{message}");
    }

    #[test]
    fn extra_tool_is_named() {
        let message = tool_table_mismatch(
            &["lookup".to_owned()],
            &["lookup".to_owned(), "sneaky".to_owned()],
        )
        .expect("extra tool refuses");
        assert!(message.contains("extra"), "{message}");
        assert!(message.contains("sneaky"), "{message}");
        assert!(!message.contains("missing"), "{message}");
    }

    #[test]
    fn missing_and_extra_are_both_named() {
        let message = tool_table_mismatch(&["lookup".to_owned()], &["other".to_owned()])
            .expect("mismatched table refuses");
        assert!(message.contains("lookup"), "{message}");
        assert!(message.contains("other"), "{message}");
        assert!(message.contains("missing"), "{message}");
        assert!(message.contains("extra"), "{message}");
    }

    #[test]
    fn nonempty_table_on_a_pure_artifact_is_the_loader_rule() {
        let message = tool_table_mismatch(&[], &["lookup".to_owned()])
            .expect("a pure artifact takes no tools");
        assert!(message.contains("must not"), "{message}");
        assert!(message.contains("ADR-0017"), "{message}");
        assert!(message.contains("lookup"), "{message}");
    }

    // ---- the per-answer tool budget (ADR-0032) ----

    /// A fake inner dispatch for budget tests: counts its own executions and
    /// answers a fixed value, so the budget/audit logic is exercised without
    /// any Python callable (mirrors auto-serve's ADR-0028 `counting_inner`).
    fn counting_inner(
        runs: Arc<AtomicU64>,
    ) -> impl FnMut(&str, &Value) -> Result<Value, String> + Send + 'static {
        move |_name: &str, _input: &Value| {
            runs.fetch_add(1, Ordering::Relaxed);
            Ok(json!("looked-up"))
        }
    }

    #[test]
    fn budget_allows_up_to_n_executions_then_refuses_without_invoking() {
        let calls = Arc::new(AtomicU64::new(0));
        let inner_runs = Arc::new(AtomicU64::new(0));
        let mut wrapped = budgeted(
            2,
            Arc::clone(&calls),
            counting_inner(Arc::clone(&inner_runs)),
        );
        assert_eq!(
            wrapped("lookup", &json!({"q": 1})).expect("call 1"),
            json!("looked-up")
        );
        assert_eq!(
            wrapped("lookup", &json!({"q": 2})).expect("call 2"),
            json!("looked-up")
        );
        let over = wrapped("lookup", &json!({"q": 3})).expect_err("call 3 exceeds budget 2");
        assert_eq!(over, "tool budget exceeded: 2 per answer (ADR-0032)");
        // a second over-budget attempt is refused identically
        wrapped("lookup", &json!({"q": 4})).expect_err("call 4 stays refused");
        assert_eq!(
            inner_runs.load(Ordering::Relaxed),
            2,
            "the callable ran only for the two allowed calls"
        );
        assert_eq!(
            calls.load(Ordering::Relaxed),
            2,
            "the counter holds EXECUTED calls only — refusals do not advance it"
        );
    }

    #[test]
    fn budget_zero_refuses_the_first_call_and_never_invokes() {
        let inner_runs = Arc::new(AtomicU64::new(0));
        let mut wrapped = budgeted(
            0,
            Arc::new(AtomicU64::new(0)),
            counting_inner(Arc::clone(&inner_runs)),
        );
        let refused = wrapped("lookup", &json!("q")).expect_err("budget 0 refuses call 1");
        assert_eq!(refused, "tool budget exceeded: 0 per answer (ADR-0032)");
        assert_eq!(
            inner_runs.load(Ordering::Relaxed),
            0,
            "the callable is never invoked under budget 0"
        );
    }

    #[test]
    fn reset_restores_the_full_budget() {
        // budget 1: call 1 executes, call 2 is refused; the binding's
        // per-answer `store(0)` (the top of `.answer`) restores the budget,
        // so the next call executes again — the reset semantic, testable
        // without CPython.
        let calls = Arc::new(AtomicU64::new(0));
        let inner_runs = Arc::new(AtomicU64::new(0));
        let mut wrapped = budgeted(
            1,
            Arc::clone(&calls),
            counting_inner(Arc::clone(&inner_runs)),
        );
        wrapped("lookup", &json!(1)).expect("answer 1, call 1");
        wrapped("lookup", &json!(2)).expect_err("answer 1, call 2 exceeds budget 1");
        calls.store(0, Ordering::Relaxed); // what `.answer` does at its top
        wrapped("lookup", &json!(3)).expect("answer 2, call 1 — budget restored");
        assert_eq!(inner_runs.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_failing_callable_still_counts_as_executed() {
        // budget 1, inner errors: the invocation happened, so it consumes the
        // budget and its OWN error (not the budget message) propagates; the
        // next call is then refused by the budget.
        let calls = Arc::new(AtomicU64::new(0));
        let mut wrapped = budgeted(1, Arc::clone(&calls), |_name: &str, _input: &Value| {
            Err("tool \"lookup\" raised: boom".to_owned())
        });
        let first = wrapped("lookup", &json!(1)).expect_err("the callable's own error");
        assert!(first.contains("boom"), "{first}");
        assert!(!first.contains("budget"), "{first}");
        let second = wrapped("lookup", &json!(2)).expect_err("budget consumed by the failed call");
        assert_eq!(second, "tool budget exceeded: 1 per answer (ADR-0032)");
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn budget_on_pure_refusal_names_the_rule_and_the_adr() {
        let message = budget_on_pure_message(3);
        assert!(message.contains("max_tool_calls=3"), "{message}");
        // the frozen wording: the budget parameter is meaningless without a
        // tool host, and the message says so
        assert!(message.contains("a tool budget needs tools="), "{message}");
        assert!(message.contains("nothing to bound"), "{message}");
        assert!(message.contains("ADR-0032"), "{message}");
    }

    // ---- envelope decode ----

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
                // the composed message, unchanged from the message-only era
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
                // the structured fields the binding exposes as attributes
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
