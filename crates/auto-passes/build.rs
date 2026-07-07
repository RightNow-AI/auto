//! Builds the artifact interpreters (`dsl-interpreter/`, `model-interpreter/`
//! — workspace-excluded crates) for wasm32-unknown-unknown and stages the
//! modules in `$OUT_DIR` for `include_bytes!`.
//!
//! Requires the wasm32-unknown-unknown target (README "build"; CI installs
//! it). Nested cargo builds get their own CARGO_TARGET_DIR inside OUT_DIR so
//! they never contend with the outer build's lock.

use std::env;
use std::path::PathBuf;
use std::process::Command;

/// (source dir, built artifact name, staged name, cargo features)
const INTERPRETERS: [(&str, &str, &str, &[&str]); 4] = [
    ("dsl-interpreter", "dsl_interpreter", "dsl_interpreter", &[]),
    // the capability build of the SAME source: auto.tool_call import declared
    // (ADR-0017); staged under its own name so pure artifacts never carry it
    (
        "dsl-interpreter",
        "dsl_interpreter",
        "dsl_tool_interpreter",
        &["tools"],
    ),
    ("mlp-interpreter", "mlp_interpreter", "mlp_interpreter", &[]),
    (
        "model-interpreter",
        "model_interpreter",
        "model_interpreter",
        &[],
    ),
];

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let crates_dir = manifest_dir.parent().expect("crates dir").to_path_buf();
    for shared in ["auto-dsl", "auto-model"] {
        println!(
            "cargo:rerun-if-changed={}",
            crates_dir.join(shared).join("src").display()
        );
    }

    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    for (dir_name, artifact_name, staged_name, features) in INTERPRETERS {
        let interpreter = manifest_dir.join(dir_name);
        println!(
            "cargo:rerun-if-changed={}",
            interpreter.join("src").display()
        );
        println!(
            "cargo:rerun-if-changed={}",
            interpreter.join("Cargo.toml").display()
        );

        let target_dir = out_dir.join(format!("{staged_name}-target"));
        let mut command = Command::new(&cargo);
        command
            .arg("build")
            .arg("--release")
            .arg("--target")
            .arg("wasm32-unknown-unknown")
            .arg("--manifest-path")
            .arg(interpreter.join("Cargo.toml"));
        if !features.is_empty() {
            command.arg("--features").arg(features.join(","));
        }
        let status = command
            .env("CARGO_TARGET_DIR", &target_dir)
            // the outer build's flags target the host; do not leak into wasm
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .status()
            .unwrap_or_else(|e| panic!("failed to spawn nested cargo for {dir_name}: {e}"));
        assert!(
            status.success(),
            "{dir_name} wasm build failed ({status}); is the \
             wasm32-unknown-unknown target installed? (rustup target add \
             wasm32-unknown-unknown — see README)"
        );

        let built = target_dir
            .join("wasm32-unknown-unknown")
            .join("release")
            .join(format!("{artifact_name}.wasm"));
        let staged = out_dir.join(format!("{staged_name}.wasm"));
        std::fs::copy(&built, &staged).unwrap_or_else(|e| {
            panic!(
                "cannot stage {} -> {}: {e}",
                built.display(),
                staged.display()
            )
        });
    }
}
