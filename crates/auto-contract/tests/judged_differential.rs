//! Judged differential (ADR-0021): the ADR-0019 judge arbitrates
//! byte-divergent differential groups; the declared ADR-0018 agreement
//! threshold still decides. Pinned with [`ScriptedJudge`] — no model
//! anywhere: these tests exercise the arbiter the differential gate feeds
//! ([`judged_differential_checks`]) and the strict `[acceptance]` parsing,
//! and say so.

use std::path::Path;

use auto_contract::harness::{
    CallableSubject, Check, CheckStatus, DifferentialComparison, ScriptedJudge, Verdict,
    judged_differential_checks, verdict_of, verify_against_subject,
    verify_against_subject_with_judge,
};
use auto_contract::model::{Acceptance, DifferentialMatch};
use auto_contract::parse::from_toml_str;
use serde_json::{Value, json};

fn contract_toml(acceptance: &str) -> String {
    format!(
        "contract_version = 0\ntask = \"t\"\n\n[scope]\ntype = \"task\"\n\n[interface]\ninput = \"json\"\noutput = \"text\"\n\n{acceptance}"
    )
}

/// A `Compared` group with an agreed (non-divergent) reference.
fn compared(reference: &str, subject: &str) -> DifferentialComparison {
    DifferentialComparison::Compared {
        reference: json!(reference),
        subject_output: json!(subject),
        observations: 1,
        distinct_outputs: 1,
    }
}

/// The single agreement check that carries the differential verdict.
fn agreement(checks: &[Check]) -> &Check {
    checks
        .iter()
        .find(|c| c.what.starts_with("differential agreement >= "))
        .expect("agreement check present")
}

// ---- [acceptance] parsing (strict, id-bearing) ------------------------------

#[test]
fn differential_match_parses_strictly_and_bears_the_id() {
    assert_eq!(
        Acceptance::default().differential_match,
        DifferentialMatch::Exact
    );

    // absent = exact = the default
    let absent = from_toml_str(
        &contract_toml("[acceptance]\ndifferential_min_agreement_milli = 800\n"),
        Path::new("."),
    )
    .expect("parse absent");
    assert_eq!(
        absent.acceptance.differential_match,
        DifferentialMatch::Exact
    );

    // declared "exact" is the default made explicit — same claim, same id
    let declared_exact = from_toml_str(
        &contract_toml(
            "[acceptance]\ndifferential_min_agreement_milli = 800\ndifferential_match = \"exact\"\n",
        ),
        Path::new("."),
    )
    .expect("parse declared exact");
    assert_eq!(declared_exact.acceptance, absent.acceptance);
    assert_eq!(declared_exact.id(), absent.id());

    // "judged" is a different reproduction claim — a different id; a judged
    // differential must never masquerade under the exact contract's id
    let judged_toml = contract_toml(
        "[acceptance]\ndifferential_min_agreement_milli = 800\ndifferential_match = \"judged\"\n",
    );
    let judged = from_toml_str(&judged_toml, Path::new(".")).expect("parse judged");
    assert_eq!(
        judged.acceptance.differential_match,
        DifferentialMatch::Judged
    );
    assert_eq!(
        judged.acceptance.differential_min_agreement_milli,
        Some(800)
    );
    assert_ne!(judged.id(), absent.id());
    assert!(
        judged
            .canonical_json()
            .contains(r#""differential_match":"judged""#),
        "{}",
        judged.canonical_json()
    );

    // stable when equal
    let judged_again = from_toml_str(&judged_toml, Path::new(".")).expect("reparse judged");
    assert_eq!(judged.id(), judged_again.id());
}

#[test]
fn differential_match_rejections_are_loud() {
    // unknown value: the error names the two legal values and the offender
    let err = from_toml_str(
        &contract_toml(
            "[acceptance]\ndifferential_min_agreement_milli = 800\ndifferential_match = \"semantic\"\n",
        ),
        Path::new("."),
    )
    .expect_err("unknown mode rejected");
    let message = err.to_string();
    assert!(message.contains("\"exact\""), "{message}");
    assert!(message.contains("\"judged\""), "{message}");
    assert!(message.contains("semantic"), "{message}");

    // non-string value
    let err = from_toml_str(
        &contract_toml(
            "[acceptance]\ndifferential_min_agreement_milli = 800\ndifferential_match = 1\n",
        ),
        Path::new("."),
    )
    .expect_err("non-string rejected");
    assert!(err.to_string().contains("string"), "{err}");

    // judged without a declared threshold: nothing to decide against —
    // the ADR-0018 threshold is the sole acceptance authority (ADR-0021)
    let err = from_toml_str(
        &contract_toml("[acceptance]\ndifferential_match = \"judged\"\n"),
        Path::new("."),
    )
    .expect_err("judged without threshold rejected");
    let message = err.to_string();
    assert!(
        message.contains("differential_min_agreement_milli"),
        "{message}"
    );
    assert!(message.contains("ADR-0021"), "{message}");
}

// ---- exact mode: no drift ----------------------------------------------------

#[test]
fn exact_acceptance_with_a_judge_supplied_changes_nothing() {
    // exact differential_match (the default): the judge-bearing entry point
    // produces byte-identical checks to the judge-less one and never
    // consults the judge — the arbiter is only fed under
    // differential_match = "judged" (the gate's dispatch, ADR-0021)
    let text = contract_toml("[acceptance]\ndifferential_min_agreement_milli = 800\n")
        + "\n[[example]]\nname = \"e\"\nmatch = \"exact\"\ninput = 1\noutput = \"one\"\n";
    let contract = from_toml_str(&text, Path::new(".")).expect("parse");
    // empty script: any consult would come back as an exhaustion error
    let mut judge = ScriptedJudge::new();
    let mut subject = CallableSubject::new("exact", |_: &Value| Ok(json!("one")));
    let with_judge = verify_against_subject_with_judge(&contract, &mut subject, Some(&mut judge));
    let mut subject = CallableSubject::new("exact", |_: &Value| Ok(json!("one")));
    let without = verify_against_subject(&contract, &mut subject);
    assert_eq!(with_judge.verdict, Verdict::Pass);
    assert_eq!(with_judge.checks, without.checks);
    assert!(
        judge.calls.is_empty(),
        "judge consulted under exact acceptance: {:?}",
        judge.calls
    );
}

// ---- the arbiter -------------------------------------------------------------

#[test]
fn judged_yes_flips_below_threshold_agreement_to_passing() {
    // 1 byte-equal + 1 divergent at the declared min 1000: byte counting
    // measures 1/2 and fails; a judged-equivalent divergent group counts
    // matched and the SAME declared threshold passes on measured 2/2.
    let comparisons = vec![
        compared("same", "same"),
        compared("the recorded summary", "a faithful paraphrase"),
    ];

    let mut no = ScriptedJudge::new();
    no.push_verdict(false);
    let failing = judged_differential_checks("t", &comparisons, 1000, Some(&mut no));
    assert_eq!(verdict_of(&failing), Verdict::Fail);
    assert_eq!(agreement(&failing).status, CheckStatus::Failed);
    assert_eq!(
        agreement(&failing).detail.as_deref(),
        Some("measured 1/2 = 50.0% (truncated)")
    );

    let mut yes = ScriptedJudge::new();
    yes.push_verdict(true);
    let passing = judged_differential_checks("t", &comparisons, 1000, Some(&mut yes));
    assert_eq!(verdict_of(&passing), Verdict::Pass, "{passing:?}");
    assert_eq!(
        agreement(&passing).detail.as_deref(),
        Some("measured 2/2 = 100.0% (truncated)")
    );

    // exactly one consult — the byte-equal group was free — with the pinned
    // task context and both values (reference = expected, subject = actual)
    assert_eq!(yes.calls.len(), 1);
    assert_eq!(yes.calls[0].expected, json!("the recorded summary"));
    assert_eq!(yes.calls[0].actual, json!("a faithful paraphrase"));
    assert_eq!(
        yes.calls[0].task_context,
        "task \"t\", differential input #1"
    );

    // the judged match is never mistakable for a byte match
    let judged_line = passing
        .iter()
        .find(|c| c.what.contains("input #1"))
        .expect("judged line");
    assert!(
        judged_line.what.starts_with("[agreement evidence]"),
        "{}",
        judged_line.what
    );
    assert!(
        judged_line.what.contains("JUDGED equivalent"),
        "{}",
        judged_line.what
    );
    let detail = judged_line.detail.as_deref().expect("detail");
    assert!(detail.contains("not byte reproduction"), "{detail}");
    assert!(detail.contains("scripted-judge"), "{detail}");
}

#[test]
fn judged_no_stays_failing_with_both_values() {
    // zero matches fail even the loosest declarable gate (min 1)
    let comparisons = vec![compared("the recorded summary", "an off-topic answer")];
    let mut judge = ScriptedJudge::new();
    judge.push_verdict(false);
    let checks = judged_differential_checks("t", &comparisons, 1, Some(&mut judge));
    assert_eq!(verdict_of(&checks), Verdict::Fail);
    assert_eq!(agreement(&checks).status, CheckStatus::Failed);
    assert_eq!(
        agreement(&checks).detail.as_deref(),
        Some("measured 0/1 = 0.0% (truncated)")
    );
    // only the agreement check fails; per-group lines are evidence
    assert_eq!(
        checks
            .iter()
            .filter(|c| c.status == CheckStatus::Failed)
            .count(),
        1
    );
    let line = checks
        .iter()
        .find(|c| c.what.contains("JUDGED not equivalent"))
        .expect("judged-no line");
    let detail = line.detail.as_deref().expect("detail");
    assert!(detail.contains("the recorded summary"), "{detail}");
    assert!(detail.contains("an off-topic answer"), "{detail}");
    assert!(detail.contains("scripted-judge"), "{detail}");
}

#[test]
fn no_judge_is_unchecked_never_exact_fallback() {
    // even when every group is byte-equal, a declared judged differential
    // without a judge is Unchecked — never silently counted exactly
    let comparisons = vec![compared("same", "same")];
    let checks = judged_differential_checks("t", &comparisons, 1, None);
    assert_eq!(verdict_of(&checks), Verdict::Inconclusive);
    let check = agreement(&checks);
    assert_eq!(check.status, CheckStatus::Unchecked);
    assert_eq!(
        check.detail.as_deref(),
        Some("judged differential declared but no judge supplied (pass --judge-model)")
    );
    // the per-group byte-equal evidence line still appears
    assert!(
        checks
            .iter()
            .any(|c| c.what
                == "differential: input #0 reproduces recorded output (1 observation(s))"),
        "{checks:?}"
    );

    // a divergent group's line says why it was not judged
    let comparisons = vec![compared("a", "b")];
    let checks = judged_differential_checks("t", &comparisons, 1, None);
    assert_eq!(verdict_of(&checks), Verdict::Inconclusive);
    let line = checks
        .iter()
        .find(|c| c.what.contains("input #0 not judged"))
        .expect("not-judged line");
    assert!(
        line.detail
            .as_deref()
            .expect("detail")
            .contains("no judge supplied"),
        "{line:?}"
    );
}

#[test]
fn judge_error_fails_the_agreement_check_and_stops_consulting() {
    let comparisons = vec![compared("a", "x"), compared("b", "y"), compared("c", "z")];
    let mut judge = ScriptedJudge::new();
    judge.push_error("scripted outage");
    // verdicts scripted after the error must never be consumed
    judge.push_verdict(true);
    judge.push_verdict(true);
    let checks = judged_differential_checks("t", &comparisons, 1, Some(&mut judge));
    assert_eq!(verdict_of(&checks), Verdict::Fail);
    let check = agreement(&checks);
    assert_eq!(check.status, CheckStatus::Failed);
    let detail = check.detail.as_deref().expect("detail");
    assert!(detail.contains("scripted outage"), "{detail}");
    assert!(detail.contains("input #0"), "{detail}");
    assert!(detail.contains("never passes"), "{detail}");
    // exactly one consult: the failure stopped the run (each may be paid)
    assert_eq!(judge.calls.len(), 1);
    // later divergent groups say why they were not judged
    let later = checks
        .iter()
        .find(|c| c.what.contains("input #2"))
        .expect("later line");
    assert!(
        later
            .detail
            .as_deref()
            .expect("detail")
            .contains("prior judge call failed"),
        "{later:?}"
    );
}

#[test]
fn byte_equal_groups_never_consult_a_supplied_judge() {
    let comparisons = vec![compared("one", "one"), compared("two", "two")];
    // empty script: any consult would come back as an exhaustion error
    let mut judge = ScriptedJudge::new();
    let checks = judged_differential_checks("t", &comparisons, 1000, Some(&mut judge));
    assert_eq!(verdict_of(&checks), Verdict::Pass);
    assert!(judge.calls.is_empty(), "judge consulted: {:?}", judge.calls);
    assert_eq!(
        agreement(&checks).detail.as_deref(),
        Some("measured 2/2 = 100.0% (truncated)")
    );
}

#[test]
fn errored_reference_and_subject_error_count_unmatched() {
    let comparisons = vec![
        compared("same", "same"),
        DifferentialComparison::ErroredReference {
            errors: 2,
            observations: 3,
        },
        DifferentialComparison::SubjectError {
            error: "kaput".to_owned(),
        },
    ];
    let mut judge = ScriptedJudge::new();
    // 1/3 at min 333: 1000 >= 999 — Passed; at min 334: 1000 < 1002 — Failed
    let checks = judged_differential_checks("t", &comparisons, 333, Some(&mut judge));
    assert_eq!(verdict_of(&checks), Verdict::Pass, "{checks:?}");
    assert_eq!(
        agreement(&checks).detail.as_deref(),
        Some("measured 1/3 = 33.3% (truncated)")
    );
    let checks = judged_differential_checks("t", &comparisons, 334, Some(&mut judge));
    assert_eq!(verdict_of(&checks), Verdict::Fail);
    assert!(
        judge.calls.is_empty(),
        "neither errored references nor subject errors are judge matters"
    );
    // evidence lines say what happened and that they count, not kill
    let errored = checks
        .iter()
        .find(|c| c.what == "[agreement evidence] recorded outputs agree for input #1")
        .expect("errored-reference line");
    let detail = errored.detail.as_deref().expect("detail");
    assert!(
        detail.contains("2 recorded error(s) over 3 observation(s)"),
        "{detail}"
    );
    assert!(detail.contains("subject not run"), "{detail}");
    assert!(detail.contains("not fatal"), "{detail}");
    let broken = checks
        .iter()
        .find(|c| c.what == "[agreement evidence] differential: subject answers input #2")
        .expect("subject-error line");
    assert!(
        broken
            .detail
            .as_deref()
            .expect("detail")
            .contains("subject error: kaput"),
        "{broken:?}"
    );
}

#[test]
fn majority_pick_reference_is_always_named() {
    // byte-equal against a majority pick: matched free, and the line says
    // the reference was the ADR-0018 pick — never a silently agreed one
    let comparisons = vec![DifferentialComparison::Compared {
        reference: json!("majority"),
        subject_output: json!("majority"),
        observations: 3,
        distinct_outputs: 2,
    }];
    let mut judge = ScriptedJudge::new();
    let checks = judged_differential_checks("t", &comparisons, 1000, Some(&mut judge));
    assert!(judge.calls.is_empty());
    assert_eq!(checks[0].status, CheckStatus::Passed);
    assert!(
        checks[0]
            .detail
            .as_deref()
            .expect("detail")
            .contains("ADR-0018 majority pick over 2 distinct recorded outputs"),
        "{:?}",
        checks[0]
    );

    // judged against a majority pick: the note rides the judged detail too
    let comparisons = vec![DifferentialComparison::Compared {
        reference: json!("majority"),
        subject_output: json!("paraphrase"),
        observations: 3,
        distinct_outputs: 2,
    }];
    let mut judge = ScriptedJudge::new();
    judge.push_verdict(true);
    let checks = judged_differential_checks("t", &comparisons, 1000, Some(&mut judge));
    let detail = checks[0].detail.as_deref().expect("detail");
    assert!(detail.contains("JUDGED equivalent"), "{detail}");
    assert!(detail.contains("majority pick"), "{detail}");
}

#[test]
fn zero_groups_is_unchecked_no_observations() {
    let mut judge = ScriptedJudge::new();
    let checks = judged_differential_checks("t", &[], 500, Some(&mut judge));
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].status, CheckStatus::Unchecked);
    assert_eq!(
        checks[0].detail.as_deref(),
        Some("no differential observations; partial data never passes")
    );
    assert!(judge.calls.is_empty());
    // judge-less identically: with nothing replayed there is nothing to judge
    let checks = judged_differential_checks("t", &[], 500, None);
    assert_eq!(checks[0].status, CheckStatus::Unchecked);
}
