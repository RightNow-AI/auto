//! The `.cbin` container, v0: a deterministic named-entry blob format.
//!
//! Layout (all integers little-endian):
//!
//! ```text
//! magic "ACB0" | u32 entry_count | entries...
//! entry: u32 name_len | name utf-8 | u64 data_len | data
//! ```
//!
//! Entries are sorted by name and names are unique, so equal artifacts
//! serialize to identical bytes; the **artifact id is the sha-256 of the
//! container bytes**. Required entries: `manifest.json`, `module.wasm`;
//! `graph.air` (the lowered IR of the compiled unit) is conventional.
//! Format rationale: spec/adr/0004-artifact-execution.md.

use std::collections::BTreeMap;

use crate::manifest::{Manifest, ManifestError};

pub const MAGIC: &[u8; 4] = b"ACB0";
pub const MANIFEST_ENTRY: &str = "manifest.json";
pub const MODULE_ENTRY: &str = "module.wasm";
pub const GRAPH_ENTRY: &str = "graph.air";
/// Synthesized artifacts: the DSL program the embedded interpreter loads via
/// the `init` ABI extension (spec/artifact.md §ABI, spec/synthesis.md).
pub const PROGRAM_ENTRY: &str = "program.json";
/// Guarded artifacts: the runtime guard (witness sketches + calibrated
/// threshold) `auto run` evaluates before tier-1 execution (spec/runtime.md).
pub const GUARD_ENTRY: &str = "guard.json";

/// A parsed artifact: named entries, canonical by construction (BTreeMap).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    pub entries: BTreeMap<String, Vec<u8>>,
}

#[derive(Debug, thiserror::Error)]
pub enum ContainerError {
    #[error("not a .cbin artifact (missing ACB0 magic)")]
    BadMagic,
    #[error("truncated container ({0})")]
    Truncated(&'static str),
    #[error("entry name is not utf-8")]
    BadName,
    #[error("entries not sorted or not unique (`{0}` out of order)")]
    NotCanonical(String),
    #[error("trailing bytes after the last entry")]
    TrailingBytes,
    #[error("missing required entry `{0}`")]
    MissingEntry(&'static str),
    #[error("entry too large for this platform")]
    TooLarge,
    #[error(transparent)]
    Manifest(#[from] ManifestError),
}

impl Artifact {
    pub fn new(entries: BTreeMap<String, Vec<u8>>) -> Self {
        Self { entries }
    }

    /// Serialize to canonical container bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(
            &u32::try_from(self.entries.len())
                .expect("entry count fits u32")
                .to_le_bytes(),
        );
        for (name, data) in &self.entries {
            out.extend_from_slice(
                &u32::try_from(name.len())
                    .expect("name length fits u32")
                    .to_le_bytes(),
            );
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&(data.len() as u64).to_le_bytes());
            out.extend_from_slice(data);
        }
        out
    }

    /// Parse container bytes. Strict: canonical order enforced, no trailing
    /// bytes, required entries present.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ContainerError> {
        let rest = bytes.strip_prefix(MAGIC).ok_or(ContainerError::BadMagic)?;
        let (count_bytes, mut rest) = split(rest, 4, "entry count")?;
        let count = u32::from_le_bytes(count_bytes.try_into().expect("4 bytes"));

        let mut entries: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        let mut previous: Option<String> = None;
        for _ in 0..count {
            let (name_len_bytes, r) = split(rest, 4, "name length")?;
            let name_len = u32::from_le_bytes(name_len_bytes.try_into().expect("4 bytes"));
            let name_len = usize::try_from(name_len).map_err(|_| ContainerError::TooLarge)?;
            let (name_bytes, r) = split(r, name_len, "name")?;
            let name = std::str::from_utf8(name_bytes)
                .map_err(|_| ContainerError::BadName)?
                .to_owned();
            if previous.as_deref() >= Some(name.as_str()) {
                return Err(ContainerError::NotCanonical(name));
            }
            let (data_len_bytes, r) = split(r, 8, "data length")?;
            let data_len = u64::from_le_bytes(data_len_bytes.try_into().expect("8 bytes"));
            let data_len = usize::try_from(data_len).map_err(|_| ContainerError::TooLarge)?;
            let (data, r) = split(r, data_len, "entry data")?;
            previous = Some(name.clone());
            entries.insert(name, data.to_vec());
            rest = r;
        }
        if !rest.is_empty() {
            return Err(ContainerError::TrailingBytes);
        }
        for required in [MANIFEST_ENTRY, MODULE_ENTRY] {
            if !entries.contains_key(required) {
                return Err(ContainerError::MissingEntry(required));
            }
        }
        Ok(Self { entries })
    }

    /// Artifact identity: sha-256 hex of the canonical container bytes.
    pub fn id(&self) -> String {
        let bytes = self.to_bytes();
        digest_hex_bytes(&bytes)
    }

    /// Parse and version-check the embedded manifest.
    pub fn manifest(&self) -> Result<Manifest, ContainerError> {
        let raw = self
            .entries
            .get(MANIFEST_ENTRY)
            .ok_or(ContainerError::MissingEntry(MANIFEST_ENTRY))?;
        let text = std::str::from_utf8(raw)
            .map_err(|_| ManifestError::BadJson("manifest is not utf-8".into()))?;
        Ok(Manifest::from_json(text)?)
    }

    pub fn module_bytes(&self) -> Result<&[u8], ContainerError> {
        self.entries
            .get(MODULE_ENTRY)
            .map(Vec::as_slice)
            .ok_or(ContainerError::MissingEntry(MODULE_ENTRY))
    }
}

fn split<'a>(
    bytes: &'a [u8],
    n: usize,
    what: &'static str,
) -> Result<(&'a [u8], &'a [u8]), ContainerError> {
    if bytes.len() < n {
        return Err(ContainerError::Truncated(what));
    }
    Ok(bytes.split_at(n))
}

/// sha-256 hex over raw bytes (the trace-crate helper digests strings).
fn digest_hex_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in hash {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}
