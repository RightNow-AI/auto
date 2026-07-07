//! Verification harness: run a contract against a subject, produce a
//! three-valued verdict.
//!
//! Subjects are recorded trace stores today (scope = span, and scope = task
//! for traces carrying task-level I/O — ADR-0025) and executable artifacts
//! from S3 on (any [`Subject`] impl). Verdict semantics
//! (spec/contract.md §7): **Fail** iff anything checked was violated;
//! **Inconclusive** iff nothing was violated but a normative claim went
//! unchecked (unwitnessed example, zero observations, declared-but-
//! unmeasurable budget); **Pass** otherwise. Inconclusive is never rounded
//! up, and nothing is extrapolated.
//!
//! Cost/token budgets are measured from the reserved span attrs
//! `cost_usd_micros` / `tokens` (decimal u64 strings, spec/trace.md §3) —
//! the recording agent's own declaration of what its API billed; the
//! harness never fabricates them. All-or-unchecked: a budget is measured
//! only when every observation carries the attr, and a present-but-
//! unparseable value is a loud failure, never silently ignored.
//!
//! `match = "judged"` examples (ADR-0019) compare by LLM-judged semantic
//! equivalence through the [`Judge`] seam. The judge implementation lives
//! outside this crate, with the spend rails (ADR-0010) — this crate never
//! spends. Exactly-equal outputs pass without consulting the judge; a
//! judged example with no judge supplied is Unchecked, never Pass; every
//! check that did consult a judge says JUDGED in its detail.
//!
//! `differential_match = "judged"` (ADR-0021) extends the same judge to the
//! differential: [`judged_differential_checks`] arbitrates byte-divergent
//! replay groups and folds them into the ADR-0018 agreement check — the
//! declared threshold still decides; the judge only decides what counts as
//! matched. The differential gate (auto-backend) prepares one
//! [`DifferentialComparison`] per distinct replayed input; this crate owns
//! the counting, the evidence lines, and the never-silently-exact rule.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::fmt::Write as _;
use std::time::Instant;

use auto_trace::Store;
use auto_trace::model::canonical_json;
use serde_json::Value;

use crate::conform::conforms;
use crate::model::{Contract, MatchMode, Scope};
use crate::properties::check as check_property;

/// Verdict of one verification run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Inconclusive,
    Fail,
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Pass => "PASS",
            Self::Inconclusive => "INCONCLUSIVE",
            Self::Fail => "FAIL",
        })
    }
}

/// Anything that can answer contract inputs. Implemented by tests via
/// [`CallableSubject`] and, from S3 on, by compiled artifacts.
pub trait Subject {
    fn describe(&self) -> String;
    fn run(&mut self, input: &Value) -> Result<Value, String>;
}

/// A subject backed by a closure — the harness's genuine API exercised
/// directly (used by tests and tooling; not a mock of anything).
pub struct CallableSubject<F> {
    name: String,
    f: F,
}

impl<F> CallableSubject<F>
where
    F: FnMut(&Value) -> Result<Value, String>,
{
    pub fn new(name: impl Into<String>, f: F) -> Self {
        Self {
            name: name.into(),
            f,
        }
    }
}

impl<F> Subject for CallableSubject<F>
where
    F: FnMut(&Value) -> Result<Value, String>,
{
    fn describe(&self) -> String {
        format!("callable:{}", self.name)
    }

    fn run(&mut self, input: &Value) -> Result<Value, String> {
        (self.f)(input)
    }
}

/// A judge of semantic equivalence for `match = "judged"` examples
/// (ADR-0019). Implemented outside this crate by the spend-capped frontier
/// judge (its calls are paid, capped, and ledgered — ADR-0010) and inside
/// tests by [`ScriptedJudge`]. A judge is a model with opinions, not exact
/// reproduction: every check that consults one says JUDGED in its detail
/// and names the judge via [`Judge::describe`].
pub trait Judge {
    /// Is `actual` semantically equivalent to `expected` for the contracted
    /// task? `task_context` names the task and example under judgment
    /// (`task "<task>", example "<name>"`). An `Err` is a judge failure —
    /// the harness treats it as Failed, never Pass.
    fn equivalent(
        &mut self,
        expected: &Value,
        actual: &Value,
        task_context: &str,
    ) -> Result<bool, String>;
    /// Names the judge (model / configuration) for check details.
    fn describe(&self) -> String;
}

/// One recorded [`ScriptedJudge`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JudgeCall {
    pub expected: Value,
    pub actual: Value,
    pub task_context: String,
}

/// Test fake: answers `equivalent` from a fixed script, records every call
/// it was asked. NOT a mock pretending to be a judge model — tests that use
/// it are testing the harness's judged-match protocol, and say so (mirrors
/// `auto_frontier::ScriptedFrontier`).
#[derive(Debug, Default)]
pub struct ScriptedJudge {
    script: VecDeque<Result<bool, String>>,
    pub calls: Vec<JudgeCall>,
}

impl ScriptedJudge {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a verdict.
    pub fn push_verdict(&mut self, equivalent: bool) {
        self.script.push_back(Ok(equivalent));
    }

    /// Queue a judge failure.
    pub fn push_error(&mut self, error: &str) {
        self.script.push_back(Err(error.to_owned()));
    }
}

impl Judge for ScriptedJudge {
    fn equivalent(
        &mut self,
        expected: &Value,
        actual: &Value,
        task_context: &str,
    ) -> Result<bool, String> {
        self.calls.push(JudgeCall {
            expected: expected.clone(),
            actual: actual.clone(),
            task_context: task_context.to_owned(),
        });
        self.script.pop_front().unwrap_or_else(|| {
            Err("scripted judge exhausted: more calls than scripted verdicts".to_owned())
        })
    }

    fn describe(&self) -> String {
        "scripted-judge".to_owned()
    }
}

/// Status of one checked claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    Passed,
    Failed,
    /// could not be checked; forces Inconclusive, never Pass
    Unchecked,
}

impl fmt::Display for CheckStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Passed => "pass",
            Self::Failed => "FAIL",
            Self::Unchecked => "----",
        })
    }
}

/// One checked (or uncheckable) claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub what: String,
    pub status: CheckStatus,
    pub detail: Option<String>,
}

impl Check {
    fn passed(what: impl Into<String>) -> Self {
        Self {
            what: what.into(),
            status: CheckStatus::Passed,
            detail: None,
        }
    }

    fn failed(what: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            what: what.into(),
            status: CheckStatus::Failed,
            detail: Some(detail.into()),
        }
    }

    fn unchecked(what: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            what: what.into(),
            status: CheckStatus::Unchecked,
            detail: Some(detail.into()),
        }
    }
}

/// The result of verifying one contract against one subject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationReport {
    /// contract content id (`Contract::id`)
    pub contract_id: String,
    pub task: String,
    pub subject: String,
    pub verdict: Verdict,
    pub observations: usize,
    pub checks: Vec<Check>,
}

#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    #[error(
        "region-scope contracts verify at compile time (the emit gate gathers \
         and replays the recorded chains); trace-mode `auto verify` for \
         regions is future work (spec/synthesis.md §8)"
    )]
    RegionScopeUnverifiableInTraceMode,
    /// region chains recorded with inconsistent or unusable structure
    #[error("region structure: {0}")]
    RegionStructure(String),
    /// a v0 region chain contains an effectful non-pure span
    #[error(
        "region purity: {0} — v0 regions compile pure chains only; capability \
         imports for tool-calling regions are recorded future work (ADR-0015)"
    )]
    RegionImpure(String),
    #[error(transparent)]
    Trace(#[from] auto_trace::TraceError),
}

/// Verdict over a check list: any Failed ⇒ Fail; else any Unchecked ⇒
/// Inconclusive; else Pass. Public so callers composing extra checks (e.g.
/// the backend's differential pass) recompute verdicts by the same rule.
pub fn verdict_of(checks: &[Check]) -> Verdict {
    if checks.iter().any(|c| c.status == CheckStatus::Failed) {
        Verdict::Fail
    } else if checks.iter().any(|c| c.status == CheckStatus::Unchecked) {
        Verdict::Inconclusive
    } else {
        Verdict::Pass
    }
}

/// Statistical acceptance over differential reproductions (ADR-0018): did
/// the subject reproduce the recorded output on at least `min_milli`
/// thousandths of the eligible replayed inputs? Pure integer math, no
/// floats: **Passed** iff `matched * 1000 >= min_milli * eligible`
/// (widened to u128 — never overflows, never rounds); **Failed**
/// otherwise; **Unchecked** when `eligible == 0` — no differential
/// observations; partial data never passes. The detail carries the
/// measured rate as `matched/eligible` plus a truncated percent — measured
/// numbers, never rounded up. The caller (the backend's differential gate)
/// decides what counts as matched and eligible, then folds this check into
/// [`verdict_of`] alongside the rest.
pub fn agreement_check(matched: usize, eligible: usize, min_milli: u32) -> Check {
    debug_assert!(
        matched <= eligible,
        "agreement_check: matched ({matched}) exceeds eligible ({eligible})"
    );
    let what = format!("differential agreement >= {min_milli}/1000");
    if eligible == 0 {
        return Check::unchecked(
            what,
            "no differential observations; partial data never passes",
        );
    }
    // usize -> u128 is lossless on every supported target (usize <= 64 bits)
    let (matched_w, eligible_w) = (matched as u128, eligible as u128);
    let passed = matched_w * 1000 >= u128::from(min_milli) * eligible_w;
    // integer thousandths, truncated toward zero — never rounded up
    let rate_milli = matched_w * 1000 / eligible_w;
    let detail = format!(
        "measured {matched}/{eligible} = {}.{}% (truncated)",
        rate_milli / 10,
        rate_milli % 10
    );
    if passed {
        Check {
            what,
            status: CheckStatus::Passed,
            detail: Some(detail),
        }
    } else {
        Check::failed(what, detail)
    }
}

/// What the differential gate observed for one replayed group — one
/// distinct recorded input — before any judge involvement; the per-group
/// input to [`judged_differential_checks`] (ADR-0021). The gate constructs
/// these in canonical input order, one per group, so the `#i` labels here
/// line up with the exact-mode differential labels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DifferentialComparison {
    /// The subject answered and the group has a usable reference: the
    /// single agreed recorded output when `distinct_outputs == 1`, else
    /// the ADR-0018 canonical pick (majority witness, lexicographic
    /// tie-break) — the gate's existing pick rule, never a second one.
    Compared {
        reference: Value,
        subject_output: Value,
        observations: usize,
        /// distinct recorded outputs behind the reference (>1 = the
        /// reference is the majority pick, and every line over it says so)
        distinct_outputs: usize,
    },
    /// The group recorded errors, so no trustworthy reference exists and
    /// the subject was never run. Counts as unmatched — a judge cannot
    /// rescue a group with nothing to compare against.
    ErroredReference { errors: usize, observations: usize },
    /// The subject failed to answer the group's input. Counts as
    /// unmatched; a subject error is never a judge matter.
    SubjectError { error: String },
}

/// Judged differential (ADR-0021): arbitrate byte-divergent groups through
/// the [`Judge`] and fold everything into per-group `[agreement evidence]`
/// lines plus ONE agreement check that alone carries the differential
/// verdict — the declared ADR-0018 threshold still decides; the judge only
/// decides what counts as matched. Semantics, in order:
///
/// - a group whose subject output equals its reference (canonical-json
///   equality) counts matched WITHOUT consulting the judge — the wave-8
///   free short-circuit; its line is the exact-mode passed line (plus a
///   note when the reference was the majority pick);
/// - a byte-divergent group consults the judge exactly once, with the
///   task context `task "<task>", differential input #<i>`:
///   judged-equivalent counts matched and its line says JUDGED equivalent
///   — never mistakable for a byte match; judged-not-equivalent counts
///   unmatched, with both values in the line;
/// - a judge error fails the whole agreement check with the error in its
///   detail — a judge failure never passes and never quietly counts as a
///   mere mismatch; later divergent groups are not consulted (each consult
///   may be a paid call, and the count is already unusable);
/// - no judge supplied: the agreement check is Unchecked ("judged
///   differential declared but no judge supplied (pass --judge-model)") —
///   never a silent fallback to exact counting, even when every group is
///   byte-equal (the declaration demands the instrument); per-group
///   byte-equal evidence lines still appear;
/// - errored-reference and subject-error groups count unmatched — exactly
///   the shortfall the declared threshold prices in.
///
/// `matched` = byte-equal + judged-equivalent; `eligible` = every group.
/// At most one judge call per byte-divergent distinct input. Zero groups
/// is Unchecked via [`agreement_check`] — no observations never pass.
pub fn judged_differential_checks(
    task: &str,
    comparisons: &[DifferentialComparison],
    min_milli: u32,
    mut judge: Option<&mut (dyn Judge + '_)>,
) -> Vec<Check> {
    let mut checks = Vec::with_capacity(comparisons.len() + 1);
    if comparisons.is_empty() {
        checks.push(agreement_check(0, 0, min_milli));
        return checks;
    }
    let evidence = "counted against the declared agreement threshold, not fatal";
    let mut matched = 0usize;
    // (input index, judge name, error) of the first failed judge call
    let mut judge_failure: Option<(usize, String, String)> = None;
    for (i, comparison) in comparisons.iter().enumerate() {
        match comparison {
            DifferentialComparison::Compared {
                reference,
                subject_output,
                observations,
                distinct_outputs,
            } => {
                let reference_canonical = canonical_json(reference);
                let subject_canonical = canonical_json(subject_output);
                let pick_note = (*distinct_outputs > 1).then(|| {
                    format!(
                        "reference is the ADR-0018 majority pick over {distinct_outputs} \
                         distinct recorded outputs"
                    )
                });
                if subject_canonical == reference_canonical {
                    // free short-circuit: byte equality needs no judge
                    matched += 1;
                    checks.push(Check {
                        what: format!(
                            "differential: input #{i} reproduces recorded output \
                             ({observations} observation(s))"
                        ),
                        status: CheckStatus::Passed,
                        detail: pick_note,
                    });
                    continue;
                }
                if judge_failure.is_some() {
                    checks.push(Check {
                        what: format!("[agreement evidence] differential: input #{i} not judged"),
                        status: CheckStatus::Passed,
                        detail: Some(
                            "byte-divergent; judge not consulted — a prior judge call failed"
                                .to_owned(),
                        ),
                    });
                    continue;
                }
                let Some(judge) = judge.as_deref_mut() else {
                    checks.push(Check {
                        what: format!("[agreement evidence] differential: input #{i} not judged"),
                        status: CheckStatus::Passed,
                        detail: Some(
                            "byte-divergent; judged differential declared but no judge supplied"
                                .to_owned(),
                        ),
                    });
                    continue;
                };
                let task_context = format!("task \"{task}\", differential input #{i}");
                let with_pick_note = |detail: String| match &pick_note {
                    Some(note) => format!("{detail}; {note}"),
                    None => detail,
                };
                match judge.equivalent(reference, subject_output, &task_context) {
                    Ok(true) => {
                        matched += 1;
                        checks.push(Check {
                            what: format!(
                                "[agreement evidence] differential: input #{i} JUDGED equivalent"
                            ),
                            status: CheckStatus::Passed,
                            detail: Some(with_pick_note(format!(
                                "JUDGED equivalent, not byte reproduction — by {}: subject {} \
                                 vs reference {}",
                                judge.describe(),
                                snippet(&subject_canonical),
                                snippet(&reference_canonical),
                            ))),
                        });
                    }
                    Ok(false) => {
                        checks.push(Check {
                            what: format!(
                                "[agreement evidence] differential: input #{i} JUDGED not \
                                 equivalent"
                            ),
                            status: CheckStatus::Passed,
                            detail: Some(with_pick_note(format!(
                                "JUDGED not equivalent by {}: subject {} != reference {}; \
                                 {evidence}",
                                judge.describe(),
                                snippet(&subject_canonical),
                                snippet(&reference_canonical),
                            ))),
                        });
                    }
                    Err(e) => {
                        checks.push(Check {
                            what: format!(
                                "[agreement evidence] differential: input #{i} judge failed"
                            ),
                            status: CheckStatus::Passed,
                            detail: Some(format!("judge {} failed: {e}", judge.describe())),
                        });
                        judge_failure = Some((i, judge.describe(), e));
                    }
                }
            }
            DifferentialComparison::ErroredReference {
                errors,
                observations,
            } => {
                checks.push(Check {
                    what: format!("[agreement evidence] recorded outputs agree for input #{i}"),
                    status: CheckStatus::Passed,
                    detail: Some(format!(
                        "{errors} recorded error(s) over {observations} observation(s); no \
                         usable reference; subject not run; {evidence}"
                    )),
                });
            }
            DifferentialComparison::SubjectError { error } => {
                checks.push(Check {
                    what: format!("[agreement evidence] differential: subject answers input #{i}"),
                    status: CheckStatus::Passed,
                    detail: Some(format!("subject error: {error}; {evidence}")),
                });
            }
        }
    }

    let what = format!("differential agreement >= {min_milli}/1000");
    checks.push(match judge_failure {
        Some((i, name, error)) => Check::failed(
            what,
            format!(
                "judge {name} failed on input #{i}: {error} (a judge failure never passes and \
                 never counts as a mere mismatch)"
            ),
        ),
        None => match judge {
            None => Check::unchecked(
                what,
                "judged differential declared but no judge supplied (pass --judge-model)",
            ),
            Some(_) => agreement_check(matched, comparisons.len(), min_milli),
        },
    });
    checks
}

/// Truncate a canonical value for check details.
fn snippet(s: &str) -> String {
    const MAX: usize = 48;
    if s.chars().count() <= MAX {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(MAX).collect();
        out.push('…');
        out
    }
}

/// Reserved span attr keys (spec/trace.md §3): decimal u64 strings set by
/// the recording agent as its own declaration of what its API billed for
/// that call. The harness reads them; it never fabricates them.
const COST_ATTR: &str = "cost_usd_micros";
const TOKENS_ATTR: &str = "tokens";

/// One observed input/output pair, however it was obtained.
struct Observation {
    input: Value,
    output: Value,
    duration_ms: u64,
    /// agent-declared cost (micro-usd) from the reserved `cost_usd_micros`
    /// span attr; always `None` in subject mode (a live call has no billing)
    cost_usd_micros: Option<u64>,
    /// agent-declared token count from the reserved `tokens` span attr;
    /// always `None` in subject mode
    tokens: Option<u64>,
}

/// Read a reserved budget attr as a decimal u64. A present-but-unparseable
/// value is pushed onto `malformed` — never silently ignored.
fn parse_budget_attr(
    attrs: &BTreeMap<String, String>,
    key: &str,
    observation: usize,
    malformed: &mut Vec<String>,
) -> Option<u64> {
    let raw = attrs.get(key)?;
    match raw.parse::<u64>() {
        Ok(v) => Some(v),
        Err(_) => {
            malformed.push(format!(
                "observation {observation} {key}={raw:?} is not a decimal u64"
            ));
            None
        }
    }
}

/// p95 by the nearest-rank method: sort ascending, take element at
/// `ceil(0.95 * n)` (1-based). Callers guarantee non-empty. Used for
/// latencies and for agent-declared cost/token values alike.
fn p95(values: &[u64]) -> u64 {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let n = sorted.len();
    let rank = (n * 95).div_ceil(100).max(1);
    sorted[rank - 1]
}

/// Verify a span- or task-scope contract against every recorded trace of its
/// task in a store. Region-scope contracts error (not silently skipped).
///
/// Task scope (ADR-0025): one observation per trace recording BOTH a task
/// input (`task_input` at construction) and a task output (`set_task_output`)
/// — never one without the other, and nothing is invented for runs that
/// recorded neither. The observation's latency is the recorded wall-clock
/// from run start (header `started_at_ms`) to the output declaration
/// (`recorded_at_ms`), both stamped by the recorder's own clock; cost/token
/// budgets have no task-level declaration channel yet, so they stay
/// Unchecked. A store whose traces carry no task-level I/O yields an
/// Unchecked "task-level observations present" check — Inconclusive, never
/// an error and never a silent pass.
pub fn verify_against_store(
    contract: &Contract,
    store: &Store,
) -> Result<VerificationReport, HarnessError> {
    if matches!(contract.scope, Scope::Region { .. }) {
        return Err(HarnessError::RegionScopeUnverifiableInTraceMode);
    }
    let traces = store.load_task(&contract.task)?;

    let mut observations = Vec::new();
    let mut checks = Vec::new();
    match &contract.scope {
        Scope::Region { .. } => unreachable!("region scope returned above"),
        Scope::Span { kind, name } => {
            let mut recorded_errors = 0usize;
            let mut malformed_attrs = Vec::new();
            for trace in &traces {
                for span in &trace.spans {
                    if span.kind.wire() == kind && &span.name == name {
                        if span.error.is_some() {
                            recorded_errors += 1;
                        }
                        let index = observations.len();
                        let cost_usd_micros =
                            parse_budget_attr(&span.attrs, COST_ATTR, index, &mut malformed_attrs);
                        let tokens = parse_budget_attr(
                            &span.attrs,
                            TOKENS_ATTR,
                            index,
                            &mut malformed_attrs,
                        );
                        observations.push(Observation {
                            input: span.input.clone(),
                            output: span.output.clone().unwrap_or(Value::Null),
                            duration_ms: span.duration_ms,
                            cost_usd_micros,
                            tokens,
                        });
                    }
                }
            }

            if recorded_errors > 0 {
                checks.push(Check::failed(
                    "observations free of recorded errors",
                    format!("{recorded_errors} recorded observation(s) errored"),
                ));
            } else if observations.is_empty() {
                checks.push(Check::unchecked(
                    "observations present",
                    format!("no recorded spans match {kind}({name})"),
                ));
            } else {
                checks.push(Check::passed(format!(
                    "observations free of recorded errors ({} observations)",
                    observations.len()
                )));
            }

            if !malformed_attrs.is_empty() {
                let shown = malformed_attrs
                    .iter()
                    .take(3)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("; ");
                checks.push(Check::failed(
                    "budget attrs are well-formed",
                    format!("{} malformed value(s): {shown}", malformed_attrs.len()),
                ));
            }
        }
        Scope::Task => {
            let mut partial = 0usize;
            for trace in &traces {
                let header = &trace.header;
                match header.task_observation() {
                    Some((input, output)) => observations.push(Observation {
                        input: input.clone(),
                        output: output.value.clone(),
                        // recorded wall-clock: run start -> output declared,
                        // both stamped by the recorder's clock
                        duration_ms: output.recorded_at_ms.saturating_sub(header.started_at_ms),
                        // no task-level billing declaration channel exists;
                        // never fabricated, so cost/token budgets stay Unchecked
                        cost_usd_micros: None,
                        tokens: None,
                    }),
                    None if header.task_input.is_some() || header.task_output.is_some() => {
                        partial += 1;
                    }
                    None => {}
                }
            }

            let partial_note = (partial > 0).then(|| {
                format!(
                    "{partial} trace(s) record only one of task input/output — not observations"
                )
            });
            if observations.is_empty() {
                let mut detail =
                    "no task-level I/O recorded (record with task_input / set_task_output)"
                        .to_owned();
                if let Some(note) = &partial_note {
                    detail = format!("{detail}; {note}");
                }
                checks.push(Check::unchecked("task-level observations present", detail));
            } else {
                checks.push(Check {
                    what: format!(
                        "task-level observations present ({} of {} traces record task input+output)",
                        observations.len(),
                        traces.len()
                    ),
                    status: CheckStatus::Passed,
                    detail: partial_note,
                });
            }
        }
    }

    // trace-mode verification carries no judge in v0: a judged example
    // whose recorded outputs diverge from the expected output is Unchecked
    checks.extend(common_checks(
        contract,
        &observations,
        LatencySource::Recorded,
        None,
    ));

    Ok(VerificationReport {
        contract_id: contract.id().0,
        task: contract.task.clone(),
        subject: format!(
            "trace-store task \"{}\" ({} traces)",
            contract.task,
            traces.len()
        ),
        verdict: verdict_of(&checks),
        observations: observations.len(),
        checks,
    })
}

/// Verify a contract by executing a subject on every example and eval input.
/// Latency is measured wall time per call; a subject error is a failure.
/// Judge-less: delegates to [`verify_against_subject_with_judge`] with
/// `None`, so a `match = "judged"` example whose output diverges is
/// Unchecked (never Pass) and everything else is byte-identical.
pub fn verify_against_subject(
    contract: &Contract,
    subject: &mut dyn Subject,
) -> VerificationReport {
    verify_against_subject_with_judge(contract, subject, None)
}

/// [`verify_against_subject`], with an optional [`Judge`] for
/// `match = "judged"` examples (ADR-0019). The subject runs exactly as in
/// the judge-less path; the judge is consulted only for a judged example
/// whose output is not already exactly equal to the expected output —
/// exact examples never touch it.
pub fn verify_against_subject_with_judge(
    contract: &Contract,
    subject: &mut dyn Subject,
    judge: Option<&mut (dyn Judge + '_)>,
) -> VerificationReport {
    let mut observations = Vec::new();
    let mut checks = Vec::new();

    let inputs: Vec<(String, &Value)> = contract
        .examples
        .iter()
        .map(|e| (format!("example \"{}\"", e.name), &e.input))
        .chain(
            contract
                .eval_cases
                .iter()
                .enumerate()
                .map(|(i, c)| (format!("eval case #{i}"), &c.input)),
        )
        .collect();

    for (label, input) in &inputs {
        let start = Instant::now();
        match subject.run(input) {
            Ok(output) => {
                let duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
                observations.push(Observation {
                    input: (*input).clone(),
                    output,
                    duration_ms,
                    // a live subject call has no billing to declare
                    cost_usd_micros: None,
                    tokens: None,
                });
            }
            Err(e) => {
                checks.push(Check::failed(
                    format!("subject answers {label}"),
                    format!("subject error: {e}"),
                ));
            }
        }
    }

    checks.extend(common_checks(
        contract,
        &observations,
        LatencySource::Measured,
        judge,
    ));

    VerificationReport {
        contract_id: contract.id().0,
        task: contract.task.clone(),
        subject: subject.describe(),
        verdict: verdict_of(&checks),
        observations: observations.len(),
        checks,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LatencySource {
    Recorded,
    Measured,
}

/// Checks shared by both modes: interface conformance, examples, eval
/// expectations, properties, budgets. `judge` serves `match = "judged"`
/// examples only; exact examples and eval expectations never consult it.
fn common_checks(
    contract: &Contract,
    observations: &[Observation],
    latency: LatencySource,
    mut judge: Option<&mut (dyn Judge + '_)>,
) -> Vec<Check> {
    let mut checks = Vec::new();

    // interface conformance over every observation
    if observations.is_empty() {
        checks.push(Check::unchecked("interface conformance", "no observations"));
    } else {
        let mut violations = Vec::new();
        for (i, obs) in observations.iter().enumerate() {
            if let Err(e) = conforms(&obs.input, &contract.interface.input) {
                violations.push(format!("observation {i} input: {e}"));
            }
            if let Err(e) = conforms(&obs.output, &contract.interface.output) {
                violations.push(format!("observation {i} output: {e}"));
            }
        }
        if violations.is_empty() {
            checks.push(Check::passed(format!(
                "interface conformance ({} observations)",
                observations.len()
            )));
        } else {
            let shown = violations
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join("; ");
            checks.push(Check::failed(
                "interface conformance",
                format!("{} violation(s): {shown}", violations.len()),
            ));
        }
    }

    // examples: witnessed by canonical input equality; exact output match,
    // or judged semantic equivalence for `match = "judged"` (ADR-0019)
    for example in &contract.examples {
        let what = format!("example \"{}\"", example.name);
        let wanted_input = canonical_json(&example.input);
        let matching: Vec<&Observation> = observations
            .iter()
            .filter(|o| canonical_json(&o.input) == wanted_input)
            .collect();
        if matching.is_empty() {
            checks.push(Check::unchecked(
                what,
                "input not witnessed by any observation",
            ));
            continue;
        }
        match example.match_mode {
            MatchMode::Exact => {
                let wanted_output = canonical_json(&example.output);
                let mismatches = matching
                    .iter()
                    .filter(|o| canonical_json(&o.output) != wanted_output)
                    .count();
                if mismatches == 0 {
                    checks.push(Check::passed(format!(
                        "{what} ({} matching observation(s))",
                        matching.len()
                    )));
                } else {
                    checks.push(Check::failed(
                        what,
                        format!(
                            "{mismatches} of {} matching observation(s) produced a different output",
                            matching.len()
                        ),
                    ));
                }
            }
            MatchMode::Judged => {
                let task_context =
                    format!("task \"{}\", example \"{}\"", contract.task, example.name);
                checks.push(judged_example_check(
                    what,
                    &example.output,
                    &matching,
                    &task_context,
                    judge.as_deref_mut(),
                ));
            }
        }
    }

    // eval cases with expected outputs behave like unnamed examples;
    // cases without expectations still demand witnessing
    for (i, case) in contract.eval_cases.iter().enumerate() {
        let what = format!("eval case #{i}");
        let wanted_input = canonical_json(&case.input);
        let matching: Vec<&Observation> = observations
            .iter()
            .filter(|o| canonical_json(&o.input) == wanted_input)
            .collect();
        if matching.is_empty() {
            checks.push(Check::unchecked(
                what,
                "input not witnessed by any observation",
            ));
            continue;
        }
        match &case.expected {
            None => checks.push(Check::passed(format!(
                "{what} witnessed ({} observation(s))",
                matching.len()
            ))),
            Some(expected) => {
                let wanted_output = canonical_json(expected);
                let mismatches = matching
                    .iter()
                    .filter(|o| canonical_json(&o.output) != wanted_output)
                    .count();
                if mismatches == 0 {
                    checks.push(Check::passed(format!(
                        "{what} ({} matching observation(s))",
                        matching.len()
                    )));
                } else {
                    checks.push(Check::failed(
                        what,
                        format!("{mismatches} observation(s) produced a different output"),
                    ));
                }
            }
        }
    }

    // properties over every observed output
    for property in &contract.properties {
        if observations.is_empty() {
            let outcome = check_property(property, &Value::Null);
            checks.push(Check::unchecked(
                format!("property {}", outcome.description),
                "no observations",
            ));
            continue;
        }
        let mut first_failure: Option<String> = None;
        let mut failures = 0usize;
        let mut description = String::new();
        for obs in observations {
            let outcome = check_property(property, &obs.output);
            description = outcome.description.clone();
            if !outcome.passed {
                failures += 1;
                if first_failure.is_none() {
                    first_failure = outcome.detail.or(Some("failed".to_owned()));
                }
            }
        }
        let what = format!("property {description}");
        if failures == 0 {
            checks.push(Check::passed(format!(
                "{what} ({} observations)",
                observations.len()
            )));
        } else {
            checks.push(Check::failed(
                what,
                format!(
                    "{failures} of {} observations violate: {}",
                    observations.len(),
                    first_failure.unwrap_or_default()
                ),
            ));
        }
    }

    // budgets
    if let Some(cap) = contract.budgets.max_latency_ms_p95 {
        if observations.is_empty() {
            checks.push(Check::unchecked(
                format!("budget max_latency_ms_p95 <= {cap}"),
                "no observations",
            ));
        } else {
            let durations: Vec<u64> = observations.iter().map(|o| o.duration_ms).collect();
            let measured = p95(&durations);
            let source = match latency {
                LatencySource::Recorded => "recorded",
                LatencySource::Measured => "measured",
            };
            if measured <= cap {
                checks.push(Check::passed(format!(
                    "budget max_latency_ms_p95 <= {cap} ({source} p95 = {measured}ms over {} observations)",
                    observations.len()
                )));
            } else {
                checks.push(Check::failed(
                    format!("budget max_latency_ms_p95 <= {cap}"),
                    format!("{source} p95 = {measured}ms"),
                ));
            }
        }
    }
    if let Some(cap) = contract.budgets.max_cost_usd_micros {
        checks.push(attr_budget_check(
            "max_cost_usd_micros",
            COST_ATTR,
            cap,
            "µ$",
            observations,
            |o| o.cost_usd_micros,
        ));
    }
    if let Some(cap) = contract.budgets.max_tokens {
        checks.push(attr_budget_check(
            "max_tokens",
            TOKENS_ATTR,
            cap,
            " tokens",
            observations,
            |o| o.tokens,
        ));
    }

    checks
}

/// Check one `match = "judged"` example (ADR-0019) over its matching
/// observations. Outputs exactly equal to the expected output (canonical-
/// json equality) pass free of charge — the judge is consulted only for
/// divergent outputs, deduplicated canonically (judging the same pair twice
/// buys nothing, and each consult may be a paid call). Any judged
/// non-equivalence or judge failure fails the example; no judge means the
/// claim goes Unchecked, never Pass. Every detail that rests on a judge
/// verdict says JUDGED and names the judge — a judge is a model with
/// opinions, not exact reproduction.
fn judged_example_check(
    what: String,
    expected: &Value,
    matching: &[&Observation],
    task_context: &str,
    // `+ '_` decouples the trait-object lifetime from the reference so the
    // caller's per-example reborrow (`as_deref_mut`) type-checks
    judge: Option<&mut (dyn Judge + '_)>,
) -> Check {
    let wanted_output = canonical_json(expected);
    let mut divergent: Vec<&Observation> = Vec::new();
    let mut seen = BTreeSet::new();
    for obs in matching {
        let canonical = canonical_json(&obs.output);
        if canonical != wanted_output && seen.insert(canonical) {
            divergent.push(obs);
        }
    }
    // free short-circuit: exactly-equal outputs need no judge
    if divergent.is_empty() {
        return Check {
            what,
            status: CheckStatus::Passed,
            detail: Some(format!(
                "output exactly equal on all {} matching observation(s); judge not consulted",
                matching.len()
            )),
        };
    }
    let Some(judge) = judge else {
        return Check::unchecked(
            what,
            "judged match declared but no judge supplied (pass --judge-model)",
        );
    };
    for obs in &divergent {
        match judge.equivalent(expected, &obs.output, task_context) {
            Ok(true) => {}
            Ok(false) => {
                return Check::failed(
                    what,
                    format!(
                        "JUDGED not equivalent by {}: expected {wanted_output} got {}",
                        judge.describe(),
                        canonical_json(&obs.output),
                    ),
                );
            }
            Err(e) => {
                return Check::failed(
                    what,
                    format!(
                        "judge {} failed: {e} (a judge failure never passes)",
                        judge.describe()
                    ),
                );
            }
        }
    }
    Check {
        what,
        status: CheckStatus::Passed,
        detail: Some(format!(
            "JUDGED equivalent, not exact reproduction — by {} over {} divergent output(s)",
            judge.describe(),
            divergent.len()
        )),
    }
}

/// Budget check over agent-declared per-observation values (the reserved
/// span attrs). All-or-unchecked: measured p95 vs cap only when **every**
/// observation carries the attr; partial data never passes. Only recorded
/// traces carry attrs, so a measurement here is always "recorded".
fn attr_budget_check(
    budget: &str,
    attr: &str,
    cap: u64,
    unit: &str,
    observations: &[Observation],
    value: impl Fn(&Observation) -> Option<u64>,
) -> Check {
    let what = format!("budget {budget} <= {cap}");
    if observations.is_empty() {
        return Check::unchecked(what, "no observations");
    }
    let values: Vec<u64> = observations.iter().filter_map(value).collect();
    if values.is_empty() {
        return Check::unchecked(what, format!("not measurable: no recorded {attr} attrs"));
    }
    let missing = observations.len() - values.len();
    if missing > 0 {
        return Check::unchecked(
            what,
            format!(
                "{missing} of {} observations carry no {attr} attr; partial data never passes",
                observations.len()
            ),
        );
    }
    let measured = p95(&values);
    if measured <= cap {
        Check::passed(format!(
            "{what} (recorded p95 = {measured}{unit} over {} observations)",
            observations.len()
        ))
    } else {
        Check::failed(what, format!("recorded p95 = {measured}{unit}"))
    }
}

/// Deterministic human rendering. Not a stable machine format — the eval-run
/// record (evalrun.rs) is the citable artifact.
pub fn render(report: &VerificationReport) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "verification — contract {} task \"{}\"",
        &report.contract_id[..12.min(report.contract_id.len())],
        report.task
    );
    let _ = writeln!(out, "subject: {}", report.subject);
    let _ = writeln!(out, "observations: {}", report.observations);
    let _ = writeln!(out, "checks:");
    for check in &report.checks {
        match &check.detail {
            Some(detail) => {
                let _ = writeln!(out, "  [{}] {} — {detail}", check.status, check.what);
            }
            None => {
                let _ = writeln!(out, "  [{}] {}", check.status, check.what);
            }
        }
    }
    let _ = writeln!(out, "verdict: {}", report.verdict);
    if report.verdict == Verdict::Inconclusive {
        let _ = writeln!(
            out,
            "(inconclusive = nothing violated, but unchecked normative claims remain; never rounded up to pass)"
        );
    }
    out
}
