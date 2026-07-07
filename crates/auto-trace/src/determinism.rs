//! Determinism analysis: the measured fraction of agent behavior that is
//! secretly symbolic. This number is the public proof of thesis (CLAUDE.md,
//! S1), so its rules are conservative by construction:
//!
//! - spans are grouped by call signature `(kind, name, input_digest)` across
//!   all traces of one task;
//! - a signature is **deterministic** only if it was witnessed at least twice
//!   AND every observation succeeded AND all outputs are identical;
//! - a signature witnessed once is **unwitnessed** — no claim is made about
//!   it, and it never counts toward the deterministic fraction;
//! - fractions are reported over *witnessed* spans only, alongside the
//!   witnessed coverage, and are `None` (rendered as "no data") when nothing
//!   was witnessed. Nothing is extrapolated.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use crate::TraceError;
use crate::model::{CallSignature, SpanKind, Trace};
use crate::store::Store;

/// How one signature behaved across runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureStats {
    pub signature: CallSignature,
    pub observations: usize,
    pub distinct_outputs: usize,
    pub errors: usize,
    pub total_duration_ms: u64,
}

impl SignatureStats {
    /// Deterministic: witnessed ≥2, all succeeded, one output.
    pub fn is_deterministic(&self) -> bool {
        self.observations >= 2 && self.errors == 0 && self.distinct_outputs == 1
    }

    pub fn is_witnessed(&self) -> bool {
        self.observations >= 2
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KindBreakdown {
    pub spans: usize,
    pub deterministic: usize,
    pub divergent: usize,
    pub unwitnessed: usize,
}

/// Task-level determinism (ADR-0025): the same witnessed-≥2 rules as spans,
/// over whole-run observations — one per trace recording BOTH a task input
/// and a task output. Present on a report only when at least one trace of
/// the task carries any task-level I/O; absent otherwise, so reports over
/// pre-ADR-0025 stores render byte-identical.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TaskLevelReport {
    /// traces recording both task input and output
    pub observations: usize,
    /// traces recording exactly one of the two — excluded, counted honestly
    pub partial: usize,
    /// observations whose task input was seen >=2 times across runs
    pub witnessed: usize,
    pub deterministic: usize,
    pub divergent: usize,
    pub unwitnessed: usize,
    /// deterministic / witnessed, by observation count; None if nothing
    /// witnessed
    pub deterministic_fraction_of_witnessed: Option<f64>,
}

/// The determinism report for one task. All numbers are measured over the
/// ingested traces; none are estimates.
#[derive(Debug, Clone, PartialEq)]
pub struct DeterminismReport {
    pub task: String,
    pub traces: usize,
    /// torn-tail partial traces (ADR-0030) excluded from witnessing. Set by
    /// [`report`] from the store; `analyze` over already-loaded traces leaves
    /// it 0 (its inputs are treated as complete). Rendered only when > 0, so a
    /// report over a store with no partial traces stays byte-identical.
    pub excluded_partial_traces: usize,
    /// spans that participate (kind ≠ structural `span`)
    pub effectful_spans: usize,
    /// structural spans excluded from analysis
    pub structural_spans: usize,
    pub witnessed_spans: usize,
    pub deterministic_spans: usize,
    pub divergent_spans: usize,
    pub unwitnessed_spans: usize,
    /// deterministic / witnessed, by span count; None if nothing witnessed
    pub deterministic_fraction_of_witnessed: Option<f64>,
    /// deterministic / witnessed, weighted by recorded duration; None if
    /// witnessed time is zero
    pub deterministic_time_fraction_of_witnessed: Option<f64>,
    pub by_kind: BTreeMap<SpanKind, KindBreakdown>,
    /// divergent signatures, most-observed first (max 10)
    pub top_divergent: Vec<SignatureStats>,
    /// task-level section (ADR-0025); None when no trace carries task I/O
    pub task_level: Option<TaskLevelReport>,
}

/// Analyze already-loaded traces (all of one task).
pub fn analyze(task: &str, traces: &[Trace]) -> DeterminismReport {
    struct Acc {
        observations: usize,
        outputs: BTreeSet<String>,
        errors: usize,
        total_duration_ms: u64,
    }
    let mut by_signature: BTreeMap<CallSignature, Acc> = BTreeMap::new();
    let mut structural_spans = 0usize;

    for trace in traces {
        for span in &trace.spans {
            if !span.kind.is_effectful() {
                structural_spans += 1;
                continue;
            }
            let acc = by_signature.entry(span.signature()).or_insert_with(|| Acc {
                observations: 0,
                outputs: BTreeSet::new(),
                errors: 0,
                total_duration_ms: 0,
            });
            acc.observations += 1;
            acc.outputs.insert(span.output_digest());
            if span.error.is_some() {
                acc.errors += 1;
            }
            acc.total_duration_ms = acc.total_duration_ms.saturating_add(span.duration_ms);
        }
    }

    let mut effectful_spans = 0;
    let mut witnessed_spans = 0;
    let mut deterministic_spans = 0;
    let mut divergent_spans = 0;
    let mut unwitnessed_spans = 0;
    let mut witnessed_time = 0u64;
    let mut deterministic_time = 0u64;
    let mut by_kind: BTreeMap<SpanKind, KindBreakdown> = BTreeMap::new();
    let mut divergent: Vec<SignatureStats> = Vec::new();

    for (signature, acc) in &by_signature {
        let stats = SignatureStats {
            signature: signature.clone(),
            observations: acc.observations,
            distinct_outputs: acc.outputs.len(),
            errors: acc.errors,
            total_duration_ms: acc.total_duration_ms,
        };
        let entry = by_kind.entry(signature.kind).or_default();
        effectful_spans += stats.observations;
        entry.spans += stats.observations;
        if !stats.is_witnessed() {
            unwitnessed_spans += stats.observations;
            entry.unwitnessed += stats.observations;
        } else {
            witnessed_spans += stats.observations;
            witnessed_time = witnessed_time.saturating_add(stats.total_duration_ms);
            if stats.is_deterministic() {
                deterministic_spans += stats.observations;
                deterministic_time = deterministic_time.saturating_add(stats.total_duration_ms);
                entry.deterministic += stats.observations;
            } else {
                divergent_spans += stats.observations;
                entry.divergent += stats.observations;
                divergent.push(stats);
            }
        }
    }

    divergent.sort_by(|a, b| {
        b.observations
            .cmp(&a.observations)
            .then_with(|| a.signature.cmp(&b.signature))
    });
    divergent.truncate(10);

    #[allow(clippy::cast_precision_loss)] // span counts are far below 2^52
    let fraction =
        |num: usize, den: usize| -> Option<f64> { (den > 0).then(|| num as f64 / den as f64) };
    #[allow(clippy::cast_precision_loss)] // durations are far below 2^52 ms
    let time_fraction =
        (witnessed_time > 0).then(|| deterministic_time as f64 / witnessed_time as f64);

    DeterminismReport {
        task: task.to_owned(),
        traces: traces.len(),
        // analyze treats its inputs as complete; report() overrides this from
        // the store's partial-trace count.
        excluded_partial_traces: 0,
        effectful_spans,
        structural_spans,
        witnessed_spans,
        deterministic_spans,
        divergent_spans,
        unwitnessed_spans,
        deterministic_fraction_of_witnessed: fraction(deterministic_spans, witnessed_spans),
        deterministic_time_fraction_of_witnessed: time_fraction,
        by_kind,
        top_divergent: divergent,
        task_level: analyze_task_level(traces),
    }
}

/// Task-level determinism (ADR-0025), by the span rules: group whole-run
/// observations by task-input digest; witnessed ≥2; deterministic iff all
/// witnessed outputs are identical. `None` when no trace carries any
/// task-level I/O — the section never appears for pre-ADR-0025 recordings.
fn analyze_task_level(traces: &[Trace]) -> Option<TaskLevelReport> {
    use crate::model::{canonical_json, digest_hex};

    let mut any_task_io = false;
    let mut partial = 0usize;
    // task-input digest -> (observations, distinct output digests)
    let mut groups: BTreeMap<String, (usize, BTreeSet<String>)> = BTreeMap::new();
    for trace in traces {
        let header = &trace.header;
        if header.task_input.is_some() || header.task_output.is_some() {
            any_task_io = true;
        }
        match header.task_observation() {
            Some((input, output)) => {
                let entry = groups
                    .entry(digest_hex(&canonical_json(input)))
                    .or_insert_with(|| (0, BTreeSet::new()));
                entry.0 += 1;
                entry.1.insert(digest_hex(&canonical_json(&output.value)));
            }
            None if header.task_input.is_some() || header.task_output.is_some() => partial += 1,
            None => {}
        }
    }
    if !any_task_io {
        return None;
    }

    let mut observations = 0usize;
    let mut witnessed = 0usize;
    let mut deterministic = 0usize;
    let mut divergent = 0usize;
    let mut unwitnessed = 0usize;
    for (count, outputs) in groups.values() {
        observations += count;
        if *count < 2 {
            unwitnessed += count;
        } else {
            witnessed += count;
            if outputs.len() == 1 {
                deterministic += count;
            } else {
                divergent += count;
            }
        }
    }
    #[allow(clippy::cast_precision_loss)] // observation counts are far below 2^52
    let fraction = (witnessed > 0).then(|| deterministic as f64 / witnessed as f64);
    Some(TaskLevelReport {
        observations,
        partial,
        witnessed,
        deterministic,
        divergent,
        unwitnessed,
        deterministic_fraction_of_witnessed: fraction,
    })
}

/// Load all traces of `task` from the store and analyze them. Torn-tail
/// partial traces (ADR-0030) are excluded from witnessing and counted
/// separately — never silently folded in, never silently dropped.
pub fn report(store: &Store, task: &str) -> Result<DeterminismReport, TraceError> {
    let loaded = store.load_task_all(task)?;
    let excluded_partial_traces = loaded.iter().filter(|st| st.partial).count();
    let complete: Vec<Trace> = loaded
        .into_iter()
        .filter(|st| !st.partial)
        .map(|st| st.trace)
        .collect();
    let mut report = analyze(task, &complete);
    report.excluded_partial_traces = excluded_partial_traces;
    Ok(report)
}

/// Deterministic human rendering. Not a stable machine format.
pub fn render(r: &DeterminismReport) -> String {
    let mut out = String::new();
    let pct = |f: Option<f64>| match f {
        Some(f) => format!("{:.1}%", f * 100.0),
        None => "no data".to_owned(),
    };
    let _ = writeln!(
        out,
        "determinism report — task \"{}\" ({} trace{})",
        r.task,
        r.traces,
        if r.traces == 1 { "" } else { "s" }
    );
    let _ = writeln!(
        out,
        "effectful spans: {} (structural spans excluded: {})",
        r.effectful_spans, r.structural_spans
    );
    // torn-tail partial traces (ADR-0030) never witness; named only when any
    // exist so reports over stores without them stay byte-identical.
    if r.excluded_partial_traces > 0 {
        let _ = writeln!(
            out,
            "partial traces excluded from witnessing (torn-tail, ADR-0030): {}",
            r.excluded_partial_traces
        );
    }
    let _ = writeln!(
        out,
        "witnessed (signature observed >=2 across runs): {} of {} spans",
        r.witnessed_spans, r.effectful_spans
    );
    let _ = writeln!(
        out,
        "  deterministic: {} spans ({} of witnessed)",
        r.deterministic_spans,
        pct(r.deterministic_fraction_of_witnessed)
    );
    let _ = writeln!(out, "  divergent:     {} spans", r.divergent_spans);
    let _ = writeln!(
        out,
        "  unwitnessed:   {} spans — no claim made",
        r.unwitnessed_spans
    );
    let _ = writeln!(
        out,
        "time-weighted deterministic fraction of witnessed: {}",
        pct(r.deterministic_time_fraction_of_witnessed)
    );
    let _ = writeln!(out, "by kind:");
    if r.by_kind.is_empty() {
        let _ = writeln!(out, "  (none)");
    }
    for (kind, b) in &r.by_kind {
        let _ = writeln!(
            out,
            "  {kind}: {} spans — {} deterministic, {} divergent, {} unwitnessed",
            b.spans, b.deterministic, b.divergent, b.unwitnessed
        );
    }
    if !r.top_divergent.is_empty() {
        let _ = writeln!(out, "top divergent signatures:");
        for s in &r.top_divergent {
            let _ = writeln!(
                out,
                "  {} — {} observations, {} distinct outputs, {} errors",
                s.signature, s.observations, s.distinct_outputs, s.errors
            );
        }
    }
    let _ = writeln!(
        out,
        "(fractions cover witnessed spans only; nothing is extrapolated)"
    );
    // task-level section (ADR-0025): appended only when at least one trace
    // recorded task I/O — reports over stores without it stay byte-identical
    if let Some(t) = &r.task_level {
        let _ = writeln!(
            out,
            "task-level determinism (traces recording task input+output):"
        );
        let _ = writeln!(
            out,
            "  observations: {} (partial task I/O traces excluded: {})",
            t.observations, t.partial
        );
        let _ = writeln!(
            out,
            "  witnessed (task input observed >=2 across runs): {} of {} observations",
            t.witnessed, t.observations
        );
        let _ = writeln!(
            out,
            "  deterministic: {} observations ({} of witnessed)",
            t.deterministic,
            pct(t.deterministic_fraction_of_witnessed)
        );
        let _ = writeln!(out, "  divergent:     {} observations", t.divergent);
        let _ = writeln!(
            out,
            "  unwitnessed:   {} observations — no claim made",
            t.unwitnessed
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::model::{Span, SpanId, TraceHeader, TraceId};

    fn span(seq: u64, kind: SpanKind, name: &str, input: &str, output: &str, dur: u64) -> Span {
        Span {
            span_id: SpanId(seq),
            parent_span_id: None,
            seq,
            kind,
            name: name.into(),
            input: serde_json::json!({ "v": input }),
            output: Some(serde_json::json!(output)),
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

    fn task_io_trace(id: u128, input: &str, output: &str) -> Trace {
        let mut t = trace(id, vec![]);
        t.header.task_input = Some(serde_json::json!({ "doc": input }));
        t.header.task_output = Some(crate::model::TaskOutput {
            value: serde_json::json!(output),
            recorded_at_ms: 50,
        });
        t
    }

    #[test]
    fn deterministic_requires_two_witnesses_and_equal_outputs() {
        let t1 = trace(
            1,
            vec![
                span(1, SpanKind::ToolCall, "stable", "x", "same", 10),
                span(2, SpanKind::ToolCall, "flaky", "x", "a", 10),
                span(3, SpanKind::ToolCall, "once", "x", "whatever", 10),
            ],
        );
        let t2 = trace(
            2,
            vec![
                span(1, SpanKind::ToolCall, "stable", "x", "same", 30),
                span(2, SpanKind::ToolCall, "flaky", "x", "b", 10),
            ],
        );
        let r = analyze("t", &[t1, t2]);
        assert_eq!(r.effectful_spans, 5);
        assert_eq!(r.witnessed_spans, 4);
        assert_eq!(r.deterministic_spans, 2); // both "stable" observations
        assert_eq!(r.divergent_spans, 2); // both "flaky" observations
        assert_eq!(r.unwitnessed_spans, 1); // "once" — no claim
        assert_eq!(r.deterministic_fraction_of_witnessed, Some(0.5));
        // time: witnessed = 10+30+10+10 = 60, deterministic = 40
        assert_eq!(
            r.deterministic_time_fraction_of_witnessed,
            Some(40.0 / 60.0)
        );
        assert_eq!(r.top_divergent.len(), 1);
        assert_eq!(r.top_divergent[0].signature.name, "flaky");
    }

    #[test]
    fn errors_disqualify_determinism() {
        let mut errored = span(1, SpanKind::ToolCall, "e", "x", "same", 1);
        errored.error = Some("boom".into());
        let ok = span(1, SpanKind::ToolCall, "e", "x", "same", 1);
        let r = analyze("t", &[trace(1, vec![errored]), trace(2, vec![ok])]);
        assert_eq!(r.deterministic_spans, 0);
        assert_eq!(r.divergent_spans, 2);
    }

    #[test]
    fn different_inputs_are_different_signatures() {
        let t1 = trace(1, vec![span(1, SpanKind::ToolCall, "f", "x", "1", 1)]);
        let t2 = trace(2, vec![span(1, SpanKind::ToolCall, "f", "y", "2", 1)]);
        let r = analyze("t", &[t1, t2]);
        // two unwitnessed signatures — honest: nothing claimed
        assert_eq!(r.unwitnessed_spans, 2);
        assert_eq!(r.deterministic_fraction_of_witnessed, None);
    }

    #[test]
    fn structural_spans_are_excluded() {
        let t = trace(
            1,
            vec![
                span(1, SpanKind::Span, "wrapper", "x", "y", 100),
                span(2, SpanKind::ToolCall, "f", "x", "1", 1),
            ],
        );
        let r = analyze("t", &[t]);
        assert_eq!(r.structural_spans, 1);
        assert_eq!(r.effectful_spans, 1);
    }

    #[test]
    fn empty_input_renders_no_data_not_zero() {
        let r = analyze("t", &[]);
        assert_eq!(r.deterministic_fraction_of_witnessed, None);
        let text = render(&r);
        assert!(text.contains("no data"));
        assert!(!text.contains("NaN"));
    }

    // --- task-level section (ADR-0025) ----------------------------------

    #[test]
    fn no_task_io_means_no_task_section() {
        let r = analyze(
            "t",
            &[trace(
                1,
                vec![span(1, SpanKind::ToolCall, "f", "x", "1", 1)],
            )],
        );
        assert_eq!(r.task_level, None);
        assert!(!render(&r).contains("task-level"));
    }

    #[test]
    fn task_level_follows_span_rules() {
        let traces = vec![
            task_io_trace(1, "a", "same"),
            task_io_trace(2, "a", "same"),
            task_io_trace(3, "b", "x"),
            task_io_trace(4, "b", "y"),
            task_io_trace(5, "c", "once"),
        ];
        let t = analyze("t", &traces).task_level.expect("section present");
        assert_eq!(t.observations, 5);
        assert_eq!(t.partial, 0);
        assert_eq!(t.witnessed, 4);
        assert_eq!(t.deterministic, 2); // both "a" observations
        assert_eq!(t.divergent, 2); // both "b" observations
        assert_eq!(t.unwitnessed, 1); // "c" — no claim
        assert_eq!(t.deterministic_fraction_of_witnessed, Some(0.5));
        let text = render(&analyze("t", &traces));
        assert!(text.contains("task-level determinism"), "{text}");
        assert!(
            text.contains("deterministic: 2 observations (50.0% of witnessed)"),
            "{text}"
        );
    }

    #[test]
    fn partial_task_io_is_counted_not_witnessed() {
        // input without output: honest partial, never an observation
        let mut partial_only = trace(1, vec![]);
        partial_only.header.task_input = Some(serde_json::json!("in"));
        let r = analyze("t", &[partial_only]);
        let t = r.task_level.expect("any task I/O opens the section");
        assert_eq!(t.observations, 0);
        assert_eq!(t.partial, 1);
        assert_eq!(t.deterministic_fraction_of_witnessed, None);
        let text = render(&r);
        assert!(
            text.contains("observations: 0 (partial task I/O traces excluded: 1)"),
            "{text}"
        );
    }

    #[test]
    fn span_numbers_are_untouched_by_task_io() {
        // the span-level lines must render byte-identical with and without
        // task I/O present in the same traces
        let spans = || vec![span(1, SpanKind::ToolCall, "f", "x", "1", 1)];
        let without = analyze("t", &[trace(1, spans()), trace(2, spans())]);
        let mut a = trace(1, spans());
        a.header.task_input = Some(serde_json::json!("in"));
        a.header.task_output = Some(crate::model::TaskOutput {
            value: serde_json::json!("out"),
            recorded_at_ms: 9,
        });
        let with = analyze("t", &[a, trace(2, spans())]);
        let span_lines = |text: String| -> String {
            text.lines()
                .take_while(|l| !l.starts_with("task-level"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert_eq!(span_lines(render(&without)), span_lines(render(&with)));
        assert_eq!(without.task_level, None);
        assert!(with.task_level.is_some());
    }

    // --- partial-trace exclusion (ADR-0030) -----------------------------

    fn open_temp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(&dir.path().join("t.db")).expect("open store");
        (dir, store)
    }

    #[test]
    fn report_excludes_partial_traces_from_witnessing_and_counts_them() {
        let (_dir, mut store) = open_temp_store();
        // two COMPLETE runs witness a stable signature -> deterministic
        store
            .ingest(&trace(
                1,
                vec![span(1, SpanKind::ToolCall, "f", "x", "same", 1)],
            ))
            .unwrap();
        store
            .ingest(&trace(
                2,
                vec![span(1, SpanKind::ToolCall, "f", "x", "same", 1)],
            ))
            .unwrap();
        // a PARTIAL run whose same-signature output DIVERGES: if it were counted
        // it would flip the signature to divergent. It must not be.
        store
            .ingest_partial(&trace(
                3,
                vec![span(1, SpanKind::ToolCall, "f", "x", "DIFF", 1)],
            ))
            .unwrap();

        let r = report(&store, "t").unwrap();
        assert_eq!(r.excluded_partial_traces, 1);
        assert_eq!(r.traces, 2, "only complete traces are analyzed");
        assert_eq!(
            r.deterministic_spans, 2,
            "the partial's divergence excluded"
        );
        assert_eq!(r.divergent_spans, 0);

        let text = render(&r);
        assert!(
            text.contains("partial traces excluded from witnessing (torn-tail, ADR-0030): 1"),
            "{text}"
        );
    }

    #[test]
    fn report_without_partials_has_no_partial_line() {
        let (_dir, mut store) = open_temp_store();
        store
            .ingest(&trace(
                1,
                vec![span(1, SpanKind::ToolCall, "f", "x", "1", 1)],
            ))
            .unwrap();
        store
            .ingest(&trace(
                2,
                vec![span(1, SpanKind::ToolCall, "f", "x", "1", 1)],
            ))
            .unwrap();
        let r = report(&store, "t").unwrap();
        assert_eq!(r.excluded_partial_traces, 0);
        assert!(!render(&r).contains("partial traces excluded"));
    }

    #[test]
    fn report_over_only_partial_traces_witnesses_nothing() {
        let (_dir, mut store) = open_temp_store();
        store
            .ingest_partial(&trace(
                1,
                vec![span(1, SpanKind::ToolCall, "f", "x", "1", 1)],
            ))
            .unwrap();
        // task exists (not UnknownTask), but no complete trace witnesses anything
        let r = report(&store, "t").unwrap();
        assert_eq!(r.excluded_partial_traces, 1);
        assert_eq!(r.traces, 0);
        assert_eq!(r.deterministic_fraction_of_witnessed, None);
    }
}
