//! LLM-guided CEGIS — the ADR-0005 *recorded upgrade* to symbolic extraction:
//! proposal generation only (spec/synthesis.md §LLM-guided proposals).
//!
//! Enumerative extraction (`extraction.rs`) *enumerates* candidate programs
//! from a mined parameter space; this module instead asks a frontier model to
//! *propose* them, under the session spend cap (ADR-0010, the capped frontier
//! client). Everything downstream is unchanged: the same `auto_dsl::eval` —
//! the exact evaluator the artifact interprets (spec/synthesis.md §5) — checks
//! every proposal against every witness, and the emit gate still re-verifies
//! differentially and against the contract (ADR-0004). Proposal generation is
//! the *only* thing the model touches.
//!
//! Honesty (ADR-0005 decision 3): **proposal generation is nondeterministic**
//! (model sampling); **acceptance is not**. A [`CegisOutcome::Found`] program
//! is evidence-bounded exactly as in enumerative search — it reproduces the
//! witnesses, nothing more — and Fail/Inconclusive at the emit gate still
//! blocks emit. The model can *suggest*; it can never *admit*.
//!
//! The counterexample loop: each round sends the deduped witnesses plus every
//! previously-failed candidate (its program JSON and the first witness it got
//! wrong — a value mismatch, a typed eval error, or a parse error) and asks
//! for fresh proposals; the first proposal that reproduces every witness wins.
//! A [`auto_frontier::FrontierError`] at any point (cap exhaustion mid-loop
//! included) is an honest stop — [`CegisOutcome::FrontierRefused`], never a
//! fabricated result.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use auto_dsl::{Program, eval};
use auto_frontier::{Frontier, FrontierRequest};
use serde_json::{Map, Value};

use crate::extraction::Observation;

/// CEGIS loop limits. Rounds and candidates-per-round bound frontier spend;
/// `max_output_tokens` bounds each response and feeds the capped client's
/// worst-case cost check (`auto_frontier`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CegisConfig {
    pub max_rounds: usize,
    pub max_candidates_per_round: usize,
    pub max_output_tokens: u32,
}

impl Default for CegisConfig {
    /// Four rounds, three candidates per round, 2000 output tokens — a small,
    /// spend-cap-friendly default; the cap itself is the hard limit.
    fn default() -> Self {
        Self {
            max_rounds: 4,
            max_candidates_per_round: 3,
            max_output_tokens: 2000,
        }
    }
}

/// The honest outcomes of a CEGIS run. `Found` carries provenance (rounds and
/// candidates spent) for the manifest; the rest report *why* nothing was
/// emitted without ever inventing a program.
#[derive(Debug, Clone, PartialEq)]
pub enum CegisOutcome {
    /// A proposal that reproduces every witness. Evidence-bounded: the emit
    /// gate still re-verifies it (spec/synthesis.md §4).
    Found {
        program: Program,
        rounds_used: usize,
        candidates_tried: usize,
    },
    /// Every round was spent and no proposal verified. `last_error` is the
    /// most recent failure detail (a mismatch, an eval error, or a parse
    /// error).
    NoCandidateVerified {
        rounds_used: usize,
        candidates_tried: usize,
        last_error: String,
    },
    /// A `FrontierError` ended the loop — a refusal (cap exceeded, missing
    /// key) or a failed call. Nothing verified; the string is the error.
    FrontierRefused { error: String },
    /// The same canonical input was observed with different outputs. The
    /// signature is not deterministic; no frontier call is made.
    ConflictingObservations { detail: String },
}

/// The complete `auto-dsl` op catalogue in wire form, enumerated for the
/// model. **Source of truth: `crates/auto-dsl/src/lib.rs` (`enum Op`)** — this
/// must track it op-for-op; a `dsl_version` bump (ADR-0005 §6) revises both.
/// Wire form is externally tagged (serde default): unit ops are bare strings,
/// ops with fields are single-key objects (spec/synthesis.md §2).
const DSL_CATALOG: &str = r#"DSL wire format (dsl_version 0). A program is a JSON object
  {"dsl_version":0,"ops":[<op>, ...]}
applied left-to-right over ONE value register initialised to the input. Every
op is total: a wrong register shape is a typed failure, so a program that
fails on any witness simply does not fit. The complete op set (register in ->
out); unit ops are bare strings, ops with fields are single-key objects:
  {"get_field":{"key":<string>}}             object -> the field's value
  "lowercase"                                text -> text
  "uppercase"                                text -> text
  "trim"                                     text -> text
  "split_whitespace"                         text -> list<text>
  {"split_on":{"sep":<string>}}              text -> list<text>
  {"trim_each_matches":{"set":<string>}}     list<text> -> list<text> (strip set's chars both ends of each)
  {"filter_longer_than":{"n":<uint>}}        list<text> -> list<text> (keep entries with char-count > n)
  "dedup_sort"                               list<text> -> list<text> (unique, ascending)
  {"take":{"n":<uint>}}                      list -> first n
  "first"                                    list -> element (empty list fails)
  "last"                                     list -> element (empty list fails)
  {"join":{"sep":<string>}}                  list<text> -> text
  "count"                                    list -> int
  "char_count"                               text -> int (unicode scalar count)
  {"add":{"k":<int>}}                        int -> int (checked; overflow fails)
  {"const_out":{"value":<any>}}              anything -> value

A unit op is the BARE JSON STRING itself — writing {"lowercase"} or
{"split_whitespace"} is NOT JSON and the whole candidate is discarded unread.
Example of a well-formed program mixing both op shapes:
  {"dsl_version":0,"ops":[{"get_field":{"key":"text"}},"lowercase","split_whitespace",{"take":{"n":3}},{"join":{"sep":" "}}]}"#;

/// Synthesize a DSL program from observations by asking a frontier model to
/// propose candidates and verifying each against every witness.
///
/// Observations are deduped by canonical input, mirroring
/// [`crate::extraction::synthesize`] (that module keeps its witness type
/// private, so a small canonicalization is repeated here). One input seen with
/// two outputs is [`CegisOutcome::ConflictingObservations`] and no frontier
/// call is made. With no observations there is nothing to verify against — a
/// program would "reproduce" the empty set vacuously — so the run refuses to
/// propose and returns [`CegisOutcome::NoCandidateVerified`] at zero rounds.
///
/// Otherwise the round loop (`1..=max_rounds`) issues one [`FrontierRequest`]
/// per round: a constant system prompt stating the closed DSL and asking for a
/// bare JSON array of up to `max_candidates_per_round` program objects, and a
/// user prompt of the deduped witnesses plus (rounds > 1) the accumulated
/// counterexamples. Responses tolerate one optional json code fence; each array
/// element is strict-parsed to a [`Program`] and verified with `auto_dsl::eval`
/// against every witness. The first fully-verifying candidate is returned;
/// otherwise every failure (mismatch / eval error / parse error) is fed back.
pub fn synthesize_llm(
    observations: &[Observation],
    frontier: &mut dyn Frontier,
    config: &CegisConfig,
) -> CegisOutcome {
    let witnesses = match dedup(observations) {
        Ok(witnesses) => witnesses,
        Err(detail) => return CegisOutcome::ConflictingObservations { detail },
    };
    if witnesses.is_empty() {
        // No evidence => any program vacuously "reproduces" the empty witness
        // set. Accepting one would be dishonest, so refuse to propose against
        // nothing and burn no frontier calls (mirrors enumerative search's
        // no-observation no-op).
        return CegisOutcome::NoCandidateVerified {
            rounds_used: 0,
            candidates_tried: 0,
            last_error: "no observations: nothing to synthesise against".to_owned(),
        };
    }

    let system = build_system(config.max_candidates_per_round);
    let mut failures: Vec<Failure> = Vec::new();
    let mut candidates_tried = 0_usize;
    let mut last_error = String::new();

    for round in 1..=config.max_rounds {
        let request = FrontierRequest {
            system: system.clone(),
            user: build_user(&witnesses, &failures),
            max_output_tokens: config.max_output_tokens,
        };
        let response = match frontier.complete(&request) {
            Ok(response) => response,
            Err(error) => {
                return CegisOutcome::FrontierRefused {
                    error: error.to_string(),
                };
            }
        };

        let candidates = match parse_candidates(&response.text) {
            Ok(candidates) => candidates,
            Err(error) => {
                // A whole-response failure (not JSON / not an array) is one
                // malformed candidate, carried into the next round.
                candidates_tried += 1;
                last_error = error.clone();
                failures.push(Failure::Parse {
                    candidate: clip(response.text.trim()),
                    error,
                });
                continue;
            }
        };
        if candidates.is_empty() {
            last_error = "response was an empty candidate array".to_owned();
            continue;
        }

        for candidate in candidates {
            candidates_tried += 1;
            let program = match candidate {
                Ok(program) => program,
                Err((candidate, error)) => {
                    last_error = format!("candidate {candidate}: parse error: {error}");
                    failures.push(Failure::Parse { candidate, error });
                    continue;
                }
            };
            match check(&program, &witnesses) {
                Verdict::Verified => {
                    return CegisOutcome::Found {
                        program,
                        rounds_used: round,
                        candidates_tried,
                    };
                }
                Verdict::Mismatch {
                    input,
                    expected,
                    got,
                } => {
                    let program = program.to_json();
                    last_error = format!(
                        "program {program} on input {input}: expected {expected}, got {got}"
                    );
                    failures.push(Failure::Mismatch {
                        program,
                        input,
                        expected,
                        got,
                    });
                }
                Verdict::EvalError { input, error } => {
                    let program = program.to_json();
                    last_error = format!("program {program} on input {input}: eval error: {error}");
                    failures.push(Failure::EvalError {
                        program,
                        input,
                        error,
                    });
                }
            }
        }
    }

    CegisOutcome::NoCandidateVerified {
        rounds_used: config.max_rounds,
        candidates_tried,
        last_error: if last_error.is_empty() {
            "no candidate verified within the round budget".to_owned()
        } else {
            last_error
        },
    }
}

// ---- witnesses ----

/// One deduped observation: the input to evaluate on, plus canonical strings
/// for comparison, conflict detection, and the prompt line (all computed once).
struct Witness {
    input: Value,
    canon_input: String,
    canon_output: String,
    /// canonical `{"input":…,"output":…}` — the exact line sent to the model
    line: String,
}

/// Dedupe by canonical input (`serde_json::to_string`, sorted keys), mirroring
/// [`crate::extraction::synthesize`]. Same input with different outputs is a
/// conflict; the returned witnesses are in ascending canonical-input order
/// (BTreeMap), so the prompt is a pure function of the observation *set*.
fn dedup(observations: &[Observation]) -> Result<Vec<Witness>, String> {
    let mut map: BTreeMap<String, Witness> = BTreeMap::new();
    for obs in observations {
        let canon_input = canonical(&obs.input);
        let canon_output = canonical(&obs.output);
        match map.entry(canon_input.clone()) {
            Entry::Occupied(existing) => {
                if existing.get().canon_output != canon_output {
                    return Err(format!(
                        "input {canon_input} recorded with conflicting outputs {} and {canon_output}",
                        existing.get().canon_output,
                    ));
                }
            }
            Entry::Vacant(slot) => {
                let mut object = Map::new();
                object.insert("input".to_owned(), obs.input.clone());
                object.insert("output".to_owned(), obs.output.clone());
                slot.insert(Witness {
                    input: obs.input.clone(),
                    canon_input,
                    canon_output,
                    line: canonical(&Value::Object(object)),
                });
            }
        }
    }
    Ok(map.into_values().collect())
}

// ---- prompt construction ----

/// Constant system prompt: the closed DSL plus the "bare JSON array of up to N
/// programs, nothing else" instruction. Same every round; only the user prompt
/// grows with counterexamples.
fn build_system(max_candidates: usize) -> String {
    format!(
        "You are a program synthesiser for Auto's symbolic-extraction pass.\n\
         {DSL_CATALOG}\n\n\
         Propose up to {max_candidates} candidate program(s) that reproduce EVERY \
         witnessed input->output pair below. Respond with ONLY a JSON array of program \
         objects and nothing else: no prose, no explanation, no markdown code fences. \
         Each element is one program object {{\"dsl_version\":0,\"ops\":[...]}}, best \
         guess first."
    )
}

/// User prompt: the witnesses (one canonical `{"input":…,"output":…}` per
/// line), then — rounds after the first — the accumulated counterexamples.
fn build_user(witnesses: &[Witness], failures: &[Failure]) -> String {
    let mut user =
        String::from("Witnesses — the program must reproduce every one (canonical JSON):\n");
    for witness in witnesses {
        user.push_str(&witness.line);
        user.push('\n');
    }
    if !failures.is_empty() {
        user.push_str(
            "\nAlready tried and FAILED — do not repeat these, and fix what they got wrong:\n",
        );
        for failure in failures {
            user.push_str(&failure.render());
            user.push('\n');
        }
    }
    user
}

/// A candidate that did not fit, rendered back into the next prompt.
enum Failure {
    /// Parsed and ran, but produced the wrong value on a witness.
    Mismatch {
        program: String,
        input: String,
        expected: String,
        got: String,
    },
    /// Parsed, but the DSL evaluator failed typed on a witness.
    EvalError {
        program: String,
        input: String,
        error: String,
    },
    /// Not a valid program (or the response was not a JSON array).
    Parse { candidate: String, error: String },
}

impl Failure {
    fn render(&self) -> String {
        match self {
            Failure::Mismatch {
                program,
                input,
                expected,
                got,
            } => format!("- program {program} on input {input}: expected {expected}, got {got}"),
            Failure::EvalError {
                program,
                input,
                error,
            } => format!("- program {program} on input {input}: eval error: {error}"),
            Failure::Parse { candidate, error } => {
                format!("- candidate {candidate}: parse error: {error}")
            }
        }
    }
}

// ---- verification (unchanged from the enumerative checker: auto_dsl::eval) ----

/// The result of checking one candidate against every witness.
enum Verdict {
    Verified,
    Mismatch {
        input: String,
        expected: String,
        got: String,
    },
    EvalError {
        input: String,
        error: String,
    },
}

/// Verify a candidate against every witness with `auto_dsl::eval` (the same
/// evaluator compiled into the artifact). The first witness it fails — a value
/// mismatch or a typed eval error — is the counterexample.
fn check(program: &Program, witnesses: &[Witness]) -> Verdict {
    for witness in witnesses {
        match eval(program, &witness.input) {
            Ok(got) => {
                let got = canonical(&got);
                if got != witness.canon_output {
                    return Verdict::Mismatch {
                        input: clip(&witness.canon_input),
                        expected: clip(&witness.canon_output),
                        got: clip(&got),
                    };
                }
            }
            Err(error) => {
                return Verdict::EvalError {
                    input: clip(&witness.canon_input),
                    error: error.to_string(),
                };
            }
        }
    }
    Verdict::Verified
}

// ---- response parsing ----

/// One parsed candidate: `Ok` a strict-parsed program, or `Err((raw, error))`
/// a proposal that was not a valid program (kept for the counterexample loop).
type Candidate = Result<Program, (String, String)>;

/// Parse a model response into candidate programs. Tolerates one optional
/// json code fence and prose around the array — the first balanced top-level
/// JSON array is extracted before giving up, because models narrate despite
/// instructions. `Ok(vec)` on a JSON array (each element parsed strictly via
/// [`Program::from_json`]); `Err` when no JSON array can be found at all —
/// itself one malformed candidate, carrying a response snippet so the
/// counterexample (and the operator) can see what actually came back.
fn parse_candidates(text: &str) -> Result<Vec<Candidate>, String> {
    let body = strip_code_fences(text);
    let value: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(direct_err) => match first_json_array(body) {
            Some(embedded) => serde_json::from_str(embedded).map_err(|e| {
                format!(
                    "response is not JSON ({direct_err}) and its embedded array is \
                     not JSON either ({e}); response began {}",
                    clip(body)
                )
            })?,
            None => {
                return Err(format!(
                    "response is not JSON ({direct_err}) and contains no JSON array; \
                     response began {}",
                    clip(body)
                ));
            }
        },
    };
    let Value::Array(elements) = value else {
        return Err(format!(
            "response is not a JSON array of programs (found {})",
            type_name(&value)
        ));
    };
    Ok(elements
        .into_iter()
        .map(|element| {
            let text = element.to_string();
            match Program::from_json(&text) {
                Ok(program) => Ok(program),
                Err(error) => Err((clip(&text), error.to_string())),
            }
        })
        .collect())
}

/// The first balanced top-level JSON array in `text`, string- and
/// escape-aware (a `]` inside a JSON string does not close the array).
/// `None` when no `[` opens or no balancing `]` closes it.
fn first_json_array(text: &str) -> Option<&str> {
    let start = text.find('[')?;
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, &b) in bytes[start..].iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..=start + offset]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Strip one optional leading (possibly json-tagged) triple-backtick fence and
/// its trailing counterpart. A bare
/// (unfenced) response is returned untouched.
fn strip_code_fences(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(after_open) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    // drop the rest of the opening fence line ("json" tag or nothing)
    let after_line = match after_open.find('\n') {
        Some(newline) => &after_open[newline + 1..],
        None => "",
    };
    after_line
        .trim_end()
        .strip_suffix("```")
        .unwrap_or(after_line)
        .trim()
}

// ---- small helpers ----

/// Canonical JSON (`serde_json::to_string` — object keys sorted, since
/// `preserve_order` is off workspace-wide), matching `extraction.rs`.
fn canonical(value: &Value) -> String {
    serde_json::to_string(value).expect("Value serialization cannot fail")
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Bound a field rendered back into a prompt (prompts are paid input tokens).
/// Test fixtures stay well under the cap, so truncation never perturbs them.
fn clip(text: &str) -> String {
    const MAX: usize = 1000;
    if text.len() <= MAX {
        return text.to_owned();
    }
    let mut end = MAX;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…(clipped)", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pure-helper unit tests; the protocol/round-loop tests (ScriptedFrontier)
    // live in tests/cegis.rs.

    #[test]
    fn fences_are_tolerated_and_bare_arrays_pass_through() {
        assert_eq!(strip_code_fences("[\"trim\"]"), "[\"trim\"]");
        assert_eq!(strip_code_fences("```json\n[\"trim\"]\n```"), "[\"trim\"]");
        assert_eq!(strip_code_fences("```\n[\"trim\"]\n```"), "[\"trim\"]");
    }

    #[test]
    fn prose_wrapped_arrays_are_extracted() {
        let wrapped =
            "Here are two candidates:\n[{\"dsl_version\":0,\"ops\":[\"trim\"]}]\nGood luck!";
        let parsed = parse_candidates(wrapped).expect("the embedded array is extracted");
        assert_eq!(parsed.len(), 1);
        assert!(parsed[0].is_ok());
    }

    #[test]
    fn array_extraction_is_string_aware() {
        // the "]" inside the op's string argument must not close the array
        let tricky = "noise [{\"dsl_version\":0,\"ops\":[{\"split_on\":{\"sep\":\"]\"}}]}] tail";
        let inner = first_json_array(tricky).expect("balanced array found");
        assert!(inner.starts_with('[') && inner.ends_with(']'));
        let parsed = parse_candidates(tricky).expect("extracted despite the bracket in a string");
        assert_eq!(parsed.len(), 1);
        assert!(parsed[0].is_ok());
    }

    #[test]
    fn no_array_anywhere_reports_a_snippet() {
        let error = parse_candidates("I cannot help with that.").expect_err("no array to find");
        assert!(error.contains("contains no JSON array"), "{error}");
        assert!(error.contains("I cannot help"), "snippet included: {error}");
    }

    #[test]
    fn a_json_object_is_not_a_candidate_array() {
        let error = parse_candidates("{\"dsl_version\":0,\"ops\":[\"trim\"]}")
            .expect_err("a bare object is not a candidate array");
        assert!(error.contains("not a JSON array"), "{error}");
    }

    #[test]
    fn array_elements_are_strict_parsed_each() {
        // one valid program, one unknown op -> Ok(program) then Err(parse)
        let parsed = parse_candidates("[{\"dsl_version\":0,\"ops\":[\"trim\"]},\"nope\"]")
            .expect("the response is a JSON array");
        assert_eq!(parsed.len(), 2);
        assert!(parsed[0].is_ok());
        assert!(parsed[1].is_err());
    }
}
