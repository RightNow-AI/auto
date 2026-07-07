//! Tier-0, the interpreter side of the ratchet — a real frontier model as the
//! deopt target (the S6 frontier binding; spec/runtime.md §3).
//!
//! v0's tier-0 had one form: a pluggable local **command** oracle
//! (spec/runtime.md §3) that `auto run` invokes on a guard trip. This module
//! adds the second, constitution-named form — a **frontier model** — behind
//! the same protocol semantics. The interpreter is handed the canonical-JSON
//! input and answers with the output value; that answer is **unverified
//! reference authority**, which the existing ingestion path conformance-checks
//! against the manifest, records as a synthetic single-span observation, and
//! recompilation folds back in (the ratchet: nothing figured out twice).
//!
//! [`Tier0Spec::parse`] turns a `--tier0` string into one of those two forms;
//! [`frontier_answer`] runs the frontier one.
//!
//! **Spend is not governed here.** This module never opens a socket and never
//! reads a price table — it drives whatever [`auto_frontier::Frontier`] it is
//! handed. The hard per-session spend cap, the append-only ledger, and the
//! fail-closed default (a cap of 0, the default everywhere, refuses every paid
//! call; a missing key refuses before any request) live in the capped client
//! the caller constructs (ADR-0010; CLAUDE.md guardrail). A caller that wires a
//! cap-0 client here cannot make a paid call, by construction.

use auto_backend::manifest::Manifest;
use auto_frontier::{Frontier, FrontierRequest};
use serde_json::Value;

/// How `auto run --tier0 <spec>` resolves the interpreter for a guard trip.
///
/// The `frontier:` prefix is **reserved**: any spec that (after trimming)
/// begins with it selects [`Tier0Spec::Frontier`], so a local command whose
/// program name is literally `frontier:...` is shadowed and unreachable through
/// this syntax. Every other spec is a whitespace-split command
/// ([`Tier0Spec::Command`]) — the v0 pluggable oracle (spec/runtime.md §3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tier0Spec {
    /// A local command: argv split on whitespace (no shell quoting). At run
    /// time the runtime appends exactly one argument — the canonical JSON of
    /// the input (spec/runtime.md §3).
    Command(Vec<String>),
    /// A frontier model id, answered by [`frontier_answer`] through a
    /// spend-capped [`auto_frontier::Frontier`] client (ADR-0010).
    Frontier { model: String },
}

impl Tier0Spec {
    /// The reserved prefix that selects the frontier interpreter.
    pub const FRONTIER_PREFIX: &'static str = "frontier:";

    /// Parse a `--tier0` spec.
    ///
    /// - `"frontier:<model-id>"` → [`Tier0Spec::Frontier`]; a missing or
    ///   whitespace-only `<model-id>` is an error — there is nothing to call.
    /// - anything else → [`Tier0Spec::Command`], argv split on whitespace; an
    ///   empty or whitespace-only spec is an error — there is no command.
    ///
    /// The `frontier:` prefix is reserved (see the type docs): a command whose
    /// program name is literally `frontier:x` cannot be expressed here.
    pub fn parse(raw: &str) -> Result<Tier0Spec, String> {
        let raw = raw.trim();
        if let Some(model) = raw.strip_prefix(Self::FRONTIER_PREFIX) {
            let model = model.trim();
            if model.is_empty() {
                return Err(format!(
                    "tier-0 spec {raw:?}: the reserved \"{prefix}\" prefix needs a \
                     model id (e.g. \"{prefix}<model-id>\")",
                    prefix = Self::FRONTIER_PREFIX,
                ));
            }
            return Ok(Tier0Spec::Frontier {
                model: model.to_owned(),
            });
        }
        let argv: Vec<String> = raw.split_whitespace().map(str::to_owned).collect();
        if argv.is_empty() {
            return Err(
                "tier-0 spec is empty: expected a command (argv split on whitespace) \
                 or \"frontier:<model-id>\""
                    .to_owned(),
            );
        }
        Ok(Tier0Spec::Command(argv))
    }
}

/// Answer one guard-tripped input by asking a frontier model to act as the
/// task's reference implementation.
///
/// The request frames the model as the reference implementation of the
/// manifest's task, scope, and interface and asks for exactly the output value
/// as a single JSON value; the user turn is the canonical JSON of `input`
/// (spec/trace.md §4), mirroring the one argument the command oracle receives.
///
/// The answer is **unverified reference authority** — exactly like the pluggable
/// command oracle's stdout. This function does not conformance-check it; the
/// ingestion path in `auto run` (spec/runtime.md §3–§4) checks it against the
/// manifest's declared output type, records it as a synthetic single-span
/// observation, and recompilation folds it into both the program and the guard
/// witnesses (the ratchet: nothing figured out twice).
///
/// Response handling, in order: a single optional markdown ```-fence pair is
/// stripped; the trimmed text is parsed as a JSON value and returned on success.
/// If parsing fails **and** the manifest declares a `text` output, the trimmed
/// text is accepted verbatim as a JSON string — models routinely answer a text
/// task with bare prose rather than a quoted string, and the ingestion
/// conformance check still gates it. Otherwise the answer is rejected with an
/// honest detail; no answer is ever invented.
///
/// **No retries in v0:** exactly one [`Frontier::complete`] call is made, and
/// every [`auto_frontier::FrontierError`] — including the `CapExceeded` and
/// `MissingKey` refusals — is surfaced as its `to_string()`, never swallowed
/// and never answered around. **Spend is governed by the capped client the
/// caller constructs** (ADR-0010), never here.
pub fn frontier_answer(
    manifest: &Manifest,
    input: &Value,
    frontier: &mut dyn Frontier,
    max_output_tokens: u32,
) -> Result<Value, String> {
    let output_is_text = declared_output_is_text(manifest);

    let mut system = format!(
        "You are the reference implementation of task {task}, span {kind}({name}), \
         interface ({input_ty}) -> ({output_ty}). Reply with EXACTLY the output \
         value as a single JSON value - no prose, no markdown fences.",
        task = manifest.task,
        kind = manifest.scope_kind,
        name = manifest.scope_name,
        input_ty = manifest.interface_input,
        output_ty = manifest.interface_output,
    );
    if output_is_text {
        system.push_str(
            " The declared output type is text, so that single JSON value is a JSON string.",
        );
    }

    let request = FrontierRequest {
        system,
        user: auto_trace::model::canonical_json(input),
        max_output_tokens,
    };

    // Single call, no retries (v0). A FrontierError — transport, api, or a
    // fail-closed refusal (CapExceeded / MissingKey) — is surfaced verbatim.
    let response = frontier.complete(&request).map_err(|e| e.to_string())?;

    let answer = strip_one_fence(&response.text);
    match serde_json::from_str::<Value>(answer) {
        Ok(value) => Ok(value),
        // Declared output is text: the bare answer *is* the value. Ingestion
        // still conformance-checks it (spec/runtime.md §3).
        Err(_) if output_is_text => Ok(Value::String(answer.to_owned())),
        Err(parse_err) => Err(format!(
            "frontier answer is not a JSON value and the declared output type \
             {output:?} is not text, so it cannot be accepted verbatim \
             ({parse_err}); answer began {snippet:?}",
            output = manifest.interface_output,
            snippet = snippet(answer),
        )),
    }
}

/// Does the manifest declare a `text` (utf-8 string) output?
///
/// Interface types are value-type grammar strings (spec/ir.md §3;
/// `auto_contract::parse::parse_value_type` is the strict inverse of
/// `ValueType`'s Display). Only the utf-8 string scalar renders as exactly
/// "text", so `json`, `list<...>`, and any non-type string are all non-text and
/// never trigger the bare-string fallback.
fn declared_output_is_text(manifest: &Manifest) -> bool {
    manifest.interface_output == "text"
}

/// Strip a single optional markdown ```-fence pair, returning the inner text
/// trimmed. Only a well-formed fence — an opening ``` line (optionally
/// language-tagged) and a matching trailing ``` — is unwrapped; anything else
/// (no fence, no closing fence, a fence with no newline) is returned trimmed and
/// otherwise unchanged. At most one pair is removed.
fn strip_one_fence(raw: &str) -> &str {
    let trimmed = raw.trim();
    let Some(after_open) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    // The fenced body starts after the opening fence line, which may carry a
    // language tag such as `json`.
    let Some(newline) = after_open.find('\n') else {
        return trimmed;
    };
    match after_open[newline + 1..].strip_suffix("```") {
        Some(inner) => inner.trim(),
        None => trimmed,
    }
}

/// A short, char-boundary-safe prefix of an answer for error messages.
fn snippet(text: &str) -> String {
    const MAX: usize = 80;
    if text.chars().count() > MAX {
        let head: String = text.chars().take(MAX).collect();
        format!("{head}...")
    } else {
        text.to_owned()
    }
}
