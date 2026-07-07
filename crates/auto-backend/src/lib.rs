//! Auto backend — the artifact side of the compiler (S3 slice).
//!
//! Real here: the `.cbin` container (deterministic named-entry format,
//! content-addressed), the manifest (measured guarantees only, never
//! aspiration), differential checks against recorded observations, and the
//! **emit gate**: an artifact is written only for a `Pass` verdict — Fail
//! and Inconclusive both block, mechanically (CLAUDE.md: failing contract
//! blocks emit, no exceptions).
//!
//! Not here yet (honest bounds): synthesis of the module (S4 — modules are
//! hand-supplied at S3), wasm *component model* packaging, model artifacts
//! (onnx/gguf, S5), signing (S7). Specs: spec/artifact.md.

pub mod container;
pub mod differential;
pub mod emit;
pub mod manifest;

pub use container::{Artifact, ContainerError, GRAPH_ENTRY, MAGIC, MANIFEST_ENTRY, MODULE_ENTRY};
pub use manifest::{MANIFEST_VERSION, Manifest, ManifestError, Measured, Provenance};
