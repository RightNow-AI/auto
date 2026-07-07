//! Auto trace core — the measured side of the compiler.
//!
//! S1 scope (see CLAUDE.md, build spine): the trace model, strict JSONL
//! ingestion of SDK-recorded runs, the sqlite trace store, replay comparison,
//! and the determinism report — the measured fraction of agent behavior that
//! is secretly symbolic.
//!
//! Everything here reports measurements over recorded data; nothing is
//! estimated or extrapolated. The wire format and semantics are specified in
//! `spec/trace.md`.

mod error;

pub mod determinism;
pub mod jsonl;
pub mod model;
pub mod replay;
pub mod store;

pub use error::TraceError;
pub use jsonl::{DroppedTail, RecoveredTrace};
pub use model::{CallSignature, Span, SpanId, SpanKind, TaskOutput, Trace, TraceHeader, TraceId};
pub use store::{Store, StoredTrace};
