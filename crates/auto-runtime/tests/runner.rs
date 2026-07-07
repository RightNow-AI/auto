//! Resident stdio runner tests (`auto_runtime::runner`).
//!
//! Every artifact here is a REAL runnable `.cbin` built in memory (wat text
//! straight into `module.wasm`, exactly as auto-serve's tests do — wasmtime
//! compiles the wat, so this is the true tier-1 path, not a fake). The
//! protocol is driven with in-memory readers/writers: no real stdin/stdout,
//! no sockets, no network. What is under test is what `Runner` does with each
//! line — parse-or-error, guard-or-abstain, execute-or-error — and that the
//! module is compiled once and reused across many lines.

use std::collections::BTreeMap;
use std::io::Cursor;

use auto_backend::container::{GUARD_ENTRY, PROGRAM_ENTRY};
use auto_backend::{
    Artifact, MANIFEST_ENTRY, MANIFEST_VERSION, MODULE_ENTRY, Manifest, Measured, Provenance,
};
use auto_runtime::executor::HostTools;
use auto_runtime::{Guard, Runner};
use auto_trace::model::canonical_json;
use serde_json::{Value, json};

/// Bump-allocates from 4096 and echoes the input region back — the canonical
/// runnable fixture (mirrors auto-runtime::executor and auto-serve).
const ECHO: &str = r#"(module
    (memory (export "memory") 2)
    (global $next (mut i32) (i32.const 4096))
    (func (export "alloc") (param i32) (result i32)
        global.get $next
        global.get $next local.get 0 i32.add global.set $next)
    (func (export "run") (param i32 i32) (result i64)
        local.get 0 i64.extend_i32_u i64.const 32 i64.shl
        local.get 1 i64.extend_i32_u i64.or))"#;

/// A module whose `run` traps — exercises the execute-error branch of
/// `answer` (a trap is a tier-1 execution failure, surfaced as `{"error"}`).
const TRAP: &str = r#"(module
    (memory (export "memory") 1)
    (func (export "alloc") (param i32) (result i32) i32.const 0)
    (func (export "run") (param i32 i32) (result i64) unreachable))"#;

/// An honest manifest with the given capability ceiling. Measured-zero
/// latencies: this manifest gated nothing, it exists to be read.
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

/// Container bytes for a runnable artifact: the given module, a manifest with
/// the given capability ceiling, and an optional guard.
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

/// The common pure case: an unguarded ECHO artifact with no capabilities.
fn echo_runner() -> Runner {
    Runner::new(&artifact_bytes(ECHO, vec![], None)).expect("echo artifact loads")
}

/// A REAL capability artifact: the tool interpreter (auto.tool_call imported)
/// carrying a pipeline-v1 payload of one `tool_call` stage, whose declared
/// capability is `lookup`. Running it hands the whole input to the `lookup`
/// tool and returns the tool's answer — the true tier-1 capability path
/// (ADR-0017), not a stand-in.
fn lookup_artifact(guard: Option<&Guard>) -> Vec<u8> {
    let pipeline = auto_passes::auto_dsl::Pipeline::new(vec![auto_passes::auto_dsl::Stage::Tool {
        name: "lookup".into(),
    }]);
    let mut entries = BTreeMap::new();
    entries.insert(
        MANIFEST_ENTRY.to_owned(),
        manifest(vec!["lookup".into()])
            .canonical_json()
            .into_bytes(),
    );
    entries.insert(
        MODULE_ENTRY.to_owned(),
        auto_passes::tool_interpreter_wasm().to_vec(),
    );
    entries.insert(PROGRAM_ENTRY.to_owned(), pipeline.to_json().into_bytes());
    if let Some(guard) = guard {
        entries.insert(GUARD_ENTRY.to_owned(), guard.to_json().into_bytes());
    }
    Artifact::new(entries).to_bytes()
}

/// A replay host covering `lookup` for one witnessed `(input -> output)` pair,
/// keyed exactly as the executor keys it (canonical JSON of the register the
/// tool stage receives — here the whole pipeline input).
fn lookup_replay(input: &Value, output: Value) -> HostTools {
    let mut recorded = BTreeMap::new();
    recorded.insert(("lookup".to_owned(), canonical_json(input)), output);
    HostTools::Replay(recorded)
}

fn parse(line: &str) -> Value {
    serde_json::from_str(line).expect("runner emits one JSON object per line")
}

// ---- answer: the three outcomes ----

#[test]
fn answer_wraps_the_output_of_a_good_line() {
    let runner = echo_runner();
    let out = runner.answer(r#"{"a":1}"#);
    assert_eq!(parse(&out), json!({ "output": { "a": 1 } }));
    // one object per line: no embedded newline the framing could split on
    assert!(!out.contains('\n'), "answer must be a single line: {out:?}");
}

#[test]
fn answer_reports_bad_json_as_an_error_object() {
    let runner = echo_runner();
    let value = parse(&runner.answer("{not json"));
    assert!(value["error"].is_string(), "{value}");
    assert!(value.get("output").is_none());
    assert!(value.get("abstained").is_none());
}

#[test]
fn answer_surfaces_a_trap_as_an_error_object() {
    let runner = Runner::new(&artifact_bytes(TRAP, vec![], None)).expect("trap artifact loads");
    let value = parse(&runner.answer("null"));
    assert!(
        value["error"]
            .as_str()
            .expect("error string")
            .contains("execution failed"),
        "{value}"
    );
}

// ---- answer: guard semantics, identical to `auto run` ----

#[test]
fn guarded_proceed_executes_tier1() {
    let guard = Guard::build(&[json!("hello world")], None).unwrap();
    let runner = Runner::new(&artifact_bytes(ECHO, vec![], Some(&guard))).unwrap();
    assert_eq!(
        parse(&runner.answer(r#""hello world""#)),
        json!({ "output": "hello world" })
    );
}

#[test]
fn guarded_trip_abstains_with_distance() {
    let guard = Guard::build(&[json!("hello world")], None).unwrap();
    let runner = Runner::new(&artifact_bytes(ECHO, vec![], Some(&guard))).unwrap();
    let value = parse(&runner.answer(r#""nothing alike here""#));
    assert_eq!(value["abstained"], json!(true));
    assert_eq!(value["threshold"], json!(0.0));
    assert!(value["distance"].as_f64().expect("distance number") > 0.0);
    assert!(value["reason"].is_string());
    assert!(value.get("output").is_none());
}

#[test]
fn guarded_wrong_shape_abstains_with_null_distance() {
    let guard = Guard::build(&[json!("hello world")], None).unwrap();
    let runner = Runner::new(&artifact_bytes(ECHO, vec![], Some(&guard))).unwrap();
    // an object where the guard requires a bare string: trips, no distance
    let value = parse(&runner.answer(r#"{"not":"text"}"#));
    assert_eq!(value["abstained"], json!(true));
    assert_eq!(value["distance"], Value::Null);
}

/// A wire-v2 embedding guard (ADR-0023) rides the same `guard.json` seam:
/// the runner loads it through `Guard::from_json` with no runner changes,
/// proceeds on a witness doc, and abstains on disjoint vocabulary — the
/// full artifact-to-decision path for v2. (Lexical geometry: the witness
/// doc is admitted because it IS the recorded spelling.)
#[test]
fn v2_embedding_guarded_artifact_proceeds_and_abstains() {
    let doc = "Compilers translate agent cognition into fast deterministic binaries.";
    let guard = Guard::build_embedding(&[json!(doc)], None, 100).unwrap();
    let runner = Runner::new(&artifact_bytes(ECHO, vec![], Some(&guard))).unwrap();
    assert_eq!(
        parse(&runner.answer(&format!("{doc:?}"))),
        json!({ "output": doc })
    );
    let value = parse(&runner.answer(r#""zzz qqq vvv jjj xxx""#));
    assert_eq!(value["abstained"], json!(true));
    assert_eq!(value["threshold"], json!(0.0));
    assert!(value["distance"].as_f64().expect("distance number") > 0.0);
}

// ---- new: refusals ----

// `Runner` holds a non-Debug `WasmExecutor`, so these match on the Result
// rather than `unwrap_err` (same style as executor.rs's own tests).

#[test]
fn new_on_garbage_bytes_is_an_error() {
    match Runner::new(b"not a .cbin at all") {
        Err(err) => assert!(err.contains("invalid artifact"), "{err}"),
        Ok(_) => panic!("garbage bytes must not load as a runner"),
    }
}

#[test]
fn capability_artifact_without_tools_refuses_via_the_loader() {
    // `new` supplies no tool host, so a capability artifact refuses through
    // WasmExecutor::from_artifact_with_tools — the loader's own message,
    // naming the missing tool (ADR-0017 amendment, wave 7; this replaces the
    // wave-6 hand-rolled refusal).
    match Runner::new(&lookup_artifact(None)) {
        Err(err) => {
            assert!(err.contains("cannot load module"), "{err}");
            assert!(err.contains("no tool host"), "{err}");
            assert!(err.contains("lookup"), "{err}");
        }
        Ok(_) => panic!("a capability artifact must refuse without a tool host"),
    }
}

#[test]
fn capability_artifact_with_a_covering_host_answers_a_line() {
    // new_with_tools loads the same artifact against a replay host that covers
    // `lookup`; a line is answered end to end (input -> tool -> output).
    let input = json!({ "q": "beta" });
    let tools = lookup_replay(&input, json!("TEAM-B"));
    let runner = Runner::new_with_tools(&lookup_artifact(None), Some(tools))
        .expect("capability artifact loads with a covering host");
    let value = parse(&runner.answer(&input.to_string()));
    assert_eq!(value, json!({ "output": "TEAM-B" }));
}

#[test]
fn capability_artifact_guard_trips_before_the_tool() {
    // the guard gates first even for a capability artifact: a far input
    // abstains and the `lookup` tool is never reached.
    let guard = Guard::build(&[json!({ "q": "hello world" })], Some("q")).unwrap();
    let tools = lookup_replay(&json!({ "q": "hello world" }), json!("TEAM-H"));
    let runner = Runner::new_with_tools(&lookup_artifact(Some(&guard)), Some(tools)).unwrap();
    let value = parse(&runner.answer(r#"{"q":"zzzzz qqqqq xxxxx"}"#));
    assert_eq!(value["abstained"], json!(true));
    assert!(value.get("output").is_none());
}

#[test]
fn pure_artifact_still_refuses_a_host() {
    // symmetric to the loader rule: a pure artifact handed a host refuses,
    // so `new`'s None path is the only way a pure artifact loads.
    let tools = lookup_replay(&json!("x"), json!("y"));
    match Runner::new_with_tools(&artifact_bytes(ECHO, vec![], None), Some(tools)) {
        Err(err) => assert!(err.contains("must not be attached"), "{err}"),
        Ok(_) => panic!("a pure artifact must refuse an attached host"),
    }
}

// ---- serve: the line loop ----

#[test]
fn serve_answers_each_line_in_order() {
    let runner = echo_runner();
    let input = Cursor::new("\"first\"\n\"second\"\n\"third\"\n".as_bytes());
    let mut output: Vec<u8> = Vec::new();
    runner.serve(input, &mut output).expect("serve to EOF");

    let text = String::from_utf8(output).unwrap();
    let lines: Vec<Value> = text.lines().map(parse).collect();
    assert_eq!(lines.len(), 3, "one response per input line");
    assert_eq!(lines[0], json!({ "output": "first" }));
    assert_eq!(lines[1], json!({ "output": "second" }));
    assert_eq!(lines[2], json!({ "output": "third" }));
}

#[test]
fn serve_mixes_outcomes_one_object_per_line() {
    let runner = echo_runner();
    // good line, bad-json line, good line — heterogeneous, order preserved
    let input = Cursor::new("\"ok\"\n{bad\n[1,2]\n".as_bytes());
    let mut output: Vec<u8> = Vec::new();
    runner.serve(input, &mut output).unwrap();

    let text = String::from_utf8(output).unwrap();
    let lines: Vec<Value> = text.lines().map(parse).collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0], json!({ "output": "ok" }));
    assert!(lines[1]["error"].is_string(), "{}", lines[1]);
    assert_eq!(lines[2], json!({ "output": [1, 2] }));
}

#[test]
fn serve_on_empty_input_writes_nothing_and_returns_ok() {
    let runner = echo_runner();
    let mut output: Vec<u8> = Vec::new();
    runner
        .serve(Cursor::new(Vec::<u8>::new()), &mut output)
        .expect("EOF on empty input is a clean stop");
    assert!(output.is_empty(), "no input lines means no output lines");
}

#[test]
fn serve_stops_at_eof_without_a_trailing_newline() {
    let runner = echo_runner();
    // last line has no terminating '\n'; BufRead::lines still yields it
    let input = Cursor::new("1\n2".as_bytes());
    let mut output: Vec<u8> = Vec::new();
    runner.serve(input, &mut output).unwrap();

    let text = String::from_utf8(output).unwrap();
    let lines: Vec<Value> = text.lines().map(parse).collect();
    assert_eq!(lines, vec![json!({ "output": 1 }), json!({ "output": 2 })]);
}

// ---- residence: one compile, many answers ----

#[test]
fn one_runner_answers_many_lines_against_the_same_compiled_module() {
    let runner = echo_runner();
    for i in 0..256 {
        let value = parse(&runner.answer(&i.to_string()));
        assert_eq!(value, json!({ "output": i }));
    }
}
