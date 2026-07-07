//! The resident stdio runner: one artifact, held open, answering a
//! line-oriented JSON protocol until EOF.
//!
//! The wave-5 systems finding this exists to kill: a compiled `run` is under a
//! millisecond, but a one-shot `auto run` pays process spawn plus module
//! compilation on every call, and those dominate. [`Runner`] compiles the
//! module ONCE (at [`Runner::new`], via [`WasmExecutor::from_artifact`]) and
//! answers many inputs against it — each answer still executes in a fresh wasm
//! instance (frozen ABI: one `run` per instance), so no cross-call state
//! leaks; only the expensive compile is amortized. An agent spawns one such
//! process and pipes it work.
//!
//! Protocol: one JSON value in per line, one JSON OBJECT out per line. For
//! each input line [`Runner::answer`] mirrors `auto run`'s decision and
//! serve's object shapes (spec/runtime.md §9):
//!
//! - unparseable JSON => `{"error": <detail>}`
//! - a guarded artifact whose guard trips =>
//!   `{"abstained": true, "reason", "distance", "threshold"}` (§2, §5)
//! - otherwise tier-1 executes => `{"output": <value>}` or `{"error": <detail>}`
//!
//! [`Runner::serve`] loops lines until EOF and flushes after every response,
//! so a caller blocked on the pipe always sees a whole line. Like `auto serve`
//! there is no tier-0 here: a trip abstains, it never deopts (the same
//! per-request spend-policy gap, §1/§8). A capability-bearing artifact (nonempty
//! manifest `capabilities`) loads through a tool table handed to
//! [`Runner::new_with_tools`] (ADR-0017); [`Runner::new`] hands none, so a pure
//! artifact loads unchanged and a capability artifact refuses through the
//! loader, naming its missing tools. Pure artifacts are unaffected either way.

use std::io::{BufRead, Write};

use auto_backend::Artifact;
use auto_backend::container::GUARD_ENTRY;
use auto_trace::model::canonical_json;
use serde_json::{Value, json};

use crate::executor::{HostTools, WasmExecutor};
use crate::guard::{Guard, GuardOutcome};

/// One artifact held resident: the compiled module (reused across every line)
/// and the parsed guard, if the artifact carried one.
///
/// Built once from artifact bytes by [`Runner::new`] / [`Runner::new_with_tools`];
/// [`Runner::answer`] and [`Runner::serve`] borrow it immutably, so a single
/// `Runner` answers any number of lines with no per-call setup beyond a fresh
/// wasm instance.
pub struct Runner {
    executor: WasmExecutor,
    /// `None` when the artifact carried no `guard.json`: it runs tier-1
    /// unguarded, mirroring `auto run` and `auto serve`.
    guard: Option<Guard>,
}

impl Runner {
    /// Parse artifact bytes, compile the module once, and parse the guard if
    /// present — with **no** tool host. A pure artifact loads; a
    /// capability-bearing artifact (nonempty manifest `capabilities`) refuses
    /// through the loader, which names the missing tools. Equivalent to
    /// [`Runner::new_with_tools`]`(artifact_bytes, None)`.
    pub fn new(artifact_bytes: &[u8]) -> Result<Runner, String> {
        Self::new_with_tools(artifact_bytes, None)
    }

    /// Parse artifact bytes, compile the module once, and parse the guard if
    /// present, loading the module through `tools` (ADR-0017). Module loading —
    /// and every cross-check it runs — is delegated to
    /// [`WasmExecutor::from_artifact_with_tools`], so all of the loader's
    /// refusals surface here as the error string: `None` demands a pure,
    /// zero-import module and refuses a capability artifact (naming the missing
    /// tools); `Some(table)` loads a capability artifact whose declared
    /// capabilities the table must cover, refuses an import outside
    /// `auto.tool_call`, and refuses a host attached to a pure artifact.
    ///
    /// Errors (all as plain strings): unparseable container or manifest, a
    /// non-utf-8 or malformed guard, and any loader refusal (missing/uncovered
    /// tools, an unexpected import, a host on a pure artifact, or a module that
    /// does not compile).
    pub fn new_with_tools(
        artifact_bytes: &[u8],
        tools: Option<HostTools>,
    ) -> Result<Runner, String> {
        let artifact =
            Artifact::from_bytes(artifact_bytes).map_err(|e| format!("invalid artifact: {e}"))?;
        // parse + version-check the manifest first so a broken manifest fails
        // here, not mid-run; the loader re-reads its capability list to decide
        // whether (and how) the tool host applies.
        artifact
            .manifest()
            .map_err(|e| format!("invalid manifest: {e}"))?;
        let guard = match artifact.entries.get(GUARD_ENTRY) {
            None => None,
            Some(raw) => {
                let text = std::str::from_utf8(raw).map_err(|_| "guard is not utf-8".to_owned())?;
                Some(Guard::from_json(text).map_err(|e| format!("invalid guard: {e}"))?)
            }
        };
        let executor = WasmExecutor::from_artifact_with_tools(&artifact, tools)
            .map_err(|e| format!("cannot load module: {e}"))?;
        Ok(Runner { executor, guard })
    }

    /// Answer one protocol line: one JSON value in, one JSON object out (no
    /// trailing newline — [`Runner::serve`] frames lines). Never panics and
    /// never emits prose; every outcome is a single JSON object.
    pub fn answer(&self, input_line: &str) -> String {
        canonical_json(&self.answer_value(input_line))
    }

    /// The response object for one line, before serialization. Mirrors
    /// `auto run`: bad JSON, then guard, then execute.
    fn answer_value(&self, input_line: &str) -> Value {
        let input: Value = match serde_json::from_str(input_line) {
            Ok(value) => value,
            Err(e) => return json!({ "error": format!("input is not valid JSON: {e}") }),
        };

        // guard first: a trip ABSTAINS. No tier-0 in the runner — a resident
        // deopt needs a per-call spend policy that does not exist (§1/§8),
        // exactly as `auto serve` and `auto run` without `--tier0`.
        if let Some(guard) = &self.guard
            && let GuardOutcome::Trip {
                reason,
                distance,
                threshold,
            } = guard.evaluate(&input)
        {
            return json!({
                "abstained": true,
                "reason": reason,
                "distance": distance,
                "threshold": threshold,
            });
        }

        match self.executor.execute(&input) {
            Ok(output) => json!({ "output": output }),
            Err(e) => json!({ "error": format!("tier-1 execution failed: {e}") }),
        }
    }

    /// Serve the protocol over any reader/writer until the reader hits EOF.
    /// Writes one response line per input line and flushes after each, so a
    /// caller blocked on the pipe sees a whole line before it reads the next.
    ///
    /// Pure with respect to its arguments — it touches no real stdin/stdout,
    /// which is what lets it be driven from an in-memory buffer in tests. The
    /// CLI's `auto run --stdio` is the shell that hands it the process's
    /// locked stdin/stdout.
    pub fn serve(&self, input: impl BufRead, mut output: impl Write) -> Result<(), String> {
        for line in input.lines() {
            let line = line.map_err(|e| format!("stdin read failed: {e}"))?;
            writeln!(output, "{}", self.answer(&line))
                .map_err(|e| format!("stdout write failed: {e}"))?;
            output
                .flush()
                .map_err(|e| format!("stdout flush failed: {e}"))?;
        }
        Ok(())
    }
}
