//! Cross-language golden: the EXACT bytes `sdk/python` emitted for one
//! toy-agent run with task-level I/O (ADR-0025), parsed by the strict rust
//! parser and carried through the store. This pins the SDK wire → rust
//! ingestion seam the CLI's `auto record` path rides; if either side drifts,
//! this fails before the toy e2e does.
//!
//! Fixture provenance: `AUTO_TRACE_FILE=... python evals/toy-agent/agent.py`
//! (auto-sdk-python/0.1.0, 2026-07-05), verbatim.

use auto_trace::jsonl::parse_str;
use auto_trace::store::Store;
use serde_json::json;

const PYTHON_SDK_RECORDING: &str = r#"{"attrs":{},"sdk":"auto-sdk-python/0.1.0","started_at_ms":1783215959117,"t":"trace","task":"toy-agent","task_input":{"doc":"The quick brown fox jumps over the lazy dog near the riverbank."},"trace_id":"0a3c0525726ff6788106a14d6ff54960","v":0}
{"attrs":{},"duration_ms":0,"error":null,"input":{"text":"The quick brown fox jumps over the lazy dog near the riverbank."},"kind":"tool_call","name":"wordcount","output":12,"parent_span_id":1,"seq":2,"span_id":2,"started_at_ms":1783215959117,"t":"span","trace_id":"0a3c0525726ff6788106a14d6ff54960","v":0}
{"attrs":{},"duration_ms":0,"error":null,"input":{"prompt":"The quick brown fox jumps over the lazy dog near the riverbank."},"kind":"model_call","name":"fake-frontier","output":"brown jumps quick","parent_span_id":1,"seq":3,"span_id":3,"started_at_ms":1783215959117,"t":"span","trace_id":"0a3c0525726ff6788106a14d6ff54960","v":0}
{"attrs":{},"duration_ms":0,"error":null,"input":{"n":12},"kind":"branch","name":"length-router","output":"long","parent_span_id":1,"seq":4,"span_id":4,"started_at_ms":1783215959117,"t":"span","trace_id":"0a3c0525726ff6788106a14d6ff54960","v":0}
{"attrs":{},"duration_ms":0,"error":null,"input":{"key":"summaries","value":"brown jumps quick"},"kind":"memory_op","name":"append","output":null,"parent_span_id":1,"seq":5,"span_id":5,"started_at_ms":1783215959117,"t":"span","trace_id":"0a3c0525726ff6788106a14d6ff54960","v":0}
{"attrs":{},"duration_ms":0,"error":null,"input":{},"kind":"tool_call","name":"clock.now_ms","output":1783215959117,"parent_span_id":1,"seq":6,"span_id":6,"started_at_ms":1783215959117,"t":"span","trace_id":"0a3c0525726ff6788106a14d6ff54960","v":0}
{"attrs":{},"duration_ms":0,"error":null,"input":{},"kind":"span","name":"run","output":null,"parent_span_id":null,"seq":1,"span_id":1,"started_at_ms":1783215959117,"t":"span","trace_id":"0a3c0525726ff6788106a14d6ff54960","v":0}
{"output":{"route":"long","summary":"brown jumps quick","words":12},"recorded_at_ms":1783215959118,"t":"task_output","trace_id":"0a3c0525726ff6788106a14d6ff54960","v":0}
"#;

#[test]
fn python_sdk_task_io_recording_parses_and_stores() {
    let trace = parse_str(PYTHON_SDK_RECORDING).expect("real python SDK output parses strictly");
    assert_eq!(trace.header.task, "toy-agent");
    assert_eq!(trace.spans.len(), 6); // 5 effectful + the structural "run" span
    assert_eq!(
        trace.header.task_input,
        Some(json!({
            "doc": "The quick brown fox jumps over the lazy dog near the riverbank."
        }))
    );
    let (input, output) = trace
        .header
        .task_observation()
        .expect("both task input and output recorded");
    assert_eq!(
        input["doc"],
        json!("The quick brown fox jumps over the lazy dog near the riverbank.")
    );
    assert_eq!(
        output.value,
        json!({"route": "long", "summary": "brown jumps quick", "words": 12})
    );
    // wall-clock: run start -> output declaration, same recorder clock
    assert_eq!(output.recorded_at_ms - trace.header.started_at_ms, 1);

    let dir = tempfile::tempdir().expect("tempdir");
    let mut store = Store::open(&dir.path().join("g.db")).expect("open");
    store.ingest(&trace).expect("ingest");
    let loaded = store.load_trace(trace.header.trace_id).expect("load");
    assert_eq!(loaded, trace);
}
