//! The pure watch core: turn a store + contract into the single number the
//! ratchet reacts to (distinct recorded inputs of the contract's span scope)
//! and the pure decision of whether that number warrants a recompile.
//!
//! Neither function runs a subprocess or touches the registry — they are the
//! side-effect-free heart of a cycle (`cycle.rs` composes them with the
//! recompile subprocess and the publish step).

use std::path::Path;

use auto_backend::differential::gather_observations;
use auto_contract::Contract;
use auto_contract::harness::HarnessError;
use auto_trace::{Store, TraceError};

use crate::DaemonError;

/// Count the distinct canonical inputs recorded for the contract's span scope
/// in the store at `store_path`. This is the daemon's watched signal: the
/// ratchet fires when it grows (spec/runtime.md §4). It is exactly the
/// `distinct_inputs` the emit gate's differential pass reports
/// (`auto_backend::differential::gather_observations`), so what the daemon
/// watches and what a recompile is graded on are the same number.
///
/// An empty store — no traces yet for the contract's task — is **zero, not an
/// error**: the daemon may be started before any deopt has landed, and it
/// simply no-ops until evidence appears. A whole-task-scope contract carries
/// no per-input I/O in v0 (there is nothing to count), so it is reported as a
/// contract fault; any other store read failure is a store fault.
pub fn distinct_input_count(store_path: &Path, contract: &Contract) -> Result<usize, DaemonError> {
    // ADR-0025: gather_observations now gathers task-level groups, but the
    // recompile subprocess (`auto compile`) still refuses task scope — a
    // watched task-scope contract would fire recompiles that always fail.
    if matches!(contract.scope, auto_contract::Scope::Task) {
        return Err(DaemonError::Contract {
            contract: contract.task.clone(),
            detail: "task-scope contracts are not compilable, so the daemon cannot                      watch them (span or region scope required; ADR-0025)"
                .to_owned(),
        });
    }
    let store = Store::open(store_path).map_err(|e| DaemonError::Store {
        store: store_path.display().to_string(),
        detail: e.to_string(),
    })?;
    match gather_observations(&store, contract) {
        Ok(gathered) => Ok(gathered.groups.len()),
        // No traces recorded for this task yet: zero distinct inputs. The
        // whole point of a watch loop is to start before the evidence exists.
        Err(HarnessError::Trace(TraceError::UnknownTask(_))) => Ok(0),
        // Any other trace read failure is a genuine store problem.
        Err(HarnessError::Trace(e)) => Err(DaemonError::Store {
            store: store_path.display().to_string(),
            detail: e.to_string(),
        }),
        // Every remaining harness error is a scope the daemon cannot watch as
        // a distinct-input count: a whole-task scope (no per-input I/O in v0),
        // or a region scope that is unverifiable/ill-formed against traces.
        // The frozen DaemonError has no dedicated "unwatchable contract"
        // variant, so these are surfaced as a contract fault, labeled by task
        // (ADR-0013). The wildcard also keeps the daemon robust as the harness
        // scope taxonomy grows.
        Err(scope_fault) => Err(DaemonError::Contract {
            contract: contract.task.clone(),
            detail: scope_fault.to_string(),
        }),
    }
}

/// Should the current distinct-input count trigger a recompile?
///
/// True when `current` has grown past `last_compiled` **and** is nonzero. A
/// fresh daemon (`last_compiled == None`) treats the first nonzero count as
/// recompile-worthy: the operator started the daemon because they want an
/// artifact built from the evidence already present.
///
/// The watermark is **in-memory only** in v0, so a restart re-observes
/// `last_compiled == None` and recompiles once redundantly. That is stated and
/// harmless: the emit gate is content-addressed, so a redundant recompile of
/// unchanged evidence produces byte-identical output that the registry dedupes
/// (ADR-0013).
pub fn should_recompile(last_compiled: Option<usize>, current: usize) -> bool {
    current > last_compiled.unwrap_or(0) && current > 0
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use auto_contract::Contract;
    use auto_trace::Store;
    use auto_trace::model::{Span, SpanId, SpanKind, Trace, TraceHeader, TraceId};
    use serde_json::{Value, json};

    use super::*;

    /// One synthetic single-span trace of task `t`, span `model_call("m")` —
    /// the shape `auto run` ingests on deopt (auto-cli `ingest_deopt_observation`).
    fn span_trace(id: u128, input: Value, output: Value) -> Trace {
        Trace {
            header: TraceHeader {
                trace_id: TraceId(id),
                task: "t".into(),
                started_at_ms: 0,
                sdk: "auto-cli-deopt/test".into(),
                attrs: BTreeMap::new(),
                task_input: None,
                task_output: None,
            },
            spans: vec![Span {
                span_id: SpanId(1),
                parent_span_id: None,
                seq: 1,
                kind: SpanKind::ModelCall,
                name: "m".into(),
                input,
                output: Some(output),
                error: None,
                started_at_ms: 0,
                duration_ms: 5,
                attrs: BTreeMap::new(),
            }],
        }
    }

    /// Load a contract from TOML text (mirrors the real daemon, which loads the
    /// operator's contract file — no hand-built structs, so no `auto-ir` dep).
    fn load_contract_str(text: &str) -> Contract {
        auto_contract::parse::from_toml_str(text, Path::new(".")).expect("contract parses")
    }

    /// Span-scope contract matching `span_trace` (task `t`, `model_call("m")`).
    fn span_contract() -> Contract {
        load_contract_str(
            "contract_version = 0\n\
             task = \"t\"\n\
             [scope]\n\
             type = \"span\"\n\
             kind = \"model_call\"\n\
             name = \"m\"\n\
             [interface]\n\
             input = \"json\"\n\
             output = \"text\"\n",
        )
    }

    fn store_with(traces: Vec<Trace>) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("store.db");
        let mut store = Store::open(&path).expect("open store");
        for t in traces {
            store.ingest(&t).expect("ingest");
        }
        (dir, path)
    }

    #[test]
    fn should_recompile_truth_table() {
        // fresh daemon: first nonzero count recompiles, zero does not
        assert!(!should_recompile(None, 0));
        assert!(should_recompile(None, 1));
        assert!(should_recompile(None, 5));
        // watermark set: only growth past it recompiles
        assert!(!should_recompile(Some(2), 2));
        assert!(!should_recompile(Some(2), 1));
        assert!(should_recompile(Some(2), 3));
        // a zero count never recompiles, whatever the watermark
        assert!(!should_recompile(Some(0), 0));
        assert!(should_recompile(Some(0), 1));
    }

    #[test]
    fn counts_distinct_canonical_inputs() {
        // two distinct inputs; the third repeats the first, so it does not add
        let (_dir, path) = store_with(vec![
            span_trace(1, json!({"x": 1}), json!("a")),
            span_trace(2, json!({"x": 2}), json!("b")),
            span_trace(3, json!({"x": 1}), json!("a")),
        ]);
        let count = distinct_input_count(&path, &span_contract()).expect("count");
        assert_eq!(count, 2);
    }

    #[test]
    fn empty_store_is_zero_not_an_error() {
        // a store with no traces for the task: the daemon started early
        let (_dir, path) = store_with(vec![]);
        let count = distinct_input_count(&path, &span_contract()).expect("empty store counts zero");
        assert_eq!(count, 0);
    }

    #[test]
    fn task_scope_contract_is_a_contract_fault() {
        let (_dir, path) = store_with(vec![span_trace(1, json!({"x": 1}), json!("a"))]);
        let contract = load_contract_str(
            "contract_version = 0\n\
             task = \"t\"\n\
             [scope]\n\
             type = \"task\"\n\
             [interface]\n\
             input = \"json\"\n\
             output = \"text\"\n",
        );
        match distinct_input_count(&path, &contract) {
            Err(DaemonError::Contract { contract, detail }) => {
                assert_eq!(contract, "t");
                assert!(detail.contains("task-scope"), "detail: {detail}");
            }
            other => panic!("expected Contract error, got {other:?}"),
        }
    }

    #[test]
    fn unreadable_store_is_a_store_fault() {
        // a path that exists but is not a sqlite database: Store::open fails
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("not-a-db");
        std::fs::write(&path, b"this is not sqlite").expect("write junk");
        match distinct_input_count(&path, &span_contract()) {
            Err(DaemonError::Store { store, .. }) => {
                assert_eq!(store, Path::new(&path).display().to_string());
            }
            other => panic!("expected Store error, got {other:?}"),
        }
    }
}
