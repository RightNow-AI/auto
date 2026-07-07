//! Callback tool host tests (`HostTools::Callback`, ADR-0027): the embedded
//! host seam auto-py rides. Every artifact here is a REAL runnable `.cbin`
//! built in memory around the tool interpreter (auto.tool_call imported) and
//! a pipeline-v1 payload — the true tier-1 capability path (ADR-0017), not a
//! stand-in. Mirrors tests/runner.rs's capability fixtures. No network, no
//! subprocess: the whole point of Callback is that the tool is a host
//! closure.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use auto_backend::container::PROGRAM_ENTRY;
use auto_backend::{
    Artifact, MANIFEST_ENTRY, MANIFEST_VERSION, MODULE_ENTRY, Manifest, Measured, Provenance,
};
use auto_runtime::Runner;
use auto_runtime::executor::{ExecError, HostTools, WasmExecutor};
use serde_json::{Value, json};

/// An honest manifest with the given capability ceiling (same fixture as
/// tests/runner.rs). Measured-zero-ish latencies: this manifest gated
/// nothing, it exists to be read.
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

/// A capability artifact around the REAL tool interpreter: one `tool_call`
/// pipeline stage invoking `tool`, with `declared` as the manifest capability
/// list. Splitting the two lets tests build honest artifacts (declared ==
/// staged) AND deliberately mismatched ones (call-time allowlist coverage).
fn tool_artifact(declared: Vec<String>, tool: &str) -> Artifact {
    let pipeline = auto_passes::auto_dsl::Pipeline::new(vec![auto_passes::auto_dsl::Stage::Tool {
        name: tool.into(),
    }]);
    let mut entries = BTreeMap::new();
    entries.insert(
        MANIFEST_ENTRY.to_owned(),
        manifest(declared).canonical_json().into_bytes(),
    );
    entries.insert(
        MODULE_ENTRY.to_owned(),
        auto_passes::tool_interpreter_wasm().to_vec(),
    );
    entries.insert(PROGRAM_ENTRY.to_owned(), pipeline.to_json().into_bytes());
    Artifact::new(entries)
}

/// The common honest case: declares `lookup`, stages `lookup`.
fn lookup_artifact() -> Artifact {
    tool_artifact(vec!["lookup".into()], "lookup")
}

/// A pure ECHO artifact (zero imports, no capabilities) — the counterparty
/// for the host-on-a-pure-artifact refusal.
fn pure_artifact() -> Artifact {
    const ECHO: &str = r#"(module
        (memory (export "memory") 2)
        (global $next (mut i32) (i32.const 4096))
        (func (export "alloc") (param i32) (result i32)
            global.get $next
            global.get $next local.get 0 i32.add global.set $next)
        (func (export "run") (param i32 i32) (result i64)
            local.get 0 i64.extend_i32_u i64.const 32 i64.shl
            local.get 1 i64.extend_i32_u i64.or))"#;
    let mut entries = BTreeMap::new();
    entries.insert(
        MANIFEST_ENTRY.to_owned(),
        manifest(vec![]).canonical_json().into_bytes(),
    );
    entries.insert(MODULE_ENTRY.to_owned(), ECHO.as_bytes().to_vec());
    Artifact::new(entries)
}

// ---- the happy path: a host closure answers the tool ----

#[test]
fn callback_host_answers_through_the_tool_interpreter() {
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&hits);
    // a REAL FnMut: `local` is captured state mutated across invocations,
    // which is exactly what the seam promises to support
    let mut local = 0usize;
    let tools = HostTools::callback(["lookup".to_owned()], move |name, input| {
        local += 1;
        seen.store(local, Ordering::SeqCst);
        assert_eq!(name, "lookup", "the seam only dispatches declared names");
        Ok(json!({ "echoed": input.clone(), "call": local }))
    });
    let exec = WasmExecutor::from_artifact_with_tools(&lookup_artifact(), Some(tools))
        .expect("capability artifact loads with a covering callback host");

    let out = exec.execute(&json!({ "q": "beta" })).expect("tool answers");
    assert_eq!(out, json!({ "echoed": { "q": "beta" }, "call": 1 }));
    // second call on the same executor: fresh wasm instance, same host, the
    // FnMut's captured state advanced — the callback really ran twice
    let out = exec.execute(&json!("next")).expect("tool answers again");
    assert_eq!(out, json!({ "echoed": "next", "call": 2 }));
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[test]
fn runner_answers_a_line_through_a_callback_host() {
    // the exact seam auto-py drives: artifact bytes + Callback host into
    // Runner::new_with_tools, one protocol line answered end to end
    let tools = HostTools::callback(["lookup".to_owned()], |_, input| {
        Ok(json!(format!("looked-up:{input}")))
    });
    let runner = Runner::new_with_tools(&lookup_artifact().to_bytes(), Some(tools))
        .expect("capability artifact loads");
    let line: Value = serde_json::from_str(&runner.answer(r#"{"q":"beta"}"#)).unwrap();
    assert_eq!(line, json!({ "output": "looked-up:{\"q\":\"beta\"}" }));
}

// ---- error envelope: a callback failure is a trap, never a crash ----

#[test]
fn callback_error_becomes_an_err_envelope_and_an_honest_trap() {
    let tools = HostTools::callback(["lookup".to_owned()], |_, _| {
        Err("lookup exploded".to_owned())
    });
    let exec = WasmExecutor::from_artifact_with_tools(&lookup_artifact(), Some(tools)).unwrap();
    // the interpreter unwraps {"err"} by panicking — a wasm trap the executor
    // surfaces as ExecError::Trap (the panic MESSAGE does not survive the
    // wasm32 abort, so assert the variant, not the text)
    match exec.execute(&json!("x")) {
        Err(ExecError::Trap(_)) => {}
        other => panic!("expected Trap from an err envelope, got {other:?}"),
    }
}

// ---- loader rules: one enforcement point, unchanged (ADR-0017) ----

#[test]
fn missing_capability_refuses_at_load() {
    // declared `lookup`, host covers only `other`: the SAME loader
    // cross-check that guards Live tables fires for Callback names
    let tools = HostTools::callback(["other".to_owned()], |_, _| Ok(json!(null)));
    match WasmExecutor::from_artifact_with_tools(&lookup_artifact(), Some(tools)) {
        Err(ExecError::CapabilityMismatch(msg)) => {
            assert!(msg.contains("lookup"), "{msg}");
        }
        other => panic!("expected CapabilityMismatch, got {:?}", other.err()),
    }
}

#[test]
fn callback_host_on_a_pure_artifact_refuses() {
    let tools = HostTools::callback(["lookup".to_owned()], |_, _| Ok(json!(null)));
    match WasmExecutor::from_artifact_with_tools(&pure_artifact(), Some(tools)) {
        Err(ExecError::CapabilityMismatch(msg)) => {
            assert!(msg.contains("must not be attached"), "{msg}");
        }
        other => panic!("expected CapabilityMismatch, got {:?}", other.err()),
    }
}

#[test]
fn capability_artifact_without_a_host_still_names_its_tools() {
    match WasmExecutor::from_artifact_with_tools(&lookup_artifact(), None) {
        Err(ExecError::ToolsRequired { capabilities }) => {
            assert_eq!(capabilities, vec!["lookup".to_owned()]);
        }
        other => panic!("expected ToolsRequired, got {:?}", other.err()),
    }
}

// ---- call-time allowlist: names gate dispatch, not just loading ----

#[test]
fn undeclared_name_errors_before_the_callback_runs() {
    // a deliberately dishonest artifact (hand-built; emit would refuse it):
    // manifest declares `lookup`, but the pipeline stage calls `other`. The
    // loader passes (declared ⊆ provided); the CALL must refuse in the seam
    // — the callback itself never sees the undeclared name.
    let artifact = tool_artifact(vec!["lookup".into()], "other");
    let invoked = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&invoked);
    let tools = HostTools::callback(["lookup".to_owned()], move |_, _| {
        counter.fetch_add(1, Ordering::SeqCst);
        Ok(json!(null))
    });
    let exec = WasmExecutor::from_artifact_with_tools(&artifact, Some(tools)).unwrap();
    // the seam answers {"err": not in the provided tool table}; the
    // interpreter turns it into a trap
    match exec.execute(&json!("x")) {
        Err(ExecError::Trap(_)) => {}
        other => panic!("expected Trap for an undeclared tool name, got {other:?}"),
    }
    assert_eq!(
        invoked.load(Ordering::SeqCst),
        0,
        "the callback must never be dispatched an undeclared name"
    );
}

// ---- panic safety: propagation and poison recovery ----

#[test]
fn callback_panic_propagates_and_does_not_brick_the_host() {
    // wasmtime catches a host panic at the wasm boundary and resumes the
    // unwind once the wasm stack is gone (wasmtime-46.0.1 traphandlers.rs:
    // 264, 446), so a panicking callback panics `execute` — it must not
    // abort, and it must not poison the host forever.
    let mut first = true;
    let tools = HostTools::callback(["lookup".to_owned()], move |_, input| {
        if std::mem::take(&mut first) {
            panic!("callback panicked on purpose");
        }
        Ok(json!({ "recovered": input.clone() }))
    });
    let exec = WasmExecutor::from_artifact_with_tools(&lookup_artifact(), Some(tools)).unwrap();

    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = exec.execute(&json!("boom"));
    }));
    assert!(unwound.is_err(), "the callback's panic must propagate");

    // the panic poisoned the mutex; the seam recovers (PoisonError::into_inner)
    // and the SAME executor keeps answering
    let out = exec
        .execute(&json!("again"))
        .expect("host survives a panic");
    assert_eq!(out, json!({ "recovered": "again" }));
}

// ---- debug: the closure never pretends to be printable ----

#[test]
fn callback_debug_names_the_covered_tools_only() {
    let tools = HostTools::callback(["lookup".to_owned()], |_, _| Ok(json!(null)));
    let rendered = format!("{tools:?}");
    assert!(rendered.contains("Callback"), "{rendered}");
    assert!(rendered.contains("lookup"), "{rendered}");
}
