//! Auto contract v0 — the contract IS the type system (CLAUDE.md).
//!
//! A contract declares examples + properties + eval sets + budgets for a
//! task or one operation signature. The verification harness checks a
//! subject — recorded traces today, compiled artifacts from S3 on — against
//! the contract and produces a three-valued verdict:
//!
//! - **Pass** — every normative claim was checked and held;
//! - **Fail** — something checked and violated (a failing contract blocks
//!   emit, no exceptions);
//! - **Inconclusive** — nothing violated, but something normative could not
//!   be checked (unwitnessed example, unmeasurable budget). Inconclusive is
//!   never rounded up to Pass.
//!
//! Format spec: `spec/contract.md`. Eval runs are content-addressed records
//! written by `evalrun.rs`; their ids are what manifests may cite (S3+).

pub mod conform;
pub mod evalrun;
pub mod harness;
pub mod model;
pub mod parse;
pub mod properties;

pub use model::{
    Budgets, CONTRACT_VERSION, Contract, ContractId, EvalCase, Example, Interface, MatchMode,
    Property, Scope, Target,
};
pub use parse::ContractError;
