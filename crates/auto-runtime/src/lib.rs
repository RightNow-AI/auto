//! Auto runtime — tier-1 invocation (S3) plus the runtime guard (first S6
//! slice).
//!
//! Real here: loading a `.cbin` artifact's wasm module and executing it on
//! JSON values, with two hard rules — a module whose imports exceed its
//! declared capabilities is refused at load (v0: only pure, zero-import
//! modules exist, so any import is a refusal), and execution runs under
//! fuel + memory limits so a runaway module cannot hang verification.
//!
//! Also real: guards ([`guard`]) — nearest-witness OOD distance
//! (trigram-set Jaccard, wire v0/v1; opt-in dense trigram-hash cosine
//! embeddings, wire v2, ADR-0023), thresholds calibrated by the same
//! split-conformal quantile over leave-one-out scores (ADR-0014, with the
//! exchangeability caveat stated there, not glossed). Still lexical and
//! says so — v2 upgrades the geometry, not the meaning; semantic
//! embeddings are the recorded upgrade.
//!
//! Partly here now (honest bounds): [`tier0`] is the tier-0 frontier
//! binding — `Tier0Spec` (the `--tier0` spec grammar) and `frontier_answer`
//! (a spend-capped frontier model as the deopt target). Still not here: the
//! deopt *orchestration* — guard-trip → tier-0 → trace-capture → recompile —
//! which lives in `auto run` (crates/auto-cli), not this crate.
//!
//! The module ABI is specified in spec/artifact.md §ABI.

pub mod executor;
pub mod guard;
pub mod runner;
pub mod tier0;

pub use executor::{ExecError, HostTools, ToolCallback, WasmExecutor, WasmSubject};
pub use guard::{EmbeddingGuard, Guard, GuardError, GuardOutcome};
pub use runner::Runner;
