//! LLM-guided CEGIS protocol tests (`auto_passes::synthesize_llm`).
//!
//! These test the *caller's protocol* — request construction, the round /
//! counterexample loop, response parsing, verification, and the honest
//! outcomes — using [`auto_frontier::ScriptedFrontier`], the labelled test
//! fake. It is NOT a mock pretending to be a frontier model: no network, no
//! paid call, no key. Every scripted answer is a candidate *we* wrote, and the
//! test asserts what the loop does with it. Acceptance is unchanged from the
//! enumerative checker: `auto_dsl::eval` verifies every proposal against every
//! witness.
//!
//! Fixtures use the toy-agent "fake-frontier" rule spelled in the DSL
//! (`evals/toy-agent/agent.py:fake_model`; `auto-dsl`'s `fake_frontier_program`):
//! lowercase, split on whitespace, strip `.,`, keep words longer than 4 chars,
//! dedup+sort, take 3, join with spaces. Witness outputs are computed by
//! evaluating that rule (`target()`), and the primary one is pinned to the
//! recorded golden ("brown jumps quick", evals/toy-agent/cases.jsonl).

use auto_frontier::{FrontierError, ScriptedFrontier};
use auto_passes::auto_dsl::{Op, Program, eval};
use auto_passes::{CegisConfig, CegisOutcome, Observation, synthesize_llm};
use serde_json::json;

/// The fake-frontier keyword-extraction rule, in the DSL. Mirrors
/// `auto-dsl`'s `fake_frontier_program` and the python `fake_model`.
fn target() -> Program {
    Program::new(vec![
        Op::GetField {
            key: "prompt".into(),
        },
        Op::Lowercase,
        Op::SplitWhitespace,
        Op::TrimEachMatches { set: ".,".into() },
        Op::FilterLongerThan { n: 4 },
        Op::DedupSort,
        Op::Take { n: 3 },
        Op::Join { sep: " ".into() },
    ])
}

const DOC_A: &str = "The quick brown fox jumps over the lazy dog near the riverbank.";
const DOC_B: &str = "Compilers translate agent cognition into fast deterministic binaries.";

/// A witness whose output is the rule applied to the doc (honest by
/// construction: the expected output is what the target program computes).
fn witness(doc: &str) -> Observation {
    let input = json!({ "prompt": doc });
    let output = eval(&target(), &input).expect("the target rule evaluates on the doc");
    Observation { input, output }
}

fn witnesses() -> Vec<Observation> {
    vec![witness(DOC_A), witness(DOC_B)]
}

/// Serialize programs as the JSON array a well-behaved model would return.
fn array_of(programs: &[Program]) -> String {
    let items: Vec<String> = programs.iter().map(Program::to_json).collect();
    format!("[{}]", items.join(","))
}

/// Queue one round's answer. Token/cost numbers are the fake's own accounting
/// (the fake never checks a cap and CEGIS ignores usage; a real capped client
/// would compute cost from the pinned price table before returning).
fn push_round(frontier: &mut ScriptedFrontier, body: &str) {
    frontier.push_text(body, 200, 40, 0);
}

// (1) round-1 success => Found, and the request carried the witnesses + catalogue
#[test]
fn round_one_success_returns_found_and_sends_witnesses() {
    // pin the fixture to the recorded toy-agent golden
    assert_eq!(witness(DOC_A).output, json!("brown jumps quick"));

    let mut frontier = ScriptedFrontier::new("scripted-model");
    push_round(&mut frontier, &array_of(&[target()]));

    let config = CegisConfig::default();
    let outcome = synthesize_llm(&witnesses(), &mut frontier, &config);

    match outcome {
        CegisOutcome::Found {
            program,
            rounds_used,
            candidates_tried,
        } => {
            assert_eq!(program, target());
            assert_eq!(rounds_used, 1);
            assert_eq!(candidates_tried, 1);
        }
        other => panic!("expected Found, got {other:?}"),
    }

    assert_eq!(frontier.requests.len(), 1);
    let request = &frontier.requests[0];
    assert_eq!(request.max_output_tokens, config.max_output_tokens);

    // the witnesses (both inputs and both expected outputs) are in the prompt
    let a_out = witness(DOC_A).output;
    let b_out = witness(DOC_B).output;
    assert!(request.user.contains("quick brown fox"));
    assert!(request.user.contains(a_out.as_str().unwrap()));
    assert!(request.user.contains(b_out.as_str().unwrap()));

    // the system prompt enumerates the closed DSL and demands a bare JSON array
    assert!(request.system.contains("dsl_version"));
    assert!(request.system.contains("get_field"));
    assert!(request.system.contains("const_out"));
    assert!(request.system.contains("JSON array"));
}

// (2) counterexample loop: wrong round-1, correct round-2 => Found at round 2,
//     and round-2's request feeds the failed program back with the mismatch
#[test]
fn counterexample_loop_feeds_the_failed_candidate_back() {
    // returns the raw prompt string, not the extracted keywords — a value
    // mismatch on every witness
    let wrong = Program::new(vec![Op::GetField {
        key: "prompt".into(),
    }]);

    let mut frontier = ScriptedFrontier::new("scripted-model");
    push_round(&mut frontier, &array_of(std::slice::from_ref(&wrong)));
    push_round(&mut frontier, &array_of(&[target()]));

    let outcome = synthesize_llm(&witnesses(), &mut frontier, &CegisConfig::default());
    match outcome {
        CegisOutcome::Found {
            program,
            rounds_used,
            candidates_tried,
        } => {
            assert_eq!(program, target());
            assert_eq!(rounds_used, 2);
            assert_eq!(candidates_tried, 2);
        }
        other => panic!("expected Found, got {other:?}"),
    }
    assert_eq!(frontier.requests.len(), 2);

    let wrong_json = wrong.to_json();
    // round 1 has no counterexamples yet
    assert!(!frontier.requests[0].user.contains(&wrong_json));
    assert!(!frontier.requests[0].user.contains("expected"));

    // round 2 feeds the failed program back, tied to the first mismatching
    // witness (expected vs got)
    let second = &frontier.requests[1].user;
    let idx = second
        .find(&wrong_json)
        .expect("the failed candidate is fed into round 2");
    let tail = &second[idx..];
    assert!(tail.contains("expected"));
    assert!(tail.contains("got"));
}

// (3) unparseable / non-array / invalid-Program each count as a tried candidate;
//     a fully-scripted run ends in NoCandidateVerified (not a script-exhaustion
//     Api error, i.e. not FrontierRefused)
#[test]
fn malformed_responses_are_tried_candidates_then_no_candidate_verified() {
    let mut frontier = ScriptedFrontier::new("scripted-model");
    push_round(&mut frontier, "this is not json at all"); // unparseable
    push_round(&mut frontier, "{\"dsl_version\":0,\"ops\":[\"lowercase\"]}"); // JSON object, not an array
    push_round(
        &mut frontier,
        "[{\"dsl_version\":0,\"ops\":[\"warp_speed\"]}]",
    ); // array, element is not a valid program

    let config = CegisConfig {
        max_rounds: 3,
        max_candidates_per_round: 3,
        max_output_tokens: 2000,
    };
    let outcome = synthesize_llm(&witnesses(), &mut frontier, &config);
    match outcome {
        CegisOutcome::NoCandidateVerified {
            rounds_used,
            candidates_tried,
            last_error,
        } => {
            assert_eq!(rounds_used, 3);
            assert_eq!(candidates_tried, 3); // one malformed candidate per round
            assert!(!last_error.is_empty());
        }
        other => panic!("expected NoCandidateVerified, got {other:?}"),
    }
    // exactly max_rounds calls: the script was constructed fully, so this is
    // honest proposal exhaustion, not a FrontierRefused from an exhausted script
    assert_eq!(frontier.requests.len(), 3);
}

// (4) CapExceeded scripted mid-loop => FrontierRefused mentioning the cap
#[test]
fn cap_exceeded_mid_loop_is_frontier_refused() {
    let wrong = Program::new(vec![Op::GetField {
        key: "prompt".into(),
    }]);

    let mut frontier = ScriptedFrontier::new("scripted-model");
    push_round(&mut frontier, &array_of(&[wrong])); // round 1: a wrong proposal
    frontier.push_error(FrontierError::CapExceeded {
        spent_usd_micros: 0,
        estimated_usd_micros: 5_000,
        cap_usd_micros: 0,
    });

    let outcome = synthesize_llm(&witnesses(), &mut frontier, &CegisConfig::default());
    match outcome {
        CegisOutcome::FrontierRefused { error } => {
            assert!(
                error.contains("cap"),
                "error should mention the cap: {error}"
            );
        }
        other => panic!("expected FrontierRefused, got {other:?}"),
    }
    // asked twice, refused on the second — an honest stop mid-loop
    assert_eq!(frontier.requests.len(), 2);
}

// (5) conflicting observations => ConflictingObservations, no frontier call
#[test]
fn conflicting_observations_short_circuit_without_calling_the_frontier() {
    // same canonical input (sorted keys collapse the key order), two outputs
    let observations = vec![
        Observation {
            input: json!({ "a": 1, "b": 2 }),
            output: json!("x"),
        },
        Observation {
            input: json!({ "b": 2, "a": 1 }),
            output: json!("y"),
        },
    ];

    let mut frontier = ScriptedFrontier::new("scripted-model");
    let outcome = synthesize_llm(&observations, &mut frontier, &CegisConfig::default());
    match outcome {
        CegisOutcome::ConflictingObservations { detail } => assert!(!detail.is_empty()),
        other => panic!("expected ConflictingObservations, got {other:?}"),
    }
    assert!(
        frontier.requests.is_empty(),
        "conflicting observations must not call the frontier"
    );
}

// (6) a candidate that eval-errors on a witness is a counterexample, not a crash
#[test]
fn eval_error_on_a_witness_is_a_counterexample_not_a_crash() {
    // lowercase on the object input {"prompt":…} is a typed eval error (not a
    // value mismatch); it must be caught and fed back, never panic
    let bad = Program::new(vec![Op::Lowercase]);

    let mut frontier = ScriptedFrontier::new("scripted-model");
    push_round(&mut frontier, &array_of(std::slice::from_ref(&bad)));
    push_round(&mut frontier, &array_of(&[target()]));

    let outcome = synthesize_llm(&witnesses(), &mut frontier, &CegisConfig::default());
    match outcome {
        CegisOutcome::Found {
            program,
            rounds_used,
            candidates_tried,
        } => {
            assert_eq!(program, target());
            assert_eq!(rounds_used, 2);
            assert_eq!(candidates_tried, 2);
        }
        other => panic!("expected Found, got {other:?}"),
    }

    // round 2 carries the eval-error counterexample for the bad program
    let second = &frontier.requests[1].user;
    assert!(second.contains(&bad.to_json()));
    assert!(second.contains("eval error"));
    // the auto-dsl TypeMismatch message ("… expected text, register holds object")
    assert!(second.contains("expected text"));
}
