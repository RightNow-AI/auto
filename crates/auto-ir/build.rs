//! Generates rust accessors from `schema/ir.fbs` with a pinned `flatc`.
//!
//! The flatc version MUST equal the `flatbuffers` runtime crate version:
//! generated code and runtime are released in lockstep, and golden-file byte
//! stability depends on both. The build fails loudly on any mismatch.
//!
//! Resolution order: `FLATC` env var → `<workspace>/tools/flatc/flatc[.exe]`
//! (gitignored local install) → `flatc` on PATH. See README.md for install.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Must equal the `flatbuffers` dependency version in the workspace manifest.
const PINNED_FLATC_VERSION: &str = "25.12.19";

fn candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(explicit) = env::var("FLATC") {
        out.push(PathBuf::from(explicit));
    }
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    // crates/auto-ir → ancestors: [auto-ir, crates, workspace root]
    if let Some(workspace_root) = manifest_dir.ancestors().nth(2) {
        out.push(
            workspace_root
                .join("tools")
                .join("flatc")
                .join(format!("flatc{}", env::consts::EXE_SUFFIX)),
        );
    }
    out.push(PathBuf::from("flatc")); // PATH lookup
    out
}

fn version_of(flatc: &Path) -> Option<String> {
    let output = Command::new(flatc).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    // "flatc version 25.12.19" → "25.12.19"
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .last()
        .map(str::to_owned)
}

fn main() {
    println!("cargo:rerun-if-changed=schema/ir.fbs");
    println!("cargo:rerun-if-env-changed=FLATC");

    let mut wrong_versions = Vec::new();
    let mut flatc = None;
    for candidate in candidates() {
        match version_of(&candidate) {
            Some(v) if v == PINNED_FLATC_VERSION => {
                flatc = Some(candidate);
                break;
            }
            Some(v) => wrong_versions.push(format!("{} is {v}", candidate.display())),
            None => {}
        }
    }
    let Some(flatc) = flatc else {
        let seen = if wrong_versions.is_empty() {
            String::new()
        } else {
            format!(" (found, wrong version: {})", wrong_versions.join(", "))
        };
        panic!(
            "flatc {PINNED_FLATC_VERSION} not found via FLATC env, tools/flatc/, or PATH{seen}.\n\
             install the pinned binary from \
             https://github.com/google/flatbuffers/releases/tag/v{PINNED_FLATC_VERSION} \
             — see README.md \"build\"."
        );
    };

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let schema = manifest_dir.join("schema").join("ir.fbs");

    let status = Command::new(&flatc)
        .arg("--rust")
        .arg("-o")
        .arg(&out_dir)
        .arg(&schema)
        .status()
        .unwrap_or_else(|e| panic!("failed to run {}: {e}", flatc.display()));
    assert!(status.success(), "flatc exited with {status}");

    let generated = out_dir.join("ir_generated.rs");
    assert!(
        generated.is_file(),
        "flatc did not produce {}",
        generated.display()
    );
}
