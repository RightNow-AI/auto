//! Differential tests: the twin's pure logic against the REAL
//! `auto_runtime::Runner` (the reference this crate embeds), not against
//! hand-written envelope literals. Every artifact is a runnable `.cbin`
//! built in memory (wat text straight into `module.wasm`, the same canonical
//! fixtures as auto-runtime's own tests — wasmtime compiles it, so this is
//! the true tier-1 path). No napi, no Node, no network: this test target
//! builds and runs with the crate's DEFAULT features, so it is part of the
//! plain cargo gates.
//!
//! What is pinned: for each runner outcome — output, trip-with-distance,
//! trip-without-distance, execution trap, unparseable input — the envelope
//! `Runner::answer` actually emits decodes to the [`Decoded`] arm the napi
//! binding turns into return / `AutoAbstained` / `AutoError`. This is the
//! twin-alignment gate: auto-py's unit tests pin the same shapes as
//! literals; here they are pinned against the emitter itself.

use std::collections::BTreeMap;

use auto_backend::container::GUARD_ENTRY;
use auto_backend::{
    Artifact, MANIFEST_ENTRY, MANIFEST_VERSION, MODULE_ENTRY, Manifest, Measured, Provenance,
};
use auto_node::logic::{Decoded, capability_refusal_message, decode_answer};
use auto_runtime::{Guard, Runner};
use serde_json::json;

/// Bump-allocates from 4096 and echoes the input region back — the canonical
/// runnable fixture (mirrors auto-runtime::executor and auto-serve tests).
const ECHO: &str = r#"(module
    (memory (export "memory") 2)
    (global $next (mut i32) (i32.const 4096))
    (func (export "alloc") (param i32) (result i32)
        global.get $next
        global.get $next local.get 0 i32.add global.set $next)
    (func (export "run") (param i32 i32) (result i64)
        local.get 0 i64.extend_i32_u i64.const 32 i64.shl
        local.get 1 i64.extend_i32_u i64.or))"#;

/// A module whose `run` traps — the execute-error branch of `answer`.
const TRAP: &str = r#"(module
    (memory (export "memory") 1)
    (func (export "alloc") (param i32) (result i32) i32.const 0)
    (func (export "run") (param i32 i32) (result i64) unreachable))"#;

/// An honest manifest with the given capability ceiling (measured values are
/// placeholders read by nothing here; the same fixture manifest as
/// auto-runtime's runner tests).
fn manifest(capabilities: Vec<String>) -> Manifest {
    Manifest {
        manifest_version: MANIFEST_VERSION,
        task: "toy-agent".into(),
        scope_kind: "model_call".into(),
        scope_name: "fake-frontier".into(),
        interface_input: "text".into(),
        interface_output: "text".into(),
        capabilities,
        contract_id: "c".repeat(8),
        eval_run_ids: vec!["run-1".into()],
        provenance: Provenance {
            trace_ids: vec!["0".repeat(32)],
            reference: "test reference".into(),
            observations: 1,
        },
        measured: Measured {
            compiled_latency_ms_p50: 1,
            compiled_latency_ms_p95: 2,
            compiled_latency_ms_max: 3,
            reference_recorded_latency_ms_p95: 40,
        },
        notes: String::new(),
    }
}

/// Container bytes for a runnable artifact: module, manifest, optional guard.
fn artifact_bytes(module: &str, capabilities: Vec<String>, guard: Option<&Guard>) -> Vec<u8> {
    let mut entries = BTreeMap::new();
    entries.insert(
        MANIFEST_ENTRY.to_owned(),
        manifest(capabilities).canonical_json().into_bytes(),
    );
    entries.insert(MODULE_ENTRY.to_owned(), module.as_bytes().to_vec());
    if let Some(guard) = guard {
        entries.insert(GUARD_ENTRY.to_owned(), guard.to_json().into_bytes());
    }
    Artifact::new(entries).to_bytes()
}

fn echo_runner() -> Runner {
    Runner::new(&artifact_bytes(ECHO, vec![], None)).expect("echo artifact loads")
}

fn guarded_echo_runner() -> Runner {
    let guard = Guard::build(&[json!("hello world")], None).expect("guard builds");
    Runner::new(&artifact_bytes(ECHO, vec![], Some(&guard))).expect("guarded artifact loads")
}

// ---- output ----

#[test]
fn real_output_envelope_decodes_to_canonical_text() {
    let runner = echo_runner();
    assert_eq!(
        decode_answer(&runner.answer(r#"{"b":2,"a":1}"#)),
        Decoded::Output(r#"{"a":1,"b":2}"#.to_owned())
    );
    assert_eq!(
        decode_answer(&runner.answer(r#""hi""#)),
        Decoded::Output(r#""hi""#.to_owned())
    );
}

// ---- abstention ----

#[test]
fn real_trip_envelope_decodes_with_structured_fields() {
    let runner = guarded_echo_runner();
    match decode_answer(&runner.answer(r#""nothing alike here""#)) {
        Decoded::Abstained(abstention) => {
            assert!(abstention.reason.is_some(), "{abstention:?}");
            assert!(abstention.distance.expect("measured distance") > 0.0);
            assert_eq!(abstention.threshold, Some(0.0));
            assert!(
                abstention.message.contains("threshold"),
                "{}",
                abstention.message
            );
        }
        other => panic!("expected Abstained, got {other:?}"),
    }
}

#[test]
fn real_wrong_shape_trip_has_no_distance() {
    let runner = guarded_echo_runner();
    // an object where the guard requires a bare string: trips, distance null
    match decode_answer(&runner.answer(r#"{"not":"text"}"#)) {
        Decoded::Abstained(abstention) => {
            assert_eq!(abstention.distance, None);
            assert!(
                abstention.message.contains("no measurable distance"),
                "{}",
                abstention.message
            );
        }
        other => panic!("expected Abstained, got {other:?}"),
    }
}

// ---- errors ----

#[test]
fn real_trap_envelope_decodes_to_error() {
    let runner = Runner::new(&artifact_bytes(TRAP, vec![], None)).expect("trap artifact loads");
    match decode_answer(&runner.answer("null")) {
        Decoded::Error(detail) => {
            assert!(detail.contains("execution failed"), "{detail}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[test]
fn real_bad_input_envelope_decodes_to_error() {
    let runner = echo_runner();
    match decode_answer(&runner.answer("{not json")) {
        Decoded::Error(detail) => assert!(detail.contains("not valid JSON"), "{detail}"),
        other => panic!("expected Error, got {other:?}"),
    }
}

// ---- the load gate agrees with the loader ----

#[test]
fn capability_artifact_refused_by_gate_and_loader_alike() {
    let bytes = artifact_bytes(ECHO, vec!["lookup".into()], None);
    let manifest = Artifact::from_bytes(&bytes)
        .expect("valid container")
        .manifest()
        .expect("valid manifest");
    // the twin's LOAD gate fires first, with the frozen ADR-0024 message...
    let refusal =
        capability_refusal_message(&manifest.capabilities).expect("capability artifact refused");
    assert!(
        refusal.contains("not supported embedded in v0"),
        "{refusal}"
    );
    // ...and the loader underneath refuses the same bytes independently, so
    // the gate never masks a loader disagreement (Runner is not Debug; match)
    match Runner::new(&bytes) {
        Err(e) => assert!(e.contains("lookup"), "loader names the missing tool: {e}"),
        Ok(_) => panic!("loader must refuse a capability artifact with no tools"),
    }
}

// ---- bench fixture generator (operator-run, ignored by the gates) ----

/// Writes the bench fixtures `evals/embedded-node/README.md` points at:
/// a pure unguarded echo `.cbin`, a guarded echo `.cbin` (same guard as the
/// tests above: admits "hello world", trips on distant text), and an inputs
/// file. NOT a test of anything — a labeled generator, `#[ignore]`d so the
/// cargo gates never write files; run explicitly with
/// `cargo test -p auto-node --test twin_contract -- --ignored write_bench_fixture`.
/// Output lands under the workspace `target/embedded-node-bench/` (kept out
/// of the tree; same convention as the other smoke artifacts).
#[test]
#[ignore = "fixture generator, not a test: writes target/embedded-node-bench/ for bench.js"]
fn write_bench_fixture() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("embedded-node-bench");
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    std::fs::write(
        dir.join("echo-pure.cbin"),
        artifact_bytes(ECHO, vec![], None),
    )
    .expect("write pure fixture");
    let guard = Guard::build(&[json!("hello world")], None).expect("guard builds");
    std::fs::write(
        dir.join("echo-guarded.cbin"),
        artifact_bytes(ECHO, vec![], Some(&guard)),
    )
    .expect("write guarded fixture");
    std::fs::write(
        dir.join("inputs.jsonl"),
        "{\"b\":2,\"a\":1}\n\"hello world\"\n42\n",
    )
    .expect("write inputs");
    // one input that trips the guarded artifact, for an outcomes-mixed run
    std::fs::write(
        dir.join("inputs-guarded.jsonl"),
        "\"hello world\"\n\"nothing alike here\"\n",
    )
    .expect("write guarded inputs");
}
