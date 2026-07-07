//! Differential checks: replay recorded inputs through a candidate subject
//! and compare against what the reference interpreter actually did.
//!
//! Every distinct recorded input of the contract's scope (span, region, or —
//! since ADR-0025 — whole-task task-level I/O) is replayed
//! through the subject; the subject's output must equal the recorded output
//! under canonical JSON. An input whose recorded outputs already disagree
//! (or errored) fails the agreement claim outright and the subject is never
//! run on it — a divergent reference is not evidence to compare against.
//! Latencies are collected on both sides so the manifest reports measured
//! numbers only. Verdict folding stays in
//! [`auto_contract::harness::verdict_of`].

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use auto_contract::harness::{Check, CheckStatus, HarnessError, Subject};
use auto_contract::{Contract, Scope};
use auto_trace::Store;
use auto_trace::model::canonical_json;
use serde_json::Value;

/// What one differential run measured and concluded.
#[derive(Debug, Clone)]
pub struct DifferentialOutcome {
    pub checks: Vec<Check>,
    /// distinct canonical inputs replayed (or refused as divergent)
    pub distinct_inputs: usize,
    /// wall-clock subject latency per run, in run order
    pub compiled_latencies_ms: Vec<u64>,
    /// recorded duration of every matching span
    pub recorded_latencies_ms: Vec<u64>,
    /// every loaded trace of the task, matching spans or not
    pub trace_ids: Vec<String>,
}

/// Everything the reference recorded for one canonical input.
pub struct Recorded {
    pub input: Value,
    /// distinct canonical outputs (absent output records as JSON `null`)
    pub outputs: BTreeSet<String>,
    /// witness count per canonical output — `outputs` with multiplicity,
    /// the evidence a majority pick needs (ADR-0018 amendment)
    pub output_counts: BTreeMap<String, usize>,
    pub observations: usize,
    pub errors: usize,
}

/// The recorded observations of a contract's span scope, grouped by
/// canonical input (deterministic order). What synthesis consumes and what
/// [`differential_check`] replays.
pub struct Gathered {
    /// keyed by canonical input json
    pub groups: BTreeMap<String, Recorded>,
    pub recorded_latencies_ms: Vec<u64>,
    pub trace_ids: Vec<String>,
}

impl Gathered {
    /// Inputs whose recorded behavior is unusable as a reference: any
    /// recorded error, or more than one distinct recorded output.
    pub fn disqualified(&self) -> impl Iterator<Item = (usize, &Recorded)> {
        self.groups
            .values()
            .enumerate()
            .filter(|(_, g)| g.errors > 0 || g.outputs.len() > 1)
    }
}

/// The canonical training witness of one recorded group: its most-witnessed
/// canonical output, ties broken toward the lexicographically smallest
/// canonical string (deterministic; ADR-0018 amendment). `None` when the
/// group recorded any error — an errored reference is never trainable — or
/// witnessed nothing.
pub fn canonical_pick(recorded: &Recorded) -> Option<Value> {
    if recorded.errors > 0 || recorded.observations == 0 {
        return None;
    }
    // max count wins; on equal counts, Reverse(key) makes the smallest
    // canonical string the maximum — keys are unique, so no ambiguity
    let (canonical, _) = recorded
        .output_counts
        .iter()
        .max_by_key(|&(canonical, &count)| (count, std::cmp::Reverse(canonical.as_str())))?;
    Some(serde_json::from_str(canonical).expect("canonical recorded output parses"))
}

/// Training pairs under the majority pick: `(input, pick)` per group with a
/// pick, in canonical input order, plus the count of errored groups skipped.
/// Consuming these instead of refusing divergence is the operator's explicit
/// choice (a CLI flag), never a default — and it selects training data only:
/// the declared agreement threshold remains the acceptance authority at the
/// gate (ADR-0018 amendment).
pub fn pick_observations(gathered: &Gathered) -> (Vec<(Value, Value)>, usize) {
    let mut pairs = Vec::new();
    let mut errored_skipped = 0usize;
    for group in gathered.groups.values() {
        if group.errors > 0 {
            errored_skipped += 1;
            continue;
        }
        if let Some(pick) = canonical_pick(group) {
            pairs.push((group.input.clone(), pick));
        }
    }
    (pairs, errored_skipped)
}

/// Training rows under the weighted-witness pick (ADR-0031): one
/// `(input, output, weight)` row per DISTINCT WITNESSED OUTPUT of every
/// non-errored group, weight = that output's witness count — the full
/// recorded distribution instead of one majority pick. Groups in canonical
/// input order, outputs in canonical order within a group (deterministic,
/// order-independent). Errored groups are skipped and counted, never
/// trainable — exactly [`pick_observations`]'s rule. Like the pick, this
/// selects training DATA only: the declared agreement threshold remains the
/// sole acceptance authority at the differential gate.
pub fn weighted_observations(gathered: &Gathered) -> (Vec<(Value, Value, usize)>, usize) {
    let mut rows = Vec::new();
    let mut errored_skipped = 0usize;
    for group in gathered.groups.values() {
        if group.errors > 0 {
            errored_skipped += 1;
            continue;
        }
        for (canonical, &weight) in &group.output_counts {
            rows.push((
                group.input.clone(),
                serde_json::from_str(canonical).expect("canonical recorded output parses"),
                weight,
            ));
        }
    }
    (rows, errored_skipped)
}

/// A region's recorded chains: the end-to-end [`Gathered`] the differential
/// gate replays, plus the per-stage and per-glue witness pairs synthesis
/// consumes (spec/synthesis.md §8).
pub struct RegionGathered {
    /// end-to-end groups keyed by canonical from-input; recorded latencies
    /// are wall-clock from-start to to-end per chain
    pub gathered: Gathered,
    /// the (kind, name) sequence every recorded chain follows, identically
    pub chain: Vec<(String, String)>,
    /// per chain position: witnessed (input, output) pairs across traces
    pub stage_pairs: Vec<Vec<(Value, Value)>>,
    /// per adjacent pair of positions: witnessed (prev output, next input)
    pub glue_pairs: Vec<Vec<(Value, Value)>>,
}

/// Gather a region scope's recorded chains. Structure rules (v0, all loud):
/// exactly one `from` and one `to` effectful span per trace, `from` before
/// `to`, unique names within the window, an IDENTICAL (kind, name) sequence
/// across every trace. Chain spans may be `model_call` (synthesized stages)
/// or `tool_call` (declared capability boundaries, ADR-0017); env_read /
/// memory_op / branch inside the window still refuse — they are not value
/// transformations a pipeline can carry.
/// A recorded error anywhere in a chain taints that trace's end-to-end
/// observation (counted on the group; its stage/glue pairs are skipped).
pub fn gather_region(store: &Store, contract: &Contract) -> Result<RegionGathered, HarnessError> {
    let Scope::Region { from, to } = &contract.scope else {
        return Err(HarnessError::RegionStructure(
            "gather_region requires a region-scope contract".to_owned(),
        ));
    };
    let traces = store.load_task(&contract.task)?;
    let trace_ids: Vec<String> = traces
        .iter()
        .map(|t| t.header.trace_id.to_string())
        .collect();

    let mut chain_signature: Option<Vec<(String, String)>> = None;
    let mut stage_pairs: Vec<Vec<(Value, Value)>> = Vec::new();
    let mut glue_pairs: Vec<Vec<(Value, Value)>> = Vec::new();
    let mut recorded_latencies_ms = Vec::new();
    let mut groups: BTreeMap<String, Recorded> = BTreeMap::new();
    let mut chains_seen = 0usize;

    for trace in &traces {
        let mut effectful: Vec<&auto_trace::model::Span> = trace
            .spans
            .iter()
            .filter(|s| s.kind.is_effectful())
            .collect();
        effectful.sort_by_key(|s| s.seq);

        let from_positions: Vec<usize> = effectful
            .iter()
            .enumerate()
            .filter(|(_, s)| &s.name == from)
            .map(|(i, _)| i)
            .collect();
        let to_positions: Vec<usize> = effectful
            .iter()
            .enumerate()
            .filter(|(_, s)| &s.name == to)
            .map(|(i, _)| i)
            .collect();
        match (from_positions.as_slice(), to_positions.as_slice()) {
            ([], _) | (_, []) => continue, // this trace has no such chain
            ([f], [t]) if f < t => {
                let chain: Vec<&auto_trace::model::Span> = effectful[*f..=*t].to_vec();
                let signature: Vec<(String, String)> = chain
                    .iter()
                    .map(|s| (s.kind.wire().to_owned(), s.name.clone()))
                    .collect();
                let mut names: Vec<&String> = chain.iter().map(|s| &s.name).collect();
                names.sort();
                names.dedup();
                if names.len() != chain.len() {
                    return Err(HarnessError::RegionStructure(format!(
                        "trace {}: duplicate span names inside the {from}..{to} window",
                        trace.header.trace_id
                    )));
                }
                // model_call stages synthesize; tool_call stages become
                // declared capability boundaries (ADR-0017). Everything else
                // (env_read / memory_op / branch) still refuses: they are not
                // value transformations a pipeline can carry.
                if let Some(impure) = chain
                    .iter()
                    .find(|s| !matches!(s.kind.wire(), "model_call" | "tool_call"))
                {
                    return Err(HarnessError::RegionImpure(format!(
                        "trace {}: span {}({}) inside {from}..{to}",
                        trace.header.trace_id,
                        impure.kind.wire(),
                        impure.name
                    )));
                }
                match &chain_signature {
                    None => chain_signature = Some(signature),
                    Some(expected) if *expected != signature => {
                        return Err(HarnessError::RegionStructure(format!(
                            "trace {}: chain structure {:?} differs from the first recorded \
                             structure {:?} — a region must repeat identically",
                            trace.header.trace_id,
                            signature.iter().map(|(_, n)| n).collect::<Vec<_>>(),
                            expected.iter().map(|(_, n)| n).collect::<Vec<_>>(),
                        )));
                    }
                    Some(_) => {}
                }
                chains_seen += 1;
                if stage_pairs.is_empty() {
                    stage_pairs = vec![Vec::new(); chain.len()];
                    glue_pairs = vec![Vec::new(); chain.len().saturating_sub(1)];
                }

                let first = chain.first().expect("chain has from");
                let last = chain.last().expect("chain has to");
                let errored = chain.iter().any(|s| s.error.is_some());
                let end = last.started_at_ms.saturating_add(last.duration_ms);
                recorded_latencies_ms.push(end.saturating_sub(first.started_at_ms));

                let group = groups
                    .entry(canonical_json(&first.input))
                    .or_insert_with(|| Recorded {
                        input: first.input.clone(),
                        outputs: BTreeSet::new(),
                        output_counts: BTreeMap::new(),
                        observations: 0,
                        errors: 0,
                    });
                group.observations += 1;
                if errored {
                    group.errors += 1;
                    continue; // tainted chain: no reference pairs from it
                }
                let canonical = canonical_json(last.output.as_ref().unwrap_or(&Value::Null));
                *group.output_counts.entry(canonical.clone()).or_default() += 1;
                group.outputs.insert(canonical);

                for (position, span) in chain.iter().enumerate() {
                    stage_pairs[position].push((
                        span.input.clone(),
                        span.output.clone().unwrap_or(Value::Null),
                    ));
                }
                for (position, window) in chain.windows(2).enumerate() {
                    glue_pairs[position].push((
                        window[0].output.clone().unwrap_or(Value::Null),
                        window[1].input.clone(),
                    ));
                }
            }
            ([f], [t]) if f >= t => {
                return Err(HarnessError::RegionStructure(format!(
                    "trace {}: `{to}` records before `{from}`",
                    trace.header.trace_id
                )));
            }
            _ => {
                return Err(HarnessError::RegionStructure(format!(
                    "trace {}: `{from}`/`{to}` must each record exactly once per trace \
                     (found {} and {})",
                    trace.header.trace_id,
                    from_positions.len(),
                    to_positions.len()
                )));
            }
        }
    }

    let Some(chain) = chain_signature else {
        return Err(HarnessError::RegionStructure(format!(
            "no recorded trace contains a {from}..{to} chain — record the task first"
        )));
    };
    debug_assert!(chains_seen > 0);
    Ok(RegionGathered {
        gathered: Gathered {
            groups,
            recorded_latencies_ms,
            trace_ids,
        },
        chain,
        stage_pairs,
        glue_pairs,
    })
}

/// Load and group every recorded observation of the contract's scope.
/// Span scopes group matching spans; region scopes group end-to-end chains
/// (via [`gather_region`]); task scopes group whole-run task-level I/O
/// (ADR-0025) — one observation per trace recording BOTH a task input and a
/// task output, keyed by canonical task input. Task groups never carry
/// errors (there is no task-level error channel: a failed run declares no
/// output and is simply not an observation), and their recorded latency is
/// the wall-clock from run start to the output declaration. Note the emit
/// paths (`auto compile` / `auto distill`) refuse task scope before ever
/// gathering — this function feeding them task groups does not open
/// task-scope compilation.
pub fn gather_observations(store: &Store, contract: &Contract) -> Result<Gathered, HarnessError> {
    if matches!(contract.scope, Scope::Region { .. }) {
        return Ok(gather_region(store, contract)?.gathered);
    }
    let traces = store.load_task(&contract.task)?;
    let trace_ids: Vec<String> = traces
        .iter()
        .map(|t| t.header.trace_id.to_string())
        .collect();

    let mut recorded_latencies_ms = Vec::new();
    // keyed by canonical input: deterministic grouping and label order
    let mut groups: BTreeMap<String, Recorded> = BTreeMap::new();

    if matches!(contract.scope, Scope::Task) {
        for trace in &traces {
            let Some((input, output)) = trace.header.task_observation() else {
                continue;
            };
            recorded_latencies_ms.push(
                output
                    .recorded_at_ms
                    .saturating_sub(trace.header.started_at_ms),
            );
            let group = groups
                .entry(canonical_json(input))
                .or_insert_with(|| Recorded {
                    input: input.clone(),
                    outputs: BTreeSet::new(),
                    output_counts: BTreeMap::new(),
                    observations: 0,
                    errors: 0,
                });
            group.observations += 1;
            let canonical = canonical_json(&output.value);
            *group.output_counts.entry(canonical.clone()).or_default() += 1;
            group.outputs.insert(canonical);
        }
        return Ok(Gathered {
            groups,
            recorded_latencies_ms,
            trace_ids,
        });
    }

    let Scope::Span { kind, name } = &contract.scope else {
        unreachable!("region and task scopes returned above");
    };
    for trace in &traces {
        for span in &trace.spans {
            if span.kind.wire() != kind || &span.name != name {
                continue;
            }
            recorded_latencies_ms.push(span.duration_ms);
            let group = groups
                .entry(canonical_json(&span.input))
                .or_insert_with(|| Recorded {
                    input: span.input.clone(),
                    outputs: BTreeSet::new(),
                    output_counts: BTreeMap::new(),
                    observations: 0,
                    errors: 0,
                });
            group.observations += 1;
            if span.error.is_some() {
                group.errors += 1;
            }
            let canonical = canonical_json(span.output.as_ref().unwrap_or(&Value::Null));
            *group.output_counts.entry(canonical.clone()).or_default() += 1;
            group.outputs.insert(canonical);
        }
    }
    Ok(Gathered {
        groups,
        recorded_latencies_ms,
        trace_ids,
    })
}

/// Replay every distinct recorded input of the contract's scope through
/// `subject`, one check per input, inputs labeled `#0, #1, ...` in canonical
/// input order (deterministic). Task scopes replay distinct recorded task
/// inputs (ADR-0025). Zero matching observations yields a single Unchecked
/// check, never a silent pass.
pub fn differential_check(
    store: &Store,
    contract: &Contract,
    subject: &mut dyn Subject,
) -> Result<DifferentialOutcome, HarnessError> {
    differential_check_with_judge(store, contract, subject, None)
}

/// [`differential_check`] with an optional judge for
/// `differential_match = "judged"` contracts (ADR-0021): byte-divergent
/// groups are arbitrated by the judge and folded into the ADR-0018
/// agreement check — the declared threshold still decides. Exact-mode
/// contracts never touch the judge (bit-for-bit today's behavior).
pub fn differential_check_with_judge(
    store: &Store,
    contract: &Contract,
    subject: &mut dyn Subject,
    judge: Option<&mut (dyn auto_contract::harness::Judge + '_)>,
) -> Result<DifferentialOutcome, HarnessError> {
    if contract.acceptance.differential_match == auto_contract::model::DifferentialMatch::Judged {
        return judged_differential(store, contract, subject, judge);
    }
    let Gathered {
        groups,
        recorded_latencies_ms,
        trace_ids,
    } = gather_observations(store, contract)?;
    let mut checks = Vec::new();
    let mut compiled_latencies_ms = Vec::new();
    if groups.is_empty() {
        let detail = match &contract.scope {
            Scope::Span { kind, name } => format!("no recorded spans match {kind}({name})"),
            Scope::Region { from, to } => format!("no recorded spans match region {from}..{to}"),
            Scope::Task => {
                "no trace records task-level I/O (record with task_input / set_task_output)"
                    .to_owned()
            }
        };
        checks.push(Check {
            what: "differential observations present".to_owned(),
            status: CheckStatus::Unchecked,
            detail: Some(detail),
        });
    }
    for (i, group) in groups.values().enumerate() {
        if group.errors > 0 || group.outputs.len() > 1 {
            checks.push(Check {
                what: format!("recorded outputs agree for input #{i}"),
                status: CheckStatus::Failed,
                detail: Some(format!(
                    "{} distinct recorded output(s), {} recorded error(s) over {} observation(s); subject not run",
                    group.outputs.len(),
                    group.errors,
                    group.observations
                )),
            });
            continue;
        }
        let recorded = group.outputs.first().expect("agreeing group has an output");
        let start = Instant::now();
        let answer = subject.run(&group.input);
        compiled_latencies_ms.push(u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX));
        match answer {
            Err(e) => checks.push(Check {
                what: format!("differential: subject answers input #{i}"),
                status: CheckStatus::Failed,
                detail: Some(format!("subject error: {e}")),
            }),
            Ok(output) if canonical_json(&output) == *recorded => checks.push(Check {
                what: format!(
                    "differential: input #{i} reproduces recorded output ({} observation(s))",
                    group.observations
                ),
                status: CheckStatus::Passed,
                detail: None,
            }),
            Ok(output) => checks.push(Check {
                what: format!("differential: input #{i} reproduces recorded output"),
                status: CheckStatus::Failed,
                detail: Some(format!(
                    "subject: {} != recorded: {}",
                    snippet(&canonical_json(&output)),
                    snippet(recorded)
                )),
            }),
        }
    }

    // Statistical acceptance (ADR-0018): when the contract declares a
    // differential agreement threshold, the per-input reproduction checks
    // become evidence lines and ONE agreement check carries the verdict —
    // matched inputs over eligible inputs against the declared milli
    // threshold. Divergent-reference inputs count as unmatched (they are
    // exactly the stochasticity the threshold exists to price in), never as
    // hard failures. Without a declaration, behavior is unchanged: exact.
    if let Some(min_milli) = contract.acceptance.differential_min_agreement_milli {
        let eligible = checks
            .iter()
            .filter(|c| {
                c.what.starts_with("differential: ") || c.what.starts_with("recorded outputs agree")
            })
            .count();
        let matched = checks
            .iter()
            .filter(|c| c.what.starts_with("differential: ") && c.status == CheckStatus::Passed)
            .count();
        for check in checks.iter_mut().filter(|c| {
            (c.what.starts_with("differential: ") || c.what.starts_with("recorded outputs agree"))
                && c.status == CheckStatus::Failed
        }) {
            check.status = CheckStatus::Passed;
            check.what = format!("[agreement evidence] {}", check.what);
            let note = "counted against the declared agreement threshold, not fatal";
            check.detail = Some(match check.detail.take() {
                Some(detail) => format!("{detail}; {note}"),
                None => note.to_owned(),
            });
        }
        checks.push(auto_contract::harness::agreement_check(
            matched, eligible, min_milli,
        ));
    }

    Ok(DifferentialOutcome {
        checks,
        distinct_inputs: groups.len(),
        compiled_latencies_ms,
        recorded_latencies_ms,
        trace_ids,
    })
}

/// The judged-differential gate branch (ADR-0021). One
/// [`DifferentialComparison`] per distinct replayed input, in canonical
/// input order; errored groups never run the subject (today's invariant);
/// divergent-reference groups DO run it (their canonical pick is the
/// reference — the ADR-0018 rule, never a second one). The harness owns
/// the counting, the evidence lines, and the never-silently-exact rule.
fn judged_differential(
    store: &Store,
    contract: &Contract,
    subject: &mut dyn Subject,
    judge: Option<&mut (dyn auto_contract::harness::Judge + '_)>,
) -> Result<DifferentialOutcome, HarnessError> {
    use auto_contract::harness::DifferentialComparison;

    let min_milli = contract
        .acceptance
        .differential_min_agreement_milli
        .expect("parse rejects judged differential without a declared threshold (ADR-0021)");
    let Gathered {
        groups,
        recorded_latencies_ms,
        trace_ids,
    } = gather_observations(store, contract)?;

    let mut comparisons = Vec::with_capacity(groups.len());
    let mut compiled_latencies_ms = Vec::new();
    for group in groups.values() {
        if group.errors > 0 {
            comparisons.push(DifferentialComparison::ErroredReference {
                errors: group.errors,
                observations: group.observations,
            });
            continue;
        }
        let reference =
            canonical_pick(group).expect("non-errored group with observations has a pick");
        let start = Instant::now();
        let answer = subject.run(&group.input);
        compiled_latencies_ms.push(u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX));
        comparisons.push(match answer {
            Err(error) => DifferentialComparison::SubjectError { error },
            Ok(subject_output) => DifferentialComparison::Compared {
                reference,
                subject_output,
                observations: group.observations,
                distinct_outputs: group.outputs.len(),
            },
        });
    }

    let checks = auto_contract::harness::judged_differential_checks(
        &contract.task,
        &comparisons,
        min_milli,
        judge,
    );
    Ok(DifferentialOutcome {
        checks,
        distinct_inputs: groups.len(),
        compiled_latencies_ms,
        recorded_latencies_ms,
        trace_ids,
    })
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use auto_contract::harness::CallableSubject;
    use auto_contract::{Budgets, Interface};
    use auto_ir::ValueType;
    use auto_trace::model::{Span, SpanId, SpanKind, Trace, TraceHeader, TraceId};
    use serde_json::json;

    use super::*;

    fn m_span(seq: u64, input: Value, output: Value, dur: u64) -> Span {
        Span {
            span_id: SpanId(seq),
            parent_span_id: None,
            seq,
            kind: SpanKind::ModelCall,
            name: "m".into(),
            input,
            output: Some(output),
            error: None,
            started_at_ms: 0,
            duration_ms: dur,
            attrs: BTreeMap::new(),
        }
    }

    fn trace(id: u128, spans: Vec<Span>) -> Trace {
        Trace {
            header: TraceHeader {
                trace_id: TraceId(id),
                task: "t".into(),
                started_at_ms: 0,
                sdk: "test/0".into(),
                attrs: BTreeMap::new(),
                task_input: None,
                task_output: None,
            },
            spans,
        }
    }

    /// A spanless trace whose header carries task-level I/O (ADR-0025).
    fn task_io_trace(id: u128, input: Value, output: Value, recorded_at_ms: u64) -> Trace {
        let mut t = trace(id, vec![]);
        t.header.task_input = Some(input);
        t.header.task_output = Some(auto_trace::TaskOutput {
            value: output,
            recorded_at_ms,
        });
        t
    }

    fn store_with(traces: Vec<Trace>) -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = Store::open(&dir.path().join("t.db")).expect("open");
        for t in traces {
            store.ingest(&t).expect("ingest");
        }
        (dir, store)
    }

    fn contract() -> Contract {
        Contract {
            acceptance: Default::default(),
            task: "t".into(),
            scope: Scope::Span {
                kind: "model_call".into(),
                name: "m".into(),
            },
            interface: Interface {
                input: ValueType::Json,
                output: ValueType::Text,
            },
            examples: vec![],
            properties: vec![],
            budgets: Budgets::default(),
            eval_cases: vec![],
        }
    }

    /// A subject that must never be consulted; returns Err so an accidental
    /// run surfaces as an extra failed check and a latency entry.
    fn must_not_run() -> CallableSubject<impl FnMut(&Value) -> Result<Value, String>> {
        CallableSubject::new("must-not-run", |_: &Value| {
            Err("subject was run on a disqualified input".to_owned())
        })
    }

    #[test]
    fn agreeing_records_and_matching_subject_pass() {
        let (_d, store) = store_with(vec![
            trace(1, vec![m_span(1, json!({"x": 1}), json!("ok"), 7)]),
            trace(2, vec![m_span(1, json!({"x": 1}), json!("ok"), 9)]),
        ]);
        let mut subject = CallableSubject::new("good", |_: &Value| Ok(json!("ok")));
        let outcome = differential_check(&store, &contract(), &mut subject).expect("differential");
        assert_eq!(outcome.distinct_inputs, 1);
        assert_eq!(outcome.trace_ids.len(), 2);
        assert_eq!(outcome.recorded_latencies_ms, vec![7, 9]);
        assert_eq!(outcome.compiled_latencies_ms.len(), 1);
        assert_eq!(outcome.checks.len(), 1);
        assert_eq!(outcome.checks[0].status, CheckStatus::Passed);
        assert!(
            outcome.checks[0].what.contains("2 observation(s)"),
            "{}",
            outcome.checks[0].what
        );
    }

    #[test]
    fn subject_output_mismatch_is_failed() {
        let (_d, store) = store_with(vec![trace(
            1,
            vec![m_span(1, json!({"x": 1}), json!("ok"), 5)],
        )]);
        let mut subject = CallableSubject::new("wrong", |_: &Value| Ok(json!("different")));
        let outcome = differential_check(&store, &contract(), &mut subject).expect("differential");
        assert_eq!(outcome.checks.len(), 1);
        assert_eq!(outcome.checks[0].status, CheckStatus::Failed);
        assert_eq!(
            outcome.checks[0].what,
            "differential: input #0 reproduces recorded output"
        );
    }

    #[test]
    fn recorded_divergence_fails_without_running_subject() {
        let (_d, store) = store_with(vec![
            trace(1, vec![m_span(1, json!({"x": 1}), json!("a"), 5)]),
            trace(2, vec![m_span(1, json!({"x": 1}), json!("b"), 5)]),
        ]);
        let mut subject = must_not_run();
        let outcome = differential_check(&store, &contract(), &mut subject).expect("differential");
        assert_eq!(outcome.checks.len(), 1);
        assert_eq!(outcome.checks[0].status, CheckStatus::Failed);
        assert_eq!(
            outcome.checks[0].what,
            "recorded outputs agree for input #0"
        );
        assert!(
            outcome.compiled_latencies_ms.is_empty(),
            "a latency was measured, so the subject ran"
        );
        assert_eq!(outcome.distinct_inputs, 1);
    }

    #[test]
    fn recorded_error_disqualifies_input_without_running_subject() {
        let mut bad = m_span(1, json!({"x": 1}), json!("ok"), 5);
        bad.error = Some("boom".into());
        bad.output = None;
        let (_d, store) = store_with(vec![trace(1, vec![bad])]);
        let mut subject = must_not_run();
        let outcome = differential_check(&store, &contract(), &mut subject).expect("differential");
        assert_eq!(outcome.checks.len(), 1);
        assert_eq!(outcome.checks[0].status, CheckStatus::Failed);
        assert_eq!(
            outcome.checks[0].what,
            "recorded outputs agree for input #0"
        );
        assert!(outcome.compiled_latencies_ms.is_empty());
    }

    #[test]
    fn subject_error_is_failed() {
        let (_d, store) = store_with(vec![trace(
            1,
            vec![m_span(1, json!({"x": 1}), json!("ok"), 5)],
        )]);
        let mut subject = CallableSubject::new("broken", |_: &Value| Err("kaput".to_owned()));
        let outcome = differential_check(&store, &contract(), &mut subject).expect("differential");
        assert_eq!(outcome.checks.len(), 1);
        assert_eq!(outcome.checks[0].status, CheckStatus::Failed);
        assert_eq!(
            outcome.checks[0].what,
            "differential: subject answers input #0"
        );
        assert_eq!(
            outcome.checks[0].detail.as_deref(),
            Some("subject error: kaput")
        );
        // the failed run was still timed
        assert_eq!(outcome.compiled_latencies_ms.len(), 1);
    }

    #[test]
    fn zero_matching_spans_is_unchecked() {
        let other = Span {
            span_id: SpanId(1),
            parent_span_id: None,
            seq: 1,
            kind: SpanKind::ToolCall,
            name: "other".into(),
            input: json!({}),
            output: Some(json!("x")),
            error: None,
            started_at_ms: 0,
            duration_ms: 1,
            attrs: BTreeMap::new(),
        };
        let (_d, store) = store_with(vec![trace(1, vec![other])]);
        let mut subject = must_not_run();
        let outcome = differential_check(&store, &contract(), &mut subject).expect("differential");
        assert_eq!(outcome.checks.len(), 1);
        assert_eq!(outcome.checks[0].status, CheckStatus::Unchecked);
        assert_eq!(outcome.checks[0].what, "differential observations present");
        assert_eq!(outcome.distinct_inputs, 0);
        assert!(outcome.recorded_latencies_ms.is_empty());
        assert_eq!(outcome.trace_ids.len(), 1);
    }

    #[test]
    fn task_scope_gathers_task_level_groups() {
        // ADR-0025: task scope groups whole-run I/O by canonical task input;
        // the wall-clock (start -> output declaration) is the recorded latency
        let mut c = contract();
        c.scope = Scope::Task;
        let (_d, store) = store_with(vec![
            task_io_trace(1, json!({"doc": "a"}), json!("one"), 7),
            task_io_trace(2, json!({"doc": "a"}), json!("one"), 9),
            task_io_trace(3, json!({"doc": "b"}), json!("two"), 4),
        ]);
        let gathered = gather_observations(&store, &c).expect("task scope gathers");
        assert_eq!(gathered.groups.len(), 2);
        assert_eq!(gathered.recorded_latencies_ms, vec![7, 9, 4]);
        let group = &gathered.groups[&canonical_json(&json!({"doc": "a"}))];
        assert_eq!(group.observations, 2);
        assert_eq!(group.errors, 0);

        let mut subject = CallableSubject::new("echo", |input: &Value| {
            Ok(if input["doc"] == "a" {
                json!("one")
            } else {
                json!("two")
            })
        });
        let outcome = differential_check(&store, &c, &mut subject).expect("differential");
        assert_eq!(outcome.distinct_inputs, 2);
        assert!(
            outcome
                .checks
                .iter()
                .all(|k| k.status == CheckStatus::Passed),
            "{:?}",
            outcome.checks
        );
    }

    #[test]
    fn task_scope_without_task_io_is_unchecked() {
        // spans exist but no header carries task I/O: partial traces (input
        // only) witness nothing; the differential says so instead of passing
        let mut c = contract();
        c.scope = Scope::Task;
        let mut partial = trace(1, vec![m_span(1, json!({"x": 1}), json!("ok"), 5)]);
        partial.header.task_input = Some(json!({"doc": "a"}));
        let (_d, store) = store_with(vec![partial]);
        let mut subject = must_not_run();
        let outcome = differential_check(&store, &c, &mut subject).expect("differential");
        assert_eq!(outcome.checks.len(), 1);
        assert_eq!(outcome.checks[0].status, CheckStatus::Unchecked);
        assert!(
            outcome.checks[0]
                .detail
                .as_deref()
                .unwrap_or("")
                .contains("no trace records task-level I/O"),
            "{:?}",
            outcome.checks[0]
        );
        assert!(outcome.compiled_latencies_ms.is_empty());
    }

    #[test]
    fn inputs_are_labeled_in_canonical_sort_order() {
        // ingested in reverse canonical order to prove sorting, not
        // insertion order, assigns the labels
        let (_d, store) = store_with(vec![
            trace(1, vec![m_span(1, json!({"x": 2}), json!("two"), 5)]),
            trace(2, vec![m_span(1, json!({"x": 1}), json!("one"), 5)]),
        ]);
        // correct on {"x":1} (canonically first), wrong on {"x":2}
        let mut subject = CallableSubject::new("half-right", |input: &Value| {
            if input == &json!({"x": 1}) {
                Ok(json!("one"))
            } else {
                Ok(json!("wrong"))
            }
        });
        let outcome = differential_check(&store, &contract(), &mut subject).expect("differential");
        assert_eq!(outcome.distinct_inputs, 2);
        assert_eq!(outcome.checks.len(), 2);
        assert_eq!(outcome.checks[0].status, CheckStatus::Passed);
        assert!(outcome.checks[0].what.contains("input #0"));
        assert_eq!(outcome.checks[1].status, CheckStatus::Failed);
        assert!(outcome.checks[1].what.contains("input #1"));
    }
    #[test]
    fn declared_acceptance_folds_mismatches_into_one_agreement_check() {
        // 2 recorded inputs; the subject reproduces one and mangles the other.
        // Without acceptance the mismatch is fatal; with min 500 milli the
        // differential passes on measured 1/2 agreement (ADR-0018).
        let mut c = contract();
        let (_dir, store) = store_with(vec![
            trace(1, vec![m_span(1, json!({"n": 1}), json!("one"), 3)]),
            trace(2, vec![m_span(1, json!({"n": 2}), json!("two"), 3)]),
        ]);
        struct Half;
        impl Subject for Half {
            fn describe(&self) -> String {
                "half-agreeing test subject (a labeled fake)".into()
            }
            fn run(&mut self, input: &Value) -> Result<Value, String> {
                if input["n"] == 1 {
                    Ok(Value::String("one".into()))
                } else {
                    Ok(Value::String("wrong".into()))
                }
            }
        }
        let strict = differential_check(&store, &c, &mut Half).expect("runs");
        assert!(
            strict
                .checks
                .iter()
                .any(|k| k.status == CheckStatus::Failed),
            "exact mode fails the mismatch"
        );

        c.acceptance.differential_min_agreement_milli = Some(500);
        let relaxed = differential_check(&store, &c, &mut Half).expect("runs");
        assert!(
            relaxed
                .checks
                .iter()
                .all(|k| k.status != CheckStatus::Failed),
            "declared acceptance folds mismatches into evidence: {:?}",
            relaxed.checks
        );
        let agreement = relaxed
            .checks
            .iter()
            .find(|k| k.what.contains("agreement") && !k.what.starts_with("[agreement evidence]"))
            .expect("one agreement check exists");
        assert_eq!(agreement.status, CheckStatus::Passed);
        assert!(
            agreement.detail.as_deref().unwrap_or("").contains("1/2"),
            "{agreement:?}"
        );

        c.acceptance.differential_min_agreement_milli = Some(900);
        let tight = differential_check(&store, &c, &mut Half).expect("runs");
        let agreement = tight
            .checks
            .iter()
            .find(|k| k.what.contains("agreement") && !k.what.starts_with("[agreement evidence]"))
            .expect("one agreement check exists");
        assert_eq!(agreement.status, CheckStatus::Failed, "1/2 < 900 milli");
    }
}
