//! Tier-0 frontier binding tests (`auto_runtime::tier0`).
//!
//! These test the caller's protocol — spec parsing, prompt construction,
//! answer handling — with [`auto_frontier::ScriptedFrontier`], the labelled
//! test fake. No network, no key, no paid call anywhere. The answers are
//! strings WE scripted; what is under test is what the binding does with
//! them: parse-or-refuse, the text-interface fallback, and honest error
//! surfacing (spec/runtime.md §3).

use auto_backend::manifest::{Manifest, Measured, Provenance};
use auto_frontier::{FrontierError, ScriptedFrontier};
use auto_runtime::tier0::{Tier0Spec, frontier_answer};
use serde_json::json;

/// An honest manifest fixture for a task whose declared output type is
/// `output_ty` — the fields a real distilled emit writes, with measured-zero
/// latencies (this manifest never gated anything; it exists to be read).
fn manifest(output_ty: &str) -> Manifest {
    Manifest {
        manifest_version: 0,
        task: "distill-agent".into(),
        scope_kind: "model_call".into(),
        scope_name: "fuzzy-router".into(),
        interface_input: "json".into(),
        interface_output: output_ty.into(),
        capabilities: vec![],
        contract_id: "test-contract-id".into(),
        eval_run_ids: vec![],
        provenance: Provenance {
            trace_ids: vec![],
            reference: "test fixture".into(),
            observations: 0,
        },
        measured: Measured {
            compiled_latency_ms_p50: 0,
            compiled_latency_ms_p95: 0,
            compiled_latency_ms_max: 0,
            reference_recorded_latency_ms_p95: 0,
        },
        notes: "test fixture manifest — never emitted".into(),
    }
}

// ---- Tier0Spec::parse ----

#[test]
fn command_specs_split_on_whitespace() {
    let spec = Tier0Spec::parse("python evals/toy-agent/tier0_oracle.py --flag").expect("parses");
    assert_eq!(
        spec,
        Tier0Spec::Command(vec![
            "python".into(),
            "evals/toy-agent/tier0_oracle.py".into(),
            "--flag".into(),
        ])
    );
}

#[test]
fn frontier_prefix_selects_the_frontier_form() {
    let spec = Tier0Spec::parse("frontier:gpt-5.4-mini").expect("parses");
    assert_eq!(
        spec,
        Tier0Spec::Frontier {
            model: "gpt-5.4-mini".into()
        }
    );
    // surrounding whitespace is tolerated
    let spaced = Tier0Spec::parse("  frontier:gpt-5.4-mini  ").expect("parses");
    assert_eq!(spec, spaced);
}

#[test]
fn frontier_prefix_without_a_model_is_an_error() {
    let err = Tier0Spec::parse("frontier:").expect_err("no model id");
    assert!(err.contains("model id"), "{err}");
    let err = Tier0Spec::parse("frontier:   ").expect_err("whitespace model id");
    assert!(err.contains("model id"), "{err}");
}

#[test]
fn empty_specs_are_errors() {
    assert!(Tier0Spec::parse("").is_err());
    assert!(Tier0Spec::parse("   ").is_err());
}

#[test]
fn the_word_frontier_without_the_prefix_stays_a_command() {
    let spec = Tier0Spec::parse("frontier --model x").expect("parses");
    assert!(matches!(spec, Tier0Spec::Command(argv) if argv[0] == "frontier"));
}

// ---- frontier_answer ----

#[test]
fn clean_json_answer_round_trips_and_the_request_frames_the_task() {
    let mut frontier = ScriptedFrontier::new("gpt-5.4-mini");
    frontier.push_text("\"urgent\"", 50, 5, 100);

    let input = json!({"text": "A breach just hit the payments api."});
    let answer =
        frontier_answer(&manifest("text"), &input, &mut frontier, 256).expect("clean JSON parses");
    assert_eq!(answer, json!("urgent"));

    assert_eq!(frontier.requests.len(), 1, "exactly one call, no retries");
    let request = &frontier.requests[0];
    // the system prompt frames the model as the reference implementation of
    // the manifest's task/scope/interface
    assert!(request.system.contains("distill-agent"));
    assert!(request.system.contains("model_call"));
    assert!(request.system.contains("fuzzy-router"));
    assert!(request.system.contains("(json) -> (text)"));
    // the user turn is the canonical input JSON
    assert_eq!(
        request.user,
        auto_trace::model::canonical_json(&input),
        "user turn is the canonical input, mirroring the command oracle's argv"
    );
    assert_eq!(request.max_output_tokens, 256);
}

#[test]
fn fenced_json_answer_is_unwrapped() {
    let mut frontier = ScriptedFrontier::new("gpt-5.4-mini");
    frontier.push_text("```json\n\"urgent\"\n```", 50, 8, 100);
    let answer = frontier_answer(&manifest("text"), &json!({"text": "x"}), &mut frontier, 256)
        .expect("fenced JSON is unwrapped");
    assert_eq!(answer, json!("urgent"));
}

#[test]
fn bare_text_answer_is_accepted_only_for_text_interfaces() {
    // models routinely answer a text task with bare prose; for a declared
    // text output that IS the value (ingestion still conformance-checks it)
    let mut frontier = ScriptedFrontier::new("gpt-5.4-mini");
    frontier.push_text("urgent", 50, 2, 100);
    let answer = frontier_answer(&manifest("text"), &json!({"text": "x"}), &mut frontier, 256)
        .expect("bare text accepted for text interface");
    assert_eq!(answer, json!("urgent"));

    // for a json interface the same bare answer is refused, never guessed at
    let mut frontier = ScriptedFrontier::new("gpt-5.4-mini");
    frontier.push_text("urgent", 50, 2, 100);
    let err = frontier_answer(&manifest("json"), &json!({"text": "x"}), &mut frontier, 256)
        .expect_err("bare prose is not a json value");
    assert!(err.contains("not text"), "{err}");
    assert!(
        err.contains("urgent"),
        "the snippet names the answer: {err}"
    );
}

#[test]
fn frontier_errors_surface_verbatim_never_answered_around() {
    let mut frontier = ScriptedFrontier::new("gpt-5.4-mini");
    frontier.push_error(FrontierError::CapExceeded {
        spent_usd_micros: 24_999_000,
        estimated_usd_micros: 5_000,
        cap_usd_micros: 25_000_000,
    });
    let err = frontier_answer(&manifest("text"), &json!({"text": "x"}), &mut frontier, 256)
        .expect_err("a refusal is a refusal");
    assert!(err.contains("spend cap"), "{err}");
}
