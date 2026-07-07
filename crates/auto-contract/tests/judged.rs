//! Judged match mode (ADR-0019): the harness's judge protocol, pinned with
//! [`ScriptedJudge`] — no model anywhere; these tests exercise the seam the
//! frontier judge plugs into, and say so.

use auto_contract::harness::{
    CallableSubject, CheckStatus, ScriptedJudge, Verdict, render, verify_against_subject,
    verify_against_subject_with_judge,
};
use auto_contract::model::Acceptance;
use auto_contract::{Budgets, Contract, Example, Interface, MatchMode, Scope};
use auto_ir::ValueType;
use serde_json::{Value, json};

const EXPECTED: &str = "the expected summary";
const DIVERGENT: &str = "a different but equivalent summary";

fn judged_contract() -> Contract {
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
            name: "summary".into(),
            input: json!({"x": 1}),
            output: json!(EXPECTED),
            match_mode: MatchMode::Judged,
        }],
        properties: vec![],
        budgets: Budgets::default(),
        acceptance: Acceptance::default(),
        eval_cases: vec![],
    }
}

fn example_check(
    report: &auto_contract::harness::VerificationReport,
) -> &auto_contract::harness::Check {
    report
        .checks
        .iter()
        .find(|c| c.what == "example \"summary\"")
        .expect("example check present")
}

#[test]
fn exact_equal_output_short_circuits_without_consulting_judge() {
    let contract = judged_contract();
    // empty script: any consult would come back as an error and fail
    let mut judge = ScriptedJudge::new();
    let mut subject = CallableSubject::new("verbatim", |_: &Value| Ok(json!(EXPECTED)));
    let report = verify_against_subject_with_judge(&contract, &mut subject, Some(&mut judge));
    assert_eq!(report.verdict, Verdict::Pass, "{}", render(&report));
    assert!(
        judge.calls.is_empty(),
        "judge was consulted: {:?}",
        judge.calls
    );
    let check = example_check(&report);
    assert_eq!(check.status, CheckStatus::Passed);
    let detail = check.detail.as_deref().expect("short-circuit is noted");
    assert!(detail.contains("judge not consulted"), "{detail}");
}

#[test]
fn judged_equivalent_passes_naming_the_judge() {
    let contract = judged_contract();
    let mut judge = ScriptedJudge::new();
    judge.push_verdict(true);
    let mut subject = CallableSubject::new("paraphrase", |_: &Value| Ok(json!(DIVERGENT)));
    let report = verify_against_subject_with_judge(&contract, &mut subject, Some(&mut judge));
    assert_eq!(report.verdict, Verdict::Pass, "{}", render(&report));

    // exactly one consult, carrying both values and the task context
    assert_eq!(judge.calls.len(), 1);
    assert_eq!(judge.calls[0].expected, json!(EXPECTED));
    assert_eq!(judge.calls[0].actual, json!(DIVERGENT));
    assert_eq!(
        judge.calls[0].task_context,
        "task \"t\", example \"summary\""
    );

    // the pass says JUDGED and names the judge — never mistakable for exact
    let detail = example_check(&report).detail.as_deref().expect("detail");
    assert!(detail.contains("JUDGED"), "{detail}");
    assert!(detail.contains("not exact"), "{detail}");
    assert!(detail.contains("scripted-judge"), "{detail}");
}

#[test]
fn judged_not_equivalent_fails_with_both_values() {
    let contract = judged_contract();
    let mut judge = ScriptedJudge::new();
    judge.push_verdict(false);
    let mut subject = CallableSubject::new("off-topic", |_: &Value| Ok(json!(DIVERGENT)));
    let report = verify_against_subject_with_judge(&contract, &mut subject, Some(&mut judge));
    assert_eq!(report.verdict, Verdict::Fail, "{}", render(&report));
    let check = example_check(&report);
    assert_eq!(check.status, CheckStatus::Failed);
    let detail = check.detail.as_deref().expect("detail");
    assert!(detail.contains("JUDGED not equivalent"), "{detail}");
    assert!(detail.contains(EXPECTED), "{detail}");
    assert!(detail.contains(DIVERGENT), "{detail}");
    assert!(detail.contains("scripted-judge"), "{detail}");
}

#[test]
fn judge_error_fails_never_passes() {
    let contract = judged_contract();
    let mut judge = ScriptedJudge::new();
    judge.push_error("scripted outage");
    let mut subject = CallableSubject::new("paraphrase", |_: &Value| Ok(json!(DIVERGENT)));
    let report = verify_against_subject_with_judge(&contract, &mut subject, Some(&mut judge));
    assert_eq!(report.verdict, Verdict::Fail, "{}", render(&report));
    let detail = example_check(&report).detail.as_deref().expect("detail");
    assert!(detail.contains("scripted outage"), "{detail}");
    assert!(detail.contains("never passes"), "{detail}");
}

#[test]
fn no_judge_is_unchecked_and_verdict_inconclusive() {
    let contract = judged_contract();
    let mut subject = CallableSubject::new("paraphrase", |_: &Value| Ok(json!(DIVERGENT)));
    let report = verify_against_subject_with_judge(&contract, &mut subject, None);
    assert_eq!(report.verdict, Verdict::Inconclusive, "{}", render(&report));
    let check = example_check(&report);
    assert_eq!(check.status, CheckStatus::Unchecked);
    assert_eq!(
        check.detail.as_deref(),
        Some("judged match declared but no judge supplied (pass --judge-model)")
    );

    // the judge-less entry point delegates with None — identical checks
    let mut subject = CallableSubject::new("paraphrase", |_: &Value| Ok(json!(DIVERGENT)));
    let delegated = verify_against_subject(&contract, &mut subject);
    assert_eq!(delegated.verdict, Verdict::Inconclusive);
    assert_eq!(delegated.checks, report.checks);
}

#[test]
fn exact_examples_never_consult_the_judge() {
    let mut contract = judged_contract();
    contract.examples[0].match_mode = MatchMode::Exact;
    // empty script: any consult would surface as an exhaustion error
    let mut judge = ScriptedJudge::new();

    // exact match passes without a consult
    let mut right = CallableSubject::new("verbatim", |_: &Value| Ok(json!(EXPECTED)));
    let report = verify_against_subject_with_judge(&contract, &mut right, Some(&mut judge));
    assert_eq!(report.verdict, Verdict::Pass, "{}", render(&report));
    assert!(judge.calls.is_empty());

    // exact mismatch fails on its own — the judge is not asked to rescue it
    let mut wrong = CallableSubject::new("wrong", |_: &Value| Ok(json!("nope")));
    let report = verify_against_subject_with_judge(&contract, &mut wrong, Some(&mut judge));
    assert_eq!(report.verdict, Verdict::Fail, "{}", render(&report));
    assert!(judge.calls.is_empty(), "judge consulted: {:?}", judge.calls);
}

#[test]
fn unwitnessed_judged_example_is_unchecked_before_any_judge() {
    // subject mode always witnesses examples; pin the store-mode-shaped
    // path via a contract whose example input the subject errors on
    let contract = judged_contract();
    let mut judge = ScriptedJudge::new();
    let mut broken = CallableSubject::new("broken", |_: &Value| Err("kaput".to_owned()));
    let report = verify_against_subject_with_judge(&contract, &mut broken, Some(&mut judge));
    // subject error is a failure; the unwitnessed example is unchecked;
    // the judge is never consulted on an unwitnessed example
    assert_eq!(report.verdict, Verdict::Fail, "{}", render(&report));
    let check = example_check(&report);
    assert_eq!(check.status, CheckStatus::Unchecked);
    assert!(judge.calls.is_empty());
}
