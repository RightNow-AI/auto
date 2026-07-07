//! Auto passes — S4 slice: **automated symbolic extraction**.
//!
//! Real here: bottom-up enumerative synthesis over the closed extraction DSL
//! (`auto-dsl`), searching for a program that reproduces every distinct
//! recorded observation of a deterministic span — plus the generic wasm
//! interpreter (built by `build.rs`, embedded via [`interpreter_wasm`]) that
//! executes synthesized programs inside artifacts.
//!
//! Honest bounds, stated plainly:
//! - the search is **enumerative, not LLM-guided** — LLM proposal generation
//!   is the intended upgrade and requires authorized model spend;
//! - a synthesized program is evidence-bounded: it reproduces the witnessed
//!   observations, and confidence grows with *distinct* inputs (with one
//!   distinct input, constant behavior is indistinguishable from computation
//!   — spec/synthesis.md §honesty);
//! - distillation (S5 slice) drives an **external trainer** (v0: a single
//!   sklearn decision tree — spec/distillation.md) and reports only the
//!   trainer's own measured metrics; neural specialists are future work;
//! - optimization passes are not here and nothing pretends they are.

pub mod extraction;

// LLM-guided CEGIS proposal generation — the ADR-0005 recorded upgrade to
// symbolic extraction: proposals under the spend cap (ADR-0010), the checker
// (`auto_dsl::eval`) and the emit gate unchanged.
pub mod extraction_llm;

// region synthesis — a recorded chain of spans compiled into one pipeline:
// every stage and every glue edge is its own synthesis problem (ADR-0015)
pub mod region;

// re-exported so integration tests and downstream crates use the exact DSL
// and model formats this crate works against
pub use auto_dsl;
pub use auto_model;
pub use extraction::{Observation, SearchBudget, SearchOutcome, Synthesis, synthesize};
pub use extraction_llm::{CegisConfig, CegisOutcome, synthesize_llm};
pub use region::{RegionOutcome, synthesize_region};

/// The generic DSL interpreter as a wasm module (frozen artifact ABI with the
/// additive `init` extension), compiled by build.rs from
/// `crates/auto-passes/dsl-interpreter` — the exact evaluator the synthesizer
/// used natively, compiled for the artifact.
pub fn interpreter_wasm() -> &'static [u8] {
    include_bytes!(concat!(env!("OUT_DIR"), "/dsl_interpreter.wasm"))
}

/// The distilled-model interpreter as a wasm module (same ABI + `init`
/// extension; the payload is model json) — auto-model's inference compiled
/// for artifacts by build.rs from `crates/auto-passes/model-interpreter`.
pub fn model_interpreter_wasm() -> &'static [u8] {
    include_bytes!(concat!(env!("OUT_DIR"), "/model_interpreter.wasm"))
}

/// The MLP interpreter as a wasm module (same ABI + `init` extension; the
/// payload is mlp json) — auto-model's neural inference compiled for
/// artifacts by build.rs from `crates/auto-passes/mlp-interpreter`.
pub fn mlp_interpreter_wasm() -> &'static [u8] {
    include_bytes!(concat!(env!("OUT_DIR"), "/mlp_interpreter.wasm"))
}

/// The CAPABILITY build of the generic DSL interpreter: the same evaluator
/// with the `auto.tool_call` import declared (ADR-0017). Embedded only in
/// artifacts whose pipelines carry tool stages; pure artifacts embed
/// [`interpreter_wasm`] and stay zero-import.
pub fn tool_interpreter_wasm() -> &'static [u8] {
    include_bytes!(concat!(env!("OUT_DIR"), "/dsl_tool_interpreter.wasm"))
}

// S5: distillation orchestration — external-trainer driver, parity-gated by
// the trainer's own measured metrics (see the module docs).
pub mod distillation;

pub use distillation::{DistillError, Distilled, TrainerMetrics, distill, distill_validated};
