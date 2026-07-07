use std::collections::BTreeMap;
use std::fmt;

use serde_json::Value;
use sha2::{Digest, Sha256};

/// Stable identity of one recorded run. Minted by the SDK (random 128-bit),
/// never by this crate. Rendered as 32 lowercase hex chars.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TraceId(pub u128);

/// Identity of one span within a trace. Assigned by the SDK at span open,
/// starting at 1, dense or not — only uniqueness is required.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SpanId(pub u64);

impl fmt::Display for TraceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}

impl TraceId {
    /// Parse exactly 32 lowercase-or-uppercase hex chars.
    pub fn parse(s: &str) -> Option<Self> {
        if s.len() != 32 {
            return None;
        }
        u128::from_str_radix(s, 16).ok().map(Self)
    }
}

impl fmt::Display for SpanId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "s{}", self.0)
    }
}

/// What a span records. `Span` is a structural grouping node; the rest are
/// the effectful leaves that determinism analysis and replay care about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SpanKind {
    ModelCall,
    ToolCall,
    EnvRead,
    MemoryOp,
    Branch,
    /// structural grouping only; excluded from replay and determinism
    Span,
}

impl SpanKind {
    /// Wire string used in JSONL and the store.
    pub fn wire(self) -> &'static str {
        match self {
            Self::ModelCall => "model_call",
            Self::ToolCall => "tool_call",
            Self::EnvRead => "env_read",
            Self::MemoryOp => "memory_op",
            Self::Branch => "branch",
            Self::Span => "span",
        }
    }

    pub fn from_wire(s: &str) -> Option<Self> {
        Some(match s {
            "model_call" => Self::ModelCall,
            "tool_call" => Self::ToolCall,
            "env_read" => Self::EnvRead,
            "memory_op" => Self::MemoryOp,
            "branch" => Self::Branch,
            "span" => Self::Span,
            _ => return None,
        })
    }

    /// Effectful spans participate in determinism analysis and replay;
    /// structural `span` nodes do not.
    pub fn is_effectful(self) -> bool {
        !matches!(self, Self::Span)
    }
}

impl fmt::Display for SpanKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.wire())
    }
}

/// A task-level output declaration (ADR-0025): the value plus when the
/// recorder's own clock saw the agent declare it (`set_task_output`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskOutput {
    pub value: Value,
    /// unix epoch milliseconds at declaration, same clock as the header's
    /// `started_at_ms` — their difference is the run's task-level wall-clock
    pub recorded_at_ms: u64,
}

/// Run-level metadata. One per trace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceHeader {
    pub trace_id: TraceId,
    /// task label; determinism analysis groups traces sharing a task
    pub task: String,
    /// unix epoch milliseconds, as measured by the recorder
    pub started_at_ms: u64,
    /// recorder identification, e.g. "auto-sdk-python/0.1.0"
    pub sdk: String,
    pub attrs: BTreeMap<String, String>,
    /// task-level input, recorded at tracer construction (ADR-0025).
    /// `None` = not recorded; JSON `null` is not a recordable task input.
    pub task_input: Option<Value>,
    /// task-level output, declared exactly once via the SDK's
    /// `set_task_output`. `None` = never declared.
    pub task_output: Option<TaskOutput>,
}

impl TraceHeader {
    /// The task-level observation this run witnessed: present iff BOTH the
    /// task input and the task output were recorded. Runs carrying only one
    /// of the two witness nothing at task level (callers count them as
    /// partial, honestly, rather than inventing the missing half).
    pub fn task_observation(&self) -> Option<(&Value, &TaskOutput)> {
        match (&self.task_input, &self.task_output) {
            (Some(input), Some(output)) => Some((input, output)),
            _ => None,
        }
    }
}

/// One recorded operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub span_id: SpanId,
    /// enclosing span, if any; the parent always has a smaller `seq`
    pub parent_span_id: Option<SpanId>,
    /// open order within the trace, starting at 1, strictly increasing.
    /// JSONL line order is close order — not `seq` order.
    pub seq: u64,
    pub kind: SpanKind,
    /// tool name / model name / env var name / branch label / memory op
    pub name: String,
    pub input: Value,
    /// `None` and JSON `null` are deliberately conflated (wire is JSON)
    pub output: Option<Value>,
    pub error: Option<String>,
    pub started_at_ms: u64,
    pub duration_ms: u64,
    pub attrs: BTreeMap<String, String>,
}

/// Grouping key for determinism analysis: same operation, same input.
///
/// Digests are computed by THIS implementation over canonical JSON and are
/// never wire data — cross-language digest equality is never required
/// (spec/trace.md "digests").
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CallSignature {
    pub kind: SpanKind,
    pub name: String,
    pub input_digest: String,
}

impl fmt::Display for CallSignature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}({}) input={}",
            self.kind,
            self.name,
            &self.input_digest[..12]
        )
    }
}

impl Span {
    pub fn input_digest(&self) -> String {
        digest_hex(&canonical_json(&self.input))
    }

    /// Digest of the output; absent output digests as JSON `null` (the wire
    /// cannot distinguish them).
    pub fn output_digest(&self) -> String {
        match &self.output {
            Some(v) => digest_hex(&canonical_json(v)),
            None => digest_hex("null"),
        }
    }

    pub fn signature(&self) -> CallSignature {
        CallSignature {
            kind: self.kind,
            name: self.name.clone(),
            input_digest: self.input_digest(),
        }
    }
}

/// One fully-parsed, validated recorded run. Spans are in strictly
/// increasing `seq` order (enforced by the jsonl parser and the store).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trace {
    pub header: TraceHeader,
    pub spans: Vec<Span>,
}

/// Canonical JSON: sorted object keys (serde_json's default map is ordered),
/// compact separators. Canonical only within this implementation — see
/// `CallSignature` docs.
pub fn canonical_json(v: &Value) -> String {
    serde_json::to_string(v).expect("serde_json::Value serialization cannot fail")
}

/// Lowercase-hex sha-256.
pub fn digest_hex(s: &str) -> String {
    let hash = Sha256::digest(s.as_bytes());
    let mut out = String::with_capacity(64);
    for byte in hash {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_id_roundtrips_hex() {
        let id = TraceId(0xdead_beef_0000_0000_0000_0000_0000_0001);
        assert_eq!(TraceId::parse(&id.to_string()), Some(id));
        assert_eq!(TraceId::parse("xyz"), None);
        assert_eq!(TraceId::parse(""), None);
    }

    #[test]
    fn canonical_json_sorts_object_keys() {
        let a: Value = serde_json::from_str(r#"{"b":1,"a":2}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"a":2,"b":1}"#).unwrap();
        assert_eq!(canonical_json(&a), canonical_json(&b));
        assert_eq!(canonical_json(&a), r#"{"a":2,"b":1}"#);
    }

    #[test]
    fn digest_is_stable() {
        // pinned: a silent change to canonicalization or hashing would
        // silently reshuffle every signature
        assert_eq!(
            digest_hex("null"),
            "74234e98afe7498fb5daf1f36ac2d78acc339464f950703b8c019892f982b90b"
        );
    }

    #[test]
    fn output_digest_conflates_none_and_null() {
        let mut s = Span {
            span_id: SpanId(1),
            parent_span_id: None,
            seq: 1,
            kind: SpanKind::ToolCall,
            name: "t".into(),
            input: Value::Null,
            output: None,
            error: None,
            started_at_ms: 0,
            duration_ms: 0,
            attrs: BTreeMap::new(),
        };
        let none_digest = s.output_digest();
        s.output = Some(Value::Null);
        assert_eq!(s.output_digest(), none_digest);
    }

    #[test]
    fn kind_wire_roundtrips() {
        for k in [
            SpanKind::ModelCall,
            SpanKind::ToolCall,
            SpanKind::EnvRead,
            SpanKind::MemoryOp,
            SpanKind::Branch,
            SpanKind::Span,
        ] {
            assert_eq!(SpanKind::from_wire(k.wire()), Some(k));
        }
        assert_eq!(SpanKind::from_wire("nope"), None);
    }
}
