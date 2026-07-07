//! The artifact manifest — measured guarantees only, never aspiration.
//!
//! Every number here is either measured during the compile that emitted the
//! artifact (and backed by the cited eval runs) or absent. The manifest never
//! carries a parity, cost, or speed claim without the evidence id next to it
//! (CLAUDE.md: the manifest is the trust layer).

use std::fmt;

use auto_trace::model::{canonical_json, digest_hex};
use serde::{Deserialize, Serialize};

/// Manifest format version; bump with an ADR.
pub const MANIFEST_VERSION: u32 = 0;

/// Where the artifact's behavior came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    /// traces whose recorded observations gated this emit
    pub trace_ids: Vec<String>,
    /// human-readable description of the reference interpreter
    pub reference: String,
    /// recorded observations the differential check replayed
    pub observations: usize,
}

/// Latencies measured during the gating verification. Milliseconds,
/// wall-clock, on the emitting machine — reported as measured, nothing more.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Measured {
    pub compiled_latency_ms_p50: u64,
    pub compiled_latency_ms_p95: u64,
    pub compiled_latency_ms_max: u64,
    /// p95 of the recorded reference durations for the same signature
    pub reference_recorded_latency_ms_p95: u64,
}

/// The manifest carried inside every `.cbin` (entry `manifest.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub manifest_version: u32,
    pub task: String,
    /// span scope this artifact implements
    pub scope_kind: String,
    pub scope_name: String,
    /// IR value type grammar strings (spec/ir.md §3)
    pub interface_input: String,
    pub interface_output: String,
    /// declared capability ceiling, sorted. v0 emits pure artifacts only:
    /// this must be empty, and the module must have zero imports to match.
    pub capabilities: Vec<String>,
    /// content id of the gating contract
    pub contract_id: String,
    /// the PASS eval run(s) that allowed this emit
    pub eval_run_ids: Vec<String>,
    pub provenance: Provenance,
    pub measured: Measured,
    /// honest caveats, plainly worded (e.g. toy-task economics)
    pub notes: String,
}

impl Manifest {
    /// Canonical JSON body (sorted keys, compact). The artifact id is the
    /// digest of the whole container, not of this body alone.
    pub fn canonical_json(&self) -> String {
        let value = serde_json::to_value(self).expect("manifest serialization cannot fail");
        canonical_json(&value)
    }

    /// sha-256 hex of the canonical manifest body.
    pub fn digest(&self) -> String {
        digest_hex(&self.canonical_json())
    }

    pub fn from_json(text: &str) -> Result<Self, ManifestError> {
        let manifest: Self =
            serde_json::from_str(text).map_err(|e| ManifestError::BadJson(e.to_string()))?;
        if manifest.manifest_version != MANIFEST_VERSION {
            return Err(ManifestError::UnsupportedVersion {
                found: manifest.manifest_version,
            });
        }
        Ok(manifest)
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ManifestError {
    #[error("invalid manifest json: {0}")]
    BadJson(String),
    #[error("unsupported manifest_version {found}; this build reads exactly 0")]
    UnsupportedVersion { found: u32 },
}

impl fmt::Display for Manifest {
    /// Deterministic human rendering used by `auto inspect`. Not a stable
    /// machine format.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "manifest v{}", self.manifest_version)?;
        writeln!(
            f,
            "task \"{}\" scope {}({})",
            self.task, self.scope_kind, self.scope_name
        )?;
        writeln!(
            f,
            "interface: ({}) -> ({})",
            self.interface_input, self.interface_output
        )?;
        if self.capabilities.is_empty() {
            writeln!(f, "capabilities: none (pure; module has zero imports)")?;
        } else {
            writeln!(f, "capabilities: {}", self.capabilities.join(","))?;
        }
        writeln!(f, "contract: {}", self.contract_id)?;
        for id in &self.eval_run_ids {
            writeln!(f, "eval run: {id}")?;
        }
        writeln!(
            f,
            "provenance: {} observation(s) from {} trace(s); reference: {}",
            self.provenance.observations,
            self.provenance.trace_ids.len(),
            self.provenance.reference
        )?;
        writeln!(
            f,
            "measured: compiled p50={}ms p95={}ms max={}ms; reference recorded p95={}ms",
            self.measured.compiled_latency_ms_p50,
            self.measured.compiled_latency_ms_p95,
            self.measured.compiled_latency_ms_max,
            self.measured.reference_recorded_latency_ms_p95
        )?;
        if !self.notes.is_empty() {
            writeln!(f, "notes: {}", self.notes)?;
        }
        Ok(())
    }
}
