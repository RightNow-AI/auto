//! Statistical acceptance (ADR-0018), exercised through the public surface:
//! the declared threshold parses strictly and bears the contract id, and the
//! pure agreement math is pinned at its boundaries — integer thousandths,
//! truncated measurement, never rounded up.

use std::path::Path;

use auto_contract::harness::{CheckStatus, agreement_check};
use auto_contract::model::Acceptance;
use auto_contract::parse::from_toml_str;

fn contract_toml(sections: &str) -> String {
    format!(
        "contract_version = 0\ntask = \"t\"\n\n[scope]\ntype = \"task\"\n\n[interface]\ninput = \"json\"\noutput = \"text\"\n\n{sections}"
    )
}

#[test]
fn declared_acceptance_parses_and_changes_the_id() {
    let exact = from_toml_str(&contract_toml(""), Path::new(".")).expect("parse exact");
    assert_eq!(exact.acceptance, Acceptance::default());

    let relaxed = from_toml_str(
        &contract_toml("[acceptance]\ndifferential_min_agreement_milli = 800\n"),
        Path::new("."),
    )
    .expect("parse relaxed");
    assert_eq!(
        relaxed.acceptance.differential_min_agreement_milli,
        Some(800)
    );

    // different acceptance = different normative claim = different id
    assert_ne!(exact.id(), relaxed.id());
    // and equal acceptance = stable id
    let relaxed_again = from_toml_str(
        &contract_toml("[acceptance]\ndifferential_min_agreement_milli = 800\n"),
        Path::new("."),
    )
    .expect("reparse relaxed");
    assert_eq!(relaxed.id(), relaxed_again.id());
}

#[test]
fn agreement_check_boundary_at_666_and_667() {
    // 2/3 at min 666: 2*1000 = 2000 >= 666*3 = 1998 — Passed
    let check = agreement_check(2, 3, 666);
    assert_eq!(check.status, CheckStatus::Passed);
    assert_eq!(check.what, "differential agreement >= 666/1000");
    let detail = check
        .detail
        .expect("passed check still reports the measured rate");
    assert_eq!(detail, "measured 2/3 = 66.6% (truncated)");

    // 2/3 at min 667: 2000 < 667*3 = 2001 — Failed
    let check = agreement_check(2, 3, 667);
    assert_eq!(check.status, CheckStatus::Failed);
    assert_eq!(
        check.detail.as_deref(),
        Some("measured 2/3 = 66.6% (truncated)")
    );
}

#[test]
fn agreement_check_zero_eligible_is_unchecked_never_pass() {
    let check = agreement_check(0, 0, 500);
    assert_eq!(check.status, CheckStatus::Unchecked);
    assert_eq!(
        check.detail.as_deref(),
        Some("no differential observations; partial data never passes")
    );
}

#[test]
fn agreement_check_declared_exact_requires_total_agreement() {
    // min 1000: matched*1000 >= 1000*eligible iff matched == eligible
    let check = agreement_check(3, 3, 1000);
    assert_eq!(check.status, CheckStatus::Passed);
    assert_eq!(
        check.detail.as_deref(),
        Some("measured 3/3 = 100.0% (truncated)")
    );
    assert_eq!(agreement_check(2, 3, 1000).status, CheckStatus::Failed);
    // 999/1000 truncates to 99.9% and fails declared-exact — never rounded
    // up to 100%
    let check = agreement_check(999, 1000, 1000);
    assert_eq!(check.status, CheckStatus::Failed);
    assert_eq!(
        check.detail.as_deref(),
        Some("measured 999/1000 = 99.9% (truncated)")
    );
}

#[test]
fn agreement_check_rate_is_truncated_not_rounded() {
    // 1/6 = 166.66..‰ — reported as 16.6%, not 16.7%
    let check = agreement_check(1, 6, 100);
    assert_eq!(check.status, CheckStatus::Passed);
    assert_eq!(
        check.detail.as_deref(),
        Some("measured 1/6 = 16.6% (truncated)")
    );
    // 0 matched is a plain 0.0%
    let check = agreement_check(0, 4, 1);
    assert_eq!(check.status, CheckStatus::Failed);
    assert_eq!(
        check.detail.as_deref(),
        Some("measured 0/4 = 0.0% (truncated)")
    );
    // min 1 is the loosest declarable gate: any nonzero agreement passes
    assert_eq!(agreement_check(1, 1000, 1).status, CheckStatus::Passed);
}
