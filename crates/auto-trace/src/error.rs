use crate::model::{SpanId, TraceId};

/// Any failure in parsing, storing, or loading traces.
#[derive(Debug, thiserror::Error)]
pub enum TraceError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("trace file is empty")]
    EmptyFile,
    #[error("line {line}: invalid json: {err}")]
    BadJson { line: usize, err: String },
    #[error("line {line}: first line must be the trace header (t=\"trace\")")]
    FirstLineNotHeader { line: usize },
    #[error("line {line}: duplicate trace header")]
    DuplicateHeader { line: usize },
    #[error(
        "line {line}: unknown line type {t:?} (expected \"trace\", \"span\", or \"task_output\")"
    )]
    UnknownLineType { line: usize, t: String },
    #[error(
        "line {line}: duplicate task_output line — a run declares its task output exactly once"
    )]
    DuplicateTaskOutput { line: usize },
    #[error(
        "line {line}: unsupported trace format version {found}; this build reads exactly version 0"
    )]
    UnsupportedVersion { line: usize, found: u32 },
    #[error("line {line}: malformed trace id (expected 32 hex chars)")]
    BadTraceId { line: usize },
    #[error("line {line}: span trace_id does not match the header")]
    TraceIdMismatch { line: usize },
    #[error("line {line}: unknown span kind {kind:?}")]
    UnknownKind { line: usize, kind: String },
    #[error("duplicate span id {0}")]
    DuplicateSpanId(SpanId),
    #[error("duplicate seq {0}")]
    DuplicateSeq(u64),
    #[error("span {span} references unknown parent {parent}")]
    UnknownParent { span: SpanId, parent: SpanId },
    #[error("span {span} (seq {seq}) has parent {parent} that did not open earlier")]
    ParentNotEarlier {
        span: SpanId,
        seq: u64,
        parent: SpanId,
    },
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("store schema version {found} unsupported; this build supports exactly {supported}")]
    StoreVersionMismatch { found: i64, supported: i64 },
    #[error("trace {0} already ingested")]
    DuplicateTrace(TraceId),
    #[error("unknown trace {0}")]
    UnknownTrace(TraceId),
    #[error("no traces recorded for task {0:?}")]
    UnknownTask(String),
    #[error("value out of range for storage: {what}")]
    ValueOutOfRange { what: &'static str },
    #[error("corrupt store: {why}")]
    CorruptStore { why: String },
}
