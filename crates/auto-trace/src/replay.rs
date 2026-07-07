//! Replay comparison: given the trace of an original run and the trace of a
//! replay run (produced by an SDK in replay mode, where recorded outputs are
//! substituted for live calls), verify the agent took the same path.
//!
//! Comparison walks the *effectful* spans of both traces in `seq` order and
//! reports the first divergence. Structural `span` nodes are ignored — only
//! observable behavior counts. Task-level I/O (ADR-0025) plays no role in
//! replay matching: it is a whole-run record, not a call the replayed agent
//! makes.

use std::fmt::Write as _;

use crate::model::{CallSignature, Trace};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Divergence {
    /// replay made a different call (or same call with different input)
    SignatureMismatch {
        index: usize,
        original: CallSignature,
        replay: CallSignature,
    },
    /// same call, same input, different output — the world leaked in
    OutputMismatch {
        index: usize,
        signature: CallSignature,
    },
    /// one run stopped early / kept going
    LengthMismatch { original: usize, replay: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayReport {
    /// effectful span counts
    pub original_spans: usize,
    pub replay_spans: usize,
    /// pairs compared before stopping
    pub compared: usize,
    /// first divergence, if any; comparison stops there because alignment
    /// is lost afterwards
    pub divergence: Option<Divergence>,
}

impl ReplayReport {
    pub fn matched(&self) -> bool {
        self.divergence.is_none()
    }
}

/// Compare an original trace with its replay.
pub fn compare(original: &Trace, replay: &Trace) -> ReplayReport {
    let orig: Vec<_> = original
        .spans
        .iter()
        .filter(|s| s.kind.is_effectful())
        .collect();
    let repl: Vec<_> = replay
        .spans
        .iter()
        .filter(|s| s.kind.is_effectful())
        .collect();

    let mut compared = 0;
    let mut divergence = None;
    for (index, (o, r)) in orig.iter().zip(repl.iter()).enumerate() {
        compared += 1;
        let (os, rs) = (o.signature(), r.signature());
        if os != rs {
            divergence = Some(Divergence::SignatureMismatch {
                index,
                original: os,
                replay: rs,
            });
            break;
        }
        if o.output_digest() != r.output_digest() {
            divergence = Some(Divergence::OutputMismatch {
                index,
                signature: os,
            });
            break;
        }
    }
    if divergence.is_none() && orig.len() != repl.len() {
        divergence = Some(Divergence::LengthMismatch {
            original: orig.len(),
            replay: repl.len(),
        });
    }
    ReplayReport {
        original_spans: orig.len(),
        replay_spans: repl.len(),
        compared,
        divergence,
    }
}

/// Deterministic human rendering. Not a stable machine format.
pub fn render(r: &ReplayReport) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "replay comparison: {} original / {} replay effectful spans, {} compared",
        r.original_spans, r.replay_spans, r.compared
    );
    match &r.divergence {
        None => {
            let _ = writeln!(out, "result: MATCH — replay reproduced the recorded path");
        }
        Some(Divergence::SignatureMismatch {
            index,
            original,
            replay,
        }) => {
            let _ = writeln!(
                out,
                "result: DIVERGED at effectful span {index}: original {original} vs replay {replay}"
            );
        }
        Some(Divergence::OutputMismatch { index, signature }) => {
            let _ = writeln!(
                out,
                "result: DIVERGED at effectful span {index}: {signature} produced a different output"
            );
        }
        Some(Divergence::LengthMismatch { original, replay }) => {
            let _ = writeln!(
                out,
                "result: DIVERGED — original has {original} effectful spans, replay has {replay}"
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::model::{Span, SpanId, SpanKind, TraceHeader, TraceId};

    fn span(seq: u64, kind: SpanKind, name: &str, input: u64, output: &str) -> Span {
        Span {
            span_id: SpanId(seq),
            parent_span_id: None,
            seq,
            kind,
            name: name.into(),
            input: serde_json::json!({ "k": input }),
            output: Some(serde_json::json!(output)),
            error: None,
            started_at_ms: 0,
            duration_ms: 1,
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

    #[test]
    fn identical_paths_match() {
        let a = trace(
            1,
            vec![
                span(1, SpanKind::Span, "wrap", 0, "x"),
                span(2, SpanKind::ToolCall, "f", 1, "one"),
            ],
        );
        let b = trace(
            2,
            vec![span(1, SpanKind::ToolCall, "f", 1, "one")], // no wrapper: structural ignored
        );
        let r = compare(&a, &b);
        assert!(r.matched(), "{r:?}");
    }

    #[test]
    fn different_call_is_signature_mismatch() {
        let a = trace(1, vec![span(1, SpanKind::ToolCall, "f", 1, "one")]);
        let b = trace(2, vec![span(1, SpanKind::ToolCall, "g", 1, "one")]);
        assert!(matches!(
            compare(&a, &b).divergence,
            Some(Divergence::SignatureMismatch { index: 0, .. })
        ));
    }

    #[test]
    fn same_call_different_output_is_output_mismatch() {
        let a = trace(1, vec![span(1, SpanKind::ToolCall, "f", 1, "one")]);
        let b = trace(2, vec![span(1, SpanKind::ToolCall, "f", 1, "two")]);
        assert!(matches!(
            compare(&a, &b).divergence,
            Some(Divergence::OutputMismatch { index: 0, .. })
        ));
    }

    #[test]
    fn extra_calls_are_length_mismatch() {
        let a = trace(1, vec![span(1, SpanKind::ToolCall, "f", 1, "one")]);
        let b = trace(
            2,
            vec![
                span(1, SpanKind::ToolCall, "f", 1, "one"),
                span(2, SpanKind::ToolCall, "f", 2, "two"),
            ],
        );
        assert_eq!(
            compare(&a, &b).divergence,
            Some(Divergence::LengthMismatch {
                original: 1,
                replay: 2
            })
        );
    }
}
