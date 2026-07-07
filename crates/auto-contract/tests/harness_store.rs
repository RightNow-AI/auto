//! Harness behavior against real (synthetic) trace stores and callable
//! subjects: verdict semantics are the product — every path is pinned.

use std::collections::BTreeMap;

use auto_contract::harness::{
    CallableSubject, HarnessError, Verdict, render, verify_against_store, verify_against_subject,
};
use auto_contract::model::Acceptance;
use auto_contract::{
    Budgets, Contract, EvalCase, Example, Interface, MatchMode, Property, Scope, Target,
};
use auto_ir::ValueType;
use auto_trace::model::{Span, SpanId, SpanKind, Trace, TraceHeader, TraceId};
use auto_trace::store::Store;
use serde_json::{Value, json};

fn span(seq: u64, kind: SpanKind, name: &str, input: Value, output: Value, dur: u64) -> Span {
    Span {
        span_id: SpanId(seq),
        parent_span_id: None,
        seq,
        kind,
        name: name.into(),
        input,
        output: Some(output),
        error: None,
        started_at_ms: 0,
        duration_ms: dur,
        attrs: BTreeMap::new(),
    }
}

fn trace(id: u128, task: &str, spans: Vec<Span>) -> Trace {
    Trace {
        header: TraceHeader {
            trace_id: TraceId(id),
            task: task.into(),
            started_at_ms: 0,
            sdk: "test/0".into(),
            attrs: BTreeMap::new(),
            task_input: None,
            task_output: None,
        },
        spans,
    }
}

fn store_with(traces: Vec<Trace>) -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut store = Store::open(&dir.path().join("t.db")).expect("open");
    for t in traces {
        store.ingest(&t).expect("ingest");
    }
    (dir, store)
}

fn base_contract() -> Contract {
    Contract {
        task: "t".into(),
        scope: Scope::Span {
            kind: "model_call".into(),
            name: "m".into(),
        },
        interface: Interface {
            input: ValueType::Json,
            output: ValueType::Text,
        },
        examples: vec![Example {
            name: "basic".into(),
            input: json!({"x": 1}),
            output: json!("ok"),
            match_mode: MatchMode::Exact,
        }],
        properties: vec![
            Property::Regex {
                target: Target::Output,
                pattern: "^[a-z]+$".into(),
            },
            Property::LenRange {
                target: Target::Output,
                min: Some(1),
                max: Some(10),
            },
        ],
        budgets: Budgets {
            max_latency_ms_p95: Some(1000),
            ..Budgets::default()
        },
        acceptance: Acceptance::default(),
        eval_cases: vec![],
    }
}

fn good_span(seq: u64) -> Span {
    span(
        seq,
        SpanKind::ModelCall,
        "m",
        json!({"x": 1}),
        json!("ok"),
        5,
    )
}

#[test]
fn all_checked_and_held_is_pass() {
    let (_d, store) = store_with(vec![
        trace(1, "t", vec![good_span(1)]),
        trace(2, "t", vec![good_span(1)]),
    ]);
    let report = verify_against_store(&base_contract(), &store).expect("verify");
    assert_eq!(report.verdict, Verdict::Pass, "{}", render(&report));
    assert_eq!(report.observations, 2);
}

#[test]
fn property_violation_is_fail() {
    let (_d, store) = store_with(vec![trace(
        1,
        "t",
        vec![span(
            1,
            SpanKind::ModelCall,
            "m",
            json!({"x": 1}),
            json!("OK!"),
            5,
        )],
    )]);
    let report = verify_against_store(&base_contract(), &store).expect("verify");
    assert_eq!(report.verdict, Verdict::Fail, "{}", render(&report));
}

#[test]
fn example_output_mismatch_is_fail() {
    let (_d, store) = store_with(vec![
        trace(1, "t", vec![good_span(1)]),
        trace(
            2,
            "t",
            vec![span(
                1,
                SpanKind::ModelCall,
                "m",
                json!({"x": 1}),
                json!("no"),
                5,
            )],
        ),
    ]);
    let report = verify_against_store(&base_contract(), &store).expect("verify");
    assert_eq!(report.verdict, Verdict::Fail, "{}", render(&report));
}

#[test]
fn unwitnessed_example_is_inconclusive_not_pass() {
    let (_d, store) = store_with(vec![trace(
        1,
        "t",
        vec![span(
            1,
            SpanKind::ModelCall,
            "m",
            json!({"x": 999}),
            json!("ok"),
            5,
        )],
    )]);
    let report = verify_against_store(&base_contract(), &store).expect("verify");
    assert_eq!(report.verdict, Verdict::Inconclusive, "{}", render(&report));
}

#[test]
fn declared_but_unmeasurable_budget_is_inconclusive() {
    let mut contract = base_contract();
    contract.budgets.max_cost_usd_micros = Some(100);
    let (_d, store) = store_with(vec![
        trace(1, "t", vec![good_span(1)]),
        trace(2, "t", vec![good_span(1)]),
    ]);
    let report = verify_against_store(&contract, &store).expect("verify");
    assert_eq!(report.verdict, Verdict::Inconclusive, "{}", render(&report));
    let rendered = render(&report);
    assert!(rendered.contains("not measurable"), "{rendered}");
}

#[test]
fn zero_observations_is_inconclusive() {
    let (_d, store) = store_with(vec![trace(
        1,
        "t",
        vec![span(
            1,
            SpanKind::ToolCall,
            "other",
            json!({}),
            json!("x"),
            1,
        )],
    )]);
    let report = verify_against_store(&base_contract(), &store).expect("verify");
    assert_eq!(report.verdict, Verdict::Inconclusive, "{}", render(&report));
    assert_eq!(report.observations, 0);
}

#[test]
fn recorded_error_is_fail() {
    let mut bad = good_span(1);
    bad.error = Some("boom".into());
    bad.output = None;
    let (_d, store) = store_with(vec![
        trace(1, "t", vec![bad]),
        trace(2, "t", vec![good_span(1)]),
    ]);
    let report = verify_against_store(&base_contract(), &store).expect("verify");
    assert_eq!(report.verdict, Verdict::Fail, "{}", render(&report));
}

#[test]
fn interface_violation_is_fail() {
    let (_d, store) = store_with(vec![trace(
        1,
        "t",
        vec![span(
            1,
            SpanKind::ModelCall,
            "m",
            json!({"x": 1}),
            json!(42),
            5,
        )],
    )]);
    let report = verify_against_store(&base_contract(), &store).expect("verify");
    assert_eq!(report.verdict, Verdict::Fail, "{}", render(&report));
}

#[test]
fn latency_budget_exceeded_is_fail() {
    let (_d, store) = store_with(vec![
        trace(
            1,
            "t",
            vec![span(
                1,
                SpanKind::ModelCall,
                "m",
                json!({"x": 1}),
                json!("ok"),
                5000,
            )],
        ),
        trace(
            2,
            "t",
            vec![span(
                1,
                SpanKind::ModelCall,
                "m",
                json!({"x": 1}),
                json!("ok"),
                5000,
            )],
        ),
    ]);
    let report = verify_against_store(&base_contract(), &store).expect("verify");
    assert_eq!(report.verdict, Verdict::Fail, "{}", render(&report));
}

// --- task scope against traces (ADR-0025) ----------------------------------

/// A trace whose header carries task-level I/O; wall-clock = recorded_at_ms
/// (header started_at_ms is 0 in these fixtures).
fn task_io_trace(id: u128, input: Value, output: Value, recorded_at_ms: u64) -> Trace {
    let mut t = trace(id, "t", vec![good_span(1)]);
    t.header.task_input = Some(input);
    t.header.task_output = Some(auto_trace::TaskOutput {
        value: output,
        recorded_at_ms,
    });
    t
}

fn task_contract() -> Contract {
    let mut contract = base_contract();
    contract.scope = Scope::Task;
    contract.examples = vec![Example {
        name: "basic".into(),
        input: json!({"doc": "d"}),
        output: json!("ok"),
        match_mode: MatchMode::Exact,
    }];
    contract
}

#[test]
fn task_scope_verifies_against_task_io_traces() {
    let (_d, store) = store_with(vec![
        task_io_trace(1, json!({"doc": "d"}), json!("ok"), 40),
        task_io_trace(2, json!({"doc": "d"}), json!("ok"), 60),
    ]);
    let report = verify_against_store(&task_contract(), &store).expect("verify");
    assert_eq!(report.verdict, Verdict::Pass, "{}", render(&report));
    assert_eq!(report.observations, 2);
    let rendered = render(&report);
    assert!(
        rendered
            .contains("task-level observations present (2 of 2 traces record task input+output)"),
        "{rendered}"
    );
    // latency is the recorded wall-clock run start -> output declaration
    assert!(rendered.contains("recorded p95 = 60ms"), "{rendered}");
}

#[test]
fn task_scope_example_mismatch_is_fail() {
    let (_d, store) = store_with(vec![task_io_trace(1, json!({"doc": "d"}), json!("no"), 5)]);
    let report = verify_against_store(&task_contract(), &store).expect("verify");
    assert_eq!(report.verdict, Verdict::Fail, "{}", render(&report));
}

#[test]
fn task_scope_without_task_io_is_unchecked_not_an_error() {
    // the pre-ADR-0025 refusal (TaskScopeUnverifiable) retires: recordings
    // without task I/O verify Inconclusive with a how-to-record detail
    let (_d, store) = store_with(vec![trace(1, "t", vec![good_span(1)])]);
    let report = verify_against_store(&task_contract(), &store).expect("verify");
    assert_eq!(report.verdict, Verdict::Inconclusive, "{}", render(&report));
    assert_eq!(report.observations, 0);
    let rendered = render(&report);
    assert!(
        rendered.contains("no task-level I/O recorded (record with task_input / set_task_output)"),
        "{rendered}"
    );
}

#[test]
fn task_scope_partial_io_is_named_never_an_observation() {
    let mut partial_only = trace(1, "t", vec![good_span(1)]);
    partial_only.header.task_input = Some(json!({"doc": "d"}));
    let (_d, store) = store_with(vec![
        partial_only,
        task_io_trace(2, json!({"doc": "d"}), json!("ok"), 5),
    ]);
    let report = verify_against_store(&task_contract(), &store).expect("verify");
    assert_eq!(report.observations, 1);
    let rendered = render(&report);
    assert!(
        rendered.contains("1 trace(s) record only one of task input/output — not observations"),
        "{rendered}"
    );
}

#[test]
fn task_scope_cost_budget_stays_unchecked() {
    // no task-level billing declaration channel exists; never fabricated
    let mut contract = task_contract();
    contract.budgets.max_cost_usd_micros = Some(100);
    let (_d, store) = store_with(vec![
        task_io_trace(1, json!({"doc": "d"}), json!("ok"), 5),
        task_io_trace(2, json!({"doc": "d"}), json!("ok"), 5),
    ]);
    let report = verify_against_store(&contract, &store).expect("verify");
    assert_eq!(report.verdict, Verdict::Inconclusive, "{}", render(&report));
    let rendered = render(&report);
    assert!(
        rendered.contains("not measurable: no recorded cost_usd_micros attrs"),
        "{rendered}"
    );
}

#[test]
fn unknown_task_errors_loudly() {
    let (_d, store) = store_with(vec![]);
    assert!(matches!(
        verify_against_store(&base_contract(), &store),
        Err(HarnessError::Trace(auto_trace::TraceError::UnknownTask(_)))
    ));
}

#[test]
fn eval_case_expectation_mismatch_is_fail() {
    let mut contract = base_contract();
    contract.eval_cases = vec![EvalCase {
        input: json!({"x": 1}),
        expected: Some(json!("different")),
    }];
    let (_d, store) = store_with(vec![trace(1, "t", vec![good_span(1)])]);
    let report = verify_against_store(&contract, &store).expect("verify");
    assert_eq!(report.verdict, Verdict::Fail, "{}", render(&report));
}

#[test]
fn witnessed_eval_case_without_expectation_passes() {
    let mut contract = base_contract();
    contract.eval_cases = vec![EvalCase {
        input: json!({"x": 1}),
        expected: None,
    }];
    let (_d, store) = store_with(vec![
        trace(1, "t", vec![good_span(1)]),
        trace(2, "t", vec![good_span(1)]),
    ]);
    let report = verify_against_store(&contract, &store).expect("verify");
    assert_eq!(report.verdict, Verdict::Pass, "{}", render(&report));
}

#[test]
fn callable_subject_pass_and_fail_and_error() {
    let contract = base_contract();

    let mut ok = CallableSubject::new("ok", |_input: &Value| Ok(json!("ok")));
    let report = verify_against_subject(&contract, &mut ok);
    assert_eq!(report.verdict, Verdict::Pass, "{}", render(&report));
    assert_eq!(report.observations, 1);

    let mut wrong = CallableSubject::new("wrong", |_input: &Value| Ok(json!("nope!")));
    let report = verify_against_subject(&contract, &mut wrong);
    assert_eq!(report.verdict, Verdict::Fail, "{}", render(&report));

    let mut broken = CallableSubject::new("broken", |_input: &Value| Err("kaput".to_owned()));
    let report = verify_against_subject(&contract, &mut broken);
    assert_eq!(report.verdict, Verdict::Fail, "{}", render(&report));
}

// -- cost/token budgets from the reserved span attrs ------------------------

/// good_span with the given reserved budget attrs set by the "agent".
fn good_span_attrs(seq: u64, attrs: &[(&str, &str)]) -> Span {
    let mut s = good_span(seq);
    s.attrs = attrs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    s
}

#[test]
fn cost_budget_all_attrs_under_cap_is_pass() {
    let mut contract = base_contract();
    contract.budgets.max_cost_usd_micros = Some(100);
    let (_d, store) = store_with(vec![
        trace(
            1,
            "t",
            vec![good_span_attrs(1, &[("cost_usd_micros", "40")])],
        ),
        trace(
            2,
            "t",
            vec![good_span_attrs(1, &[("cost_usd_micros", "60")])],
        ),
    ]);
    let report = verify_against_store(&contract, &store).expect("verify");
    assert_eq!(report.verdict, Verdict::Pass, "{}", render(&report));
    let rendered = render(&report);
    assert!(
        rendered.contains("recorded p95 = 60µ$ over 2 observations"),
        "{rendered}"
    );
}

#[test]
fn cost_budget_over_cap_is_fail() {
    let mut contract = base_contract();
    contract.budgets.max_cost_usd_micros = Some(50);
    let (_d, store) = store_with(vec![
        trace(
            1,
            "t",
            vec![good_span_attrs(1, &[("cost_usd_micros", "40")])],
        ),
        trace(
            2,
            "t",
            vec![good_span_attrs(1, &[("cost_usd_micros", "60")])],
        ),
    ]);
    let report = verify_against_store(&contract, &store).expect("verify");
    assert_eq!(report.verdict, Verdict::Fail, "{}", render(&report));
    let rendered = render(&report);
    assert!(rendered.contains("recorded p95 = 60µ$"), "{rendered}");
}

#[test]
fn partial_cost_attrs_is_inconclusive() {
    let mut contract = base_contract();
    contract.budgets.max_cost_usd_micros = Some(100);
    let (_d, store) = store_with(vec![
        trace(
            1,
            "t",
            vec![good_span_attrs(1, &[("cost_usd_micros", "40")])],
        ),
        trace(2, "t", vec![good_span(1)]),
    ]);
    let report = verify_against_store(&contract, &store).expect("verify");
    assert_eq!(report.verdict, Verdict::Inconclusive, "{}", render(&report));
    let rendered = render(&report);
    assert!(
        rendered.contains(
            "1 of 2 observations carry no cost_usd_micros attr; partial data never passes"
        ),
        "{rendered}"
    );
}

#[test]
fn malformed_cost_attr_is_fail() {
    let mut contract = base_contract();
    contract.budgets.max_cost_usd_micros = Some(100);
    let (_d, store) = store_with(vec![trace(
        1,
        "t",
        vec![good_span_attrs(1, &[("cost_usd_micros", "12.5")])],
    )]);
    let report = verify_against_store(&contract, &store).expect("verify");
    assert_eq!(report.verdict, Verdict::Fail, "{}", render(&report));
    let rendered = render(&report);
    assert!(
        rendered.contains("budget attrs are well-formed"),
        "{rendered}"
    );
    assert!(rendered.contains("is not a decimal u64"), "{rendered}");
}

#[test]
fn tokens_budget_all_attrs_under_cap_is_pass() {
    let mut contract = base_contract();
    contract.budgets.max_tokens = Some(100);
    let (_d, store) = store_with(vec![
        trace(1, "t", vec![good_span_attrs(1, &[("tokens", "40")])]),
        trace(2, "t", vec![good_span_attrs(1, &[("tokens", "60")])]),
    ]);
    let report = verify_against_store(&contract, &store).expect("verify");
    assert_eq!(report.verdict, Verdict::Pass, "{}", render(&report));
    let rendered = render(&report);
    assert!(
        rendered.contains("recorded p95 = 60 tokens over 2 observations"),
        "{rendered}"
    );
}

#[test]
fn tokens_budget_over_cap_is_fail() {
    let mut contract = base_contract();
    contract.budgets.max_tokens = Some(50);
    let (_d, store) = store_with(vec![
        trace(1, "t", vec![good_span_attrs(1, &[("tokens", "40")])]),
        trace(2, "t", vec![good_span_attrs(1, &[("tokens", "60")])]),
    ]);
    let report = verify_against_store(&contract, &store).expect("verify");
    assert_eq!(report.verdict, Verdict::Fail, "{}", render(&report));
    let rendered = render(&report);
    assert!(rendered.contains("recorded p95 = 60 tokens"), "{rendered}");
}

#[test]
fn partial_tokens_attrs_is_inconclusive() {
    let mut contract = base_contract();
    contract.budgets.max_tokens = Some(100);
    let (_d, store) = store_with(vec![
        trace(1, "t", vec![good_span_attrs(1, &[("tokens", "40")])]),
        trace(2, "t", vec![good_span(1)]),
    ]);
    let report = verify_against_store(&contract, &store).expect("verify");
    assert_eq!(report.verdict, Verdict::Inconclusive, "{}", render(&report));
    let rendered = render(&report);
    assert!(
        rendered.contains("1 of 2 observations carry no tokens attr; partial data never passes"),
        "{rendered}"
    );
}

#[test]
fn malformed_tokens_attr_is_fail() {
    let mut contract = base_contract();
    contract.budgets.max_tokens = Some(100);
    let (_d, store) = store_with(vec![trace(
        1,
        "t",
        vec![good_span_attrs(1, &[("tokens", "12k")])],
    )]);
    let report = verify_against_store(&contract, &store).expect("verify");
    assert_eq!(report.verdict, Verdict::Fail, "{}", render(&report));
    let rendered = render(&report);
    assert!(
        rendered.contains("budget attrs are well-formed"),
        "{rendered}"
    );
    assert!(rendered.contains("is not a decimal u64"), "{rendered}");
}

#[test]
fn subject_mode_cost_budget_is_still_unchecked() {
    let mut contract = base_contract();
    contract.budgets.max_cost_usd_micros = Some(100);
    let mut ok = CallableSubject::new("ok", |_input: &Value| Ok(json!("ok")));
    let report = verify_against_subject(&contract, &mut ok);
    assert_eq!(report.verdict, Verdict::Inconclusive, "{}", render(&report));
    let rendered = render(&report);
    assert!(
        rendered.contains("not measurable: no recorded cost_usd_micros attrs"),
        "{rendered}"
    );
}
