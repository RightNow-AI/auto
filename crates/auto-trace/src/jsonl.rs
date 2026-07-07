//! Strict parser for the v0 JSONL trace emission format (spec/trace.md).
//!
//! One file = one trace. The first non-blank line is the trace header
//! (`"t":"trace"`); every later line is a span (`"t":"span"`) or the at-most-
//! once task output declaration (`"t":"task_output"`, ADR-0025). Lines are
//! written at span *close*, so line order is close order; `seq` (assigned at
//! span *open*) is the authoritative order and the parser sorts by it.
//! Unknown fields, unknown kinds, version ≠ 0, and cross-trace lines are
//! rejected — no best-effort reads. The schema additions for task-level I/O
//! (optional header `task_input`, the `task_output` line) are named optional
//! extensions of the strict schema, not tolerance of unknown fields: a file
//! that does not use them is byte-identical to the pre-ADR-0025 format.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use crate::TraceError;
use crate::model::{Span, SpanId, SpanKind, TaskOutput, Trace, TraceHeader, TraceId};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HeaderLine {
    v: u32,
    #[allow(dead_code)] // the tag was already inspected to pick this struct
    t: String,
    trace_id: String,
    task: String,
    started_at_ms: u64,
    sdk: String,
    #[serde(default)]
    attrs: BTreeMap<String, String>,
    /// optional task-level input (ADR-0025); JSON `null` reads as absent
    #[serde(default)]
    task_input: Option<Value>,
}

/// The task output declaration line (ADR-0025). Emitted by `set_task_output`
/// as its own line because the header line is already on disk when the agent
/// finally knows its output — the stream is append-only by design.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskOutputLine {
    v: u32,
    #[allow(dead_code)]
    t: String,
    trace_id: String,
    output: Value,
    recorded_at_ms: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SpanLine {
    v: u32,
    #[allow(dead_code)]
    t: String,
    trace_id: String,
    span_id: u64,
    #[serde(default)]
    parent_span_id: Option<u64>,
    seq: u64,
    kind: String,
    name: String,
    input: Value,
    #[serde(default)]
    output: Option<Value>,
    #[serde(default)]
    error: Option<String>,
    started_at_ms: u64,
    duration_ms: u64,
    #[serde(default)]
    attrs: BTreeMap<String, String>,
}

/// Parse a whole trace file. Strict: any malformed line fails the parse.
pub fn parse_file(path: &Path) -> Result<Trace, TraceError> {
    parse_str(&std::fs::read_to_string(path)?)
}

/// Serialize a trace to the v0 JSONL emission format (header first, spans in
/// `seq` order, then the task output line iff one was declared).
/// `parse_str(&to_jsonl(t)) == t` for any valid trace. Task I/O fields are
/// emitted only when present — a trace without them serializes byte-identical
/// to the pre-ADR-0025 format.
pub fn to_jsonl(trace: &Trace) -> String {
    let mut out = String::new();
    let mut header = serde_json::json!({
        "v": 0,
        "t": "trace",
        "trace_id": trace.header.trace_id.to_string(),
        "task": trace.header.task,
        "started_at_ms": trace.header.started_at_ms,
        "sdk": trace.header.sdk,
        "attrs": trace.header.attrs,
    });
    if let Some(task_input) = &trace.header.task_input {
        header
            .as_object_mut()
            .expect("json! object literal")
            .insert("task_input".to_owned(), task_input.clone());
    }
    out.push_str(&header.to_string());
    out.push('\n');
    for span in &trace.spans {
        let line = serde_json::json!({
            "v": 0,
            "t": "span",
            "trace_id": trace.header.trace_id.to_string(),
            "span_id": span.span_id.0,
            "parent_span_id": span.parent_span_id.map(|p| p.0),
            "seq": span.seq,
            "kind": span.kind.wire(),
            "name": span.name,
            "input": span.input,
            "output": span.output,
            "error": span.error,
            "started_at_ms": span.started_at_ms,
            "duration_ms": span.duration_ms,
            "attrs": span.attrs,
        });
        out.push_str(&line.to_string());
        out.push('\n');
    }
    if let Some(task_output) = &trace.header.task_output {
        let line = serde_json::json!({
            "v": 0,
            "t": "task_output",
            "trace_id": trace.header.trace_id.to_string(),
            "output": task_output.value,
            "recorded_at_ms": task_output.recorded_at_ms,
        });
        out.push_str(&line.to_string());
        out.push('\n');
    }
    out
}

/// Parse trace file content. See module docs for the accepted format.
pub fn parse_str(content: &str) -> Result<Trace, TraceError> {
    let mut header: Option<TraceHeader> = None;
    let mut spans: Vec<Span> = Vec::new();
    let mut task_output: Option<TaskOutput> = None;

    for (idx, raw) in content.lines().enumerate() {
        let line_no = idx + 1;
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(raw).map_err(|e| TraceError::BadJson {
            line: line_no,
            err: e.to_string(),
        })?;
        let tag = value.get("t").and_then(Value::as_str).unwrap_or_default();
        match tag {
            "trace" => {
                if header.is_some() {
                    return Err(TraceError::DuplicateHeader { line: line_no });
                }
                let h: HeaderLine =
                    serde_json::from_value(value).map_err(|e| TraceError::BadJson {
                        line: line_no,
                        err: e.to_string(),
                    })?;
                if h.v != 0 {
                    return Err(TraceError::UnsupportedVersion {
                        line: line_no,
                        found: h.v,
                    });
                }
                let trace_id =
                    TraceId::parse(&h.trace_id).ok_or(TraceError::BadTraceId { line: line_no })?;
                header = Some(TraceHeader {
                    trace_id,
                    task: h.task,
                    started_at_ms: h.started_at_ms,
                    sdk: h.sdk,
                    attrs: h.attrs,
                    task_input: h.task_input,
                    task_output: None, // folded in from the task_output line
                });
            }
            "span" => {
                let Some(header) = header.as_ref() else {
                    return Err(TraceError::FirstLineNotHeader { line: line_no });
                };
                let s: SpanLine =
                    serde_json::from_value(value).map_err(|e| TraceError::BadJson {
                        line: line_no,
                        err: e.to_string(),
                    })?;
                if s.v != 0 {
                    return Err(TraceError::UnsupportedVersion {
                        line: line_no,
                        found: s.v,
                    });
                }
                let line_trace_id =
                    TraceId::parse(&s.trace_id).ok_or(TraceError::BadTraceId { line: line_no })?;
                if line_trace_id != header.trace_id {
                    return Err(TraceError::TraceIdMismatch { line: line_no });
                }
                let kind = SpanKind::from_wire(&s.kind).ok_or(TraceError::UnknownKind {
                    line: line_no,
                    kind: s.kind.clone(),
                })?;
                spans.push(Span {
                    span_id: SpanId(s.span_id),
                    parent_span_id: s.parent_span_id.map(SpanId),
                    seq: s.seq,
                    kind,
                    name: s.name,
                    input: s.input,
                    output: s.output,
                    error: s.error,
                    started_at_ms: s.started_at_ms,
                    duration_ms: s.duration_ms,
                    attrs: s.attrs,
                });
            }
            "task_output" => {
                let Some(header) = header.as_ref() else {
                    return Err(TraceError::FirstLineNotHeader { line: line_no });
                };
                if task_output.is_some() {
                    return Err(TraceError::DuplicateTaskOutput { line: line_no });
                }
                let o: TaskOutputLine =
                    serde_json::from_value(value).map_err(|e| TraceError::BadJson {
                        line: line_no,
                        err: e.to_string(),
                    })?;
                if o.v != 0 {
                    return Err(TraceError::UnsupportedVersion {
                        line: line_no,
                        found: o.v,
                    });
                }
                let line_trace_id =
                    TraceId::parse(&o.trace_id).ok_or(TraceError::BadTraceId { line: line_no })?;
                if line_trace_id != header.trace_id {
                    return Err(TraceError::TraceIdMismatch { line: line_no });
                }
                task_output = Some(TaskOutput {
                    value: o.output,
                    recorded_at_ms: o.recorded_at_ms,
                });
            }
            other => {
                return Err(TraceError::UnknownLineType {
                    line: line_no,
                    t: other.to_owned(),
                });
            }
        }
    }

    let Some(mut header) = header else {
        return Err(TraceError::EmptyFile);
    };
    header.task_output = task_output;

    spans.sort_by_key(|s| s.seq);
    validate_spans(&spans)?;
    Ok(Trace { header, spans })
}

/// A trace parsed in RECOVERY mode (ADR-0030), plus what — if anything — was
/// dropped to obtain it. `dropped == None` means the file was fully valid and
/// this is exactly the strict [`parse_str`] result; `Some` means a torn final
/// line was dropped and the trace is **partial**.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredTrace {
    pub trace: Trace,
    pub dropped: Option<DroppedTail>,
}

impl RecoveredTrace {
    /// Whether recovery dropped a torn tail — i.e. the trace is partial.
    pub fn is_partial(&self) -> bool {
        self.dropped.is_some()
    }
}

/// Description of the torn final line a recovery pass dropped (ADR-0030).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedTail {
    /// 1-based physical line number of the torn (unterminated) final line.
    pub line: usize,
    /// bytes dropped: the length of the unterminated final line.
    pub bytes: usize,
    /// why the tail would not parse (truncated JSON, mid-UTF-8, ...).
    pub reason: String,
}

/// Parse a trace file in RECOVERY mode (ADR-0030). Identical to [`parse_file`]
/// on a fully valid file (returns it with `dropped: None`), but a single torn
/// FINAL line — the hard-kill-mid-write artifact: an *unterminated* last line
/// that will not parse — is dropped, and the recovered prefix is returned
/// marked partial (`dropped: Some`). A torn MIDDLE line, a *newline-terminated*
/// corrupt final line (the write completed; the corruption is not a torn
/// tail), or a torn header (no committed content to recover) all stay strict
/// errors even here. Recovery is never the default — callers opt in explicitly
/// and the strict [`parse_file`] / [`parse_str`] path is unchanged.
pub fn parse_file_recovering(path: &Path) -> Result<RecoveredTrace, TraceError> {
    parse_bytes_recovering(&std::fs::read(path)?)
}

/// [`parse_file_recovering`] over in-memory bytes. Takes `&[u8]`, not `&str`,
/// because a torn tail may be truncated mid-UTF-8: the committed prefix (up to
/// and including the last newline) is always valid UTF-8 — a `\n` byte never
/// falls inside a multibyte sequence — but the dropped tail need not be.
pub fn parse_bytes_recovering(bytes: &[u8]) -> Result<RecoveredTrace, TraceError> {
    // Split at the last newline: `prefix` is every committed (newline-
    // terminated) line; `tail` is whatever follows the last newline — the
    // unterminated final line, empty when the file ends cleanly.
    let (prefix, tail) = match bytes.iter().rposition(|&b| b == b'\n') {
        Some(i) => (&bytes[..=i], &bytes[i + 1..]),
        None => (&bytes[..0], bytes),
    };

    // No unterminated tail (the file ends with a newline, or has only trailing
    // whitespace after the last one): nothing is torn. Parse the whole thing
    // strictly — byte-identical to `parse_str`, so middle-line AND last-line
    // corruption in a properly terminated file stay strict errors.
    if tail.iter().all(u8::is_ascii_whitespace) {
        return Ok(RecoveredTrace {
            trace: parse_str(committed_utf8(bytes)?)?,
            dropped: None,
        });
    }

    // There is an unterminated final line. If the whole file is still valid
    // UTF-8 and strict-parses, that line was a COMPLETE record that merely lost
    // its trailing newline (killed after the bytes, before the `\n`): keep it,
    // not partial. `str::lines()` already yields a final line without a
    // trailing newline, so this matches the strict reading exactly.
    if let Ok(s) = std::str::from_utf8(bytes)
        && let Ok(trace) = parse_str(s)
    {
        return Ok(RecoveredTrace {
            trace,
            dropped: None,
        });
    }

    // The final line is torn. Recover the committed prefix strictly: if the
    // prefix itself fails, the corruption is in a committed (middle) line, not
    // the tail — a strict error even in recovery mode, so it propagates. A torn
    // header leaves an empty prefix -> `EmptyFile` (nothing to recover), also
    // propagated: a partial trace still needs a committed header to exist.
    let trace = parse_str(committed_utf8(prefix)?)?;
    Ok(RecoveredTrace {
        trace,
        dropped: Some(DroppedTail {
            line: prefix.iter().filter(|&&b| b == b'\n').count() + 1,
            bytes: tail.len(),
            reason: describe_torn_tail(tail),
        }),
    })
}

/// Interpret committed trace bytes as UTF-8, mirroring `parse_file`'s
/// `read_to_string`: non-UTF-8 in a committed (newline-terminated) line is
/// corruption, not a torn tail, so it is a loud error, never a silent drop.
fn committed_utf8(bytes: &[u8]) -> Result<&str, TraceError> {
    std::str::from_utf8(bytes).map_err(|e| {
        TraceError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("committed trace bytes are not valid UTF-8: {e}"),
        ))
    })
}

/// Human description of why the dropped tail would not parse.
fn describe_torn_tail(tail: &[u8]) -> String {
    match std::str::from_utf8(tail) {
        Err(_) => "unterminated final line: invalid UTF-8 (truncated mid-character)".to_owned(),
        Ok(s) => match serde_json::from_str::<Value>(s.trim()) {
            Err(e) => format!("unterminated final line: truncated JSON ({e})"),
            Ok(_) => "unterminated final line dropped".to_owned(),
        },
    }
}

/// Structural invariants shared by the parser and the store loader:
/// unique seq (strictly increasing once sorted), unique span ids, parents
/// exist and opened earlier.
pub(crate) fn validate_spans(spans: &[Span]) -> Result<(), TraceError> {
    let mut ids: BTreeSet<SpanId> = BTreeSet::new();
    for pair in spans.windows(2) {
        if pair[0].seq == pair[1].seq {
            return Err(TraceError::DuplicateSeq(pair[0].seq));
        }
    }
    for span in spans {
        if !ids.insert(span.span_id) {
            return Err(TraceError::DuplicateSpanId(span.span_id));
        }
    }
    let seq_of: BTreeMap<SpanId, u64> = spans.iter().map(|s| (s.span_id, s.seq)).collect();
    for span in spans {
        if let Some(parent) = span.parent_span_id {
            let Some(&parent_seq) = seq_of.get(&parent) else {
                return Err(TraceError::UnknownParent {
                    span: span.span_id,
                    parent,
                });
            };
            if parent_seq >= span.seq {
                return Err(TraceError::ParentNotEarlier {
                    span: span.span_id,
                    seq: span.seq,
                    parent,
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_line() -> String {
        r#"{"v":0,"t":"trace","trace_id":"00000000000000000000000000000001","task":"toy","started_at_ms":1000,"sdk":"test/0","attrs":{}}"#.to_owned()
    }

    fn span_line(span_id: u64, seq: u64, kind: &str) -> String {
        format!(
            r#"{{"v":0,"t":"span","trace_id":"00000000000000000000000000000001","span_id":{span_id},"parent_span_id":null,"seq":{seq},"kind":"{kind}","name":"n","input":{{"a":1}},"output":"ok","error":null,"started_at_ms":1000,"duration_ms":5,"attrs":{{}}}}"#
        )
    }

    #[test]
    fn parses_header_and_spans_sorted_by_seq() {
        // close order (line order) is reversed relative to open order (seq)
        let content = format!(
            "{}\n{}\n{}\n",
            header_line(),
            span_line(2, 2, "tool_call"),
            span_line(1, 1, "span")
        );
        let trace = parse_str(&content).unwrap();
        assert_eq!(trace.header.task, "toy");
        assert_eq!(trace.spans.len(), 2);
        assert_eq!(trace.spans[0].seq, 1);
        assert_eq!(trace.spans[1].seq, 2);
    }

    #[test]
    fn header_only_is_a_valid_empty_trace() {
        let trace = parse_str(&header_line()).unwrap();
        assert!(trace.spans.is_empty());
    }

    #[test]
    fn empty_file_rejected() {
        assert!(matches!(parse_str(""), Err(TraceError::EmptyFile)));
        assert!(matches!(parse_str("\n\n"), Err(TraceError::EmptyFile)));
    }

    #[test]
    fn span_before_header_rejected() {
        let content = span_line(1, 1, "tool_call");
        assert!(matches!(
            parse_str(&content),
            Err(TraceError::FirstLineNotHeader { line: 1 })
        ));
    }

    #[test]
    fn duplicate_header_rejected() {
        let content = format!("{}\n{}", header_line(), header_line());
        assert!(matches!(
            parse_str(&content),
            Err(TraceError::DuplicateHeader { line: 2 })
        ));
    }

    #[test]
    fn version_1_rejected() {
        let content = header_line().replace(r#""v":0"#, r#""v":1"#);
        assert!(matches!(
            parse_str(&content),
            Err(TraceError::UnsupportedVersion { found: 1, .. })
        ));
    }

    #[test]
    fn unknown_kind_rejected() {
        let content = format!("{}\n{}", header_line(), span_line(1, 1, "quantum_call"));
        assert!(matches!(
            parse_str(&content),
            Err(TraceError::UnknownKind { .. })
        ));
    }

    #[test]
    fn unknown_fields_rejected() {
        let content = header_line().replace(r#""attrs":{}"#, r#""attrs":{},"extra":1"#);
        assert!(matches!(
            parse_str(&content),
            Err(TraceError::BadJson { .. })
        ));
    }

    #[test]
    fn cross_trace_span_rejected() {
        let foreign = span_line(1, 1, "tool_call").replace(
            "00000000000000000000000000000001",
            "00000000000000000000000000000002",
        );
        let content = format!("{}\n{foreign}", header_line());
        assert!(matches!(
            parse_str(&content),
            Err(TraceError::TraceIdMismatch { .. })
        ));
    }

    #[test]
    fn duplicate_span_id_rejected() {
        let content = format!(
            "{}\n{}\n{}",
            header_line(),
            span_line(1, 1, "tool_call"),
            span_line(1, 2, "tool_call")
        );
        assert!(matches!(
            parse_str(&content),
            Err(TraceError::DuplicateSpanId(SpanId(1)))
        ));
    }

    #[test]
    fn duplicate_seq_rejected() {
        let content = format!(
            "{}\n{}\n{}",
            header_line(),
            span_line(1, 1, "tool_call"),
            span_line(2, 1, "tool_call")
        );
        assert!(matches!(
            parse_str(&content),
            Err(TraceError::DuplicateSeq(1))
        ));
    }

    #[test]
    fn parent_must_open_earlier() {
        let child = span_line(1, 1, "tool_call")
            .replace(r#""parent_span_id":null"#, r#""parent_span_id":2"#);
        let parent = span_line(2, 2, "span");
        let content = format!("{}\n{child}\n{parent}", header_line());
        assert!(matches!(
            parse_str(&content),
            Err(TraceError::ParentNotEarlier { .. })
        ));
    }

    #[test]
    fn unknown_parent_rejected() {
        let orphan = span_line(1, 1, "tool_call")
            .replace(r#""parent_span_id":null"#, r#""parent_span_id":99"#);
        let content = format!("{}\n{orphan}", header_line());
        assert!(matches!(
            parse_str(&content),
            Err(TraceError::UnknownParent { .. })
        ));
    }

    #[test]
    fn blank_lines_are_tolerated() {
        let content = format!(
            "\n{}\n\n{}\n\n",
            header_line(),
            span_line(1, 1, "tool_call")
        );
        assert_eq!(parse_str(&content).unwrap().spans.len(), 1);
    }

    // --- task-level I/O (ADR-0025) -------------------------------------

    fn task_output_line() -> String {
        r#"{"v":0,"t":"task_output","trace_id":"00000000000000000000000000000001","output":{"ok":true},"recorded_at_ms":1500}"#.to_owned()
    }

    #[test]
    fn task_io_parses_into_the_header() {
        let header =
            header_line().replace(r#""attrs":{}"#, r#""attrs":{},"task_input":{"doc":"d"}"#);
        let content = format!(
            "{header}\n{}\n{}\n",
            span_line(1, 1, "tool_call"),
            task_output_line()
        );
        let trace = parse_str(&content).unwrap();
        assert_eq!(
            trace.header.task_input,
            Some(serde_json::json!({"doc": "d"}))
        );
        let out = trace.header.task_output.as_ref().unwrap();
        assert_eq!(out.value, serde_json::json!({"ok": true}));
        assert_eq!(out.recorded_at_ms, 1500);
        assert_eq!(trace.spans.len(), 1);
    }

    #[test]
    fn task_io_roundtrips_and_absence_is_byte_identical() {
        // with task I/O: parse(to_jsonl(t)) == t
        let header = header_line().replace(r#""attrs":{}"#, r#""attrs":{},"task_input":"in""#);
        let content = format!("{header}\n{}\n", task_output_line());
        let trace = parse_str(&content).unwrap();
        assert_eq!(parse_str(&to_jsonl(&trace)).unwrap(), trace);

        // without task I/O: emitted bytes carry no new field and no new line
        let plain = parse_str(&format!(
            "{}\n{}\n",
            header_line(),
            span_line(1, 1, "tool_call")
        ))
        .unwrap();
        let emitted = to_jsonl(&plain);
        assert!(!emitted.contains("task_input"));
        assert!(!emitted.contains("task_output"));
        assert_eq!(emitted.lines().count(), 2);
    }

    #[test]
    fn null_task_input_reads_as_absent() {
        // JSON null is not a recordable task input; None and null conflate
        let header = header_line().replace(r#""attrs":{}"#, r#""attrs":{},"task_input":null"#);
        let trace = parse_str(&header).unwrap();
        assert_eq!(trace.header.task_input, None);
    }

    #[test]
    fn duplicate_task_output_rejected() {
        let content = format!(
            "{}\n{}\n{}",
            header_line(),
            task_output_line(),
            task_output_line()
        );
        assert!(matches!(
            parse_str(&content),
            Err(TraceError::DuplicateTaskOutput { line: 3 })
        ));
    }

    #[test]
    fn task_output_before_header_rejected() {
        assert!(matches!(
            parse_str(&task_output_line()),
            Err(TraceError::FirstLineNotHeader { line: 1 })
        ));
    }

    #[test]
    fn cross_trace_task_output_rejected() {
        let foreign = task_output_line().replace(
            "00000000000000000000000000000001",
            "00000000000000000000000000000002",
        );
        let content = format!("{}\n{foreign}", header_line());
        assert!(matches!(
            parse_str(&content),
            Err(TraceError::TraceIdMismatch { line: 2 })
        ));
    }

    #[test]
    fn task_output_unknown_fields_and_bad_version_rejected() {
        let extra = task_output_line().replace(
            r#""recorded_at_ms":1500"#,
            r#""recorded_at_ms":1500,"extra":1"#,
        );
        let content = format!("{}\n{extra}", header_line());
        assert!(matches!(
            parse_str(&content),
            Err(TraceError::BadJson { .. })
        ));

        let v1 = task_output_line().replace(r#""v":0"#, r#""v":1"#);
        let content = format!("{}\n{v1}", header_line());
        assert!(matches!(
            parse_str(&content),
            Err(TraceError::UnsupportedVersion { found: 1, .. })
        ));
    }

    // --- torn-tail recovery (ADR-0030) ----------------------------------

    /// A valid, newline-terminated two-line file.
    fn valid_file() -> String {
        format!("{}\n{}\n", header_line(), span_line(1, 1, "tool_call"))
    }

    #[test]
    fn recovering_a_valid_file_is_never_partial_and_equals_strict() {
        let content = valid_file();
        let rec = parse_bytes_recovering(content.as_bytes()).unwrap();
        assert_eq!(rec.dropped, None);
        assert!(!rec.is_partial());
        // byte-identical outcome: recovery of a valid file == the strict parse
        assert_eq!(rec.trace, parse_str(&content).unwrap());
    }

    #[test]
    fn valid_file_missing_final_newline_is_kept_not_partial() {
        // a kill after the final record's bytes but before its '\n' left a
        // COMPLETE last line without a terminator — nothing was lost.
        let content = format!("{}\n{}", header_line(), span_line(1, 1, "tool_call"));
        assert!(!content.ends_with('\n'));
        let rec = parse_bytes_recovering(content.as_bytes()).unwrap();
        assert_eq!(rec.dropped, None);
        assert_eq!(rec.trace, parse_str(&content).unwrap());
        assert_eq!(rec.trace.spans.len(), 1);
    }

    #[test]
    fn header_only_missing_newline_recovers_clean() {
        let rec = parse_bytes_recovering(header_line().as_bytes()).unwrap();
        assert_eq!(rec.dropped, None);
        assert!(rec.trace.spans.is_empty());
    }

    #[test]
    fn torn_final_line_is_dropped_and_prefix_recovered() {
        // header + one committed span + a truncated (unterminated) final span
        let torn = &span_line(2, 2, "tool_call")[..40]; // arbitrary mid-line cut
        let content = format!(
            "{}\n{}\n{torn}",
            header_line(),
            span_line(1, 1, "tool_call")
        );
        // strict refuses the whole file
        assert!(matches!(
            parse_str(&content),
            Err(TraceError::BadJson { .. })
        ));
        // recovery keeps the committed prefix, marked partial
        let rec = parse_bytes_recovering(content.as_bytes()).unwrap();
        assert!(rec.is_partial());
        let dropped = rec.dropped.unwrap();
        assert_eq!(dropped.line, 3, "torn line is the third physical line");
        assert_eq!(dropped.bytes, torn.len());
        assert_eq!(rec.trace.spans.len(), 1, "only the committed span survives");
        assert_eq!(rec.trace.header.task, "toy");
    }

    #[test]
    fn truncation_at_every_offset_in_the_final_line_recovers_the_prefix() {
        // A valid file whose LAST line carries a multibyte character, truncated
        // at every byte offset strictly inside that final line — including
        // offsets that split the multibyte character (invalid-UTF-8 tail).
        let last = span_line(2, 2, "tool_call").replace(r#""name":"n""#, r#""name":"café-日""#);
        let prefix = format!("{}\n{}\n", header_line(), span_line(1, 1, "tool_call"));
        let full = format!("{prefix}{last}");
        let prefix_len = prefix.len();
        let strict_prefix = parse_str(&prefix).unwrap();

        // every cut that leaves a non-empty, non-whitespace tail is a torn tail
        for cut in (prefix_len + 1)..full.len() {
            let bytes = &full.as_bytes()[..cut];
            let tail = &bytes[prefix_len..];
            if tail.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let rec = parse_bytes_recovering(bytes)
                .unwrap_or_else(|e| panic!("recovery failed at cut {cut}: {e}"));
            assert!(rec.is_partial(), "cut {cut} should be partial");
            assert_eq!(rec.trace, strict_prefix, "cut {cut} recovers the prefix");
            let dropped = rec.dropped.unwrap();
            assert_eq!(dropped.line, 3);
            assert_eq!(dropped.bytes, tail.len());
        }
    }

    #[test]
    fn mid_utf8_truncation_reports_invalid_utf8() {
        let last = span_line(2, 2, "tool_call").replace(r#""name":"n""#, r#""name":"日""#);
        let prefix = format!("{}\n", header_line());
        let full = format!("{prefix}{last}");
        // cut one byte into the 3-byte '日' at the end of the string
        let idx = full.rfind('日').unwrap();
        let bytes = &full.as_bytes()[..idx + 1];
        assert!(
            std::str::from_utf8(bytes).is_err(),
            "tail splits a codepoint"
        );
        let rec = parse_bytes_recovering(bytes).unwrap();
        assert!(rec.is_partial());
        assert!(rec.dropped.unwrap().reason.contains("invalid UTF-8"));
        assert!(rec.trace.spans.is_empty());
    }

    #[test]
    fn torn_middle_line_stays_fatal_in_both_modes() {
        // a truncated line in the MIDDLE (a committed, newline-terminated span
        // follows it): recovery must NOT rescue this — only a torn TAIL.
        let content = format!(
            "{}\n{{\"v\":0,\"t\":\"spa\n{}\n",
            header_line(),
            span_line(1, 1, "tool_call")
        );
        assert!(matches!(
            parse_str(&content),
            Err(TraceError::BadJson { line: 2, .. })
        ));
        assert!(matches!(
            parse_bytes_recovering(content.as_bytes()),
            Err(TraceError::BadJson { line: 2, .. })
        ));
    }

    #[test]
    fn newline_terminated_corrupt_final_line_stays_fatal_in_both_modes() {
        // the final line is corrupt but PROPERLY TERMINATED (ends with '\n'):
        // the write completed, so this is genuine corruption, not a torn tail.
        let content = format!("{}\n{{\"v\":0,\"t\":\"spa\n", header_line());
        assert!(content.ends_with('\n'));
        assert!(matches!(
            parse_str(&content),
            Err(TraceError::BadJson { line: 2, .. })
        ));
        assert!(matches!(
            parse_bytes_recovering(content.as_bytes()),
            Err(TraceError::BadJson { line: 2, .. })
        ));
    }

    #[test]
    fn torn_header_has_nothing_to_recover() {
        // the header itself was being written when killed: no committed prefix
        let content = r#"{"v":0,"t":"trac"#; // truncated, no newline
        assert!(matches!(
            parse_bytes_recovering(content.as_bytes()),
            Err(TraceError::EmptyFile)
        ));
    }

    #[test]
    fn recovering_empty_and_blank_matches_strict() {
        assert!(matches!(
            parse_bytes_recovering(b""),
            Err(TraceError::EmptyFile)
        ));
        assert!(matches!(
            parse_bytes_recovering(b"\n\n"),
            Err(TraceError::EmptyFile)
        ));
    }
}
