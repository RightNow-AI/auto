//! End-to-end daemon cycles over real stores, contracts, artifacts, and a
//! real recompile subprocess — no network, no sockets (CLAUDE.md). Kept a
//! `#[cfg(test)]` module under `src/` (not a `tests/` integration binary) to
//! stay within the crate's build-out scope; it drives the crate's public
//! `run_cycle` / `daemon` exactly as an external caller would.
//!
//! The recompile stand-in is a python one-liner that copies prebuilt artifact
//! bytes to the `{out}` path the daemon chooses. Those bytes are a genuine
//! `.cbin` (built here with `auto-backend`, exactly as the registry and
//! auto-serve tests build one); the registry validates the container and
//! manifest and never executes the module, so no wasm toolchain is needed. A
//! real `auto compile` is the production recompile command — this fake stands
//! in only for the byte-writing contract the daemon depends on.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use auto_backend::{
    Artifact, MANIFEST_ENTRY, MANIFEST_VERSION, MODULE_ENTRY, Manifest, Measured, Provenance,
};
use auto_contract::Contract;
use auto_registry::Registry;
use auto_trace::Store;
use auto_trace::model::{Span, SpanId, SpanKind, Trace, TraceHeader, TraceId};
use serde_json::{Value, json};

use crate::{DaemonConfig, DaemonError, daemon, run_cycle};

/// The ECHO module wat text, stored verbatim as the module bytes. The
/// registry validates the container + manifest only and never compiles this
/// (mirrors auto-serve's api.rs fixture), so it is opaque bytes here.
const ECHO_MODULE: &str = r#"(module
    (memory (export "memory") 2)
    (func (export "run") (param i32 i32) (result i64) i64.const 0))"#;

/// A span contract matching the synthetic traces (mirror
/// evals/*/router.contract.toml shape).
const CONTRACT_TOML: &str = "contract_version = 0\n\
     task = \"t\"\n\
     [scope]\n\
     type = \"span\"\n\
     kind = \"model_call\"\n\
     name = \"m\"\n\
     [interface]\n\
     input = \"json\"\n\
     output = \"text\"\n";

fn manifest() -> Manifest {
    Manifest {
        manifest_version: MANIFEST_VERSION,
        task: "t".into(),
        scope_kind: "model_call".into(),
        scope_name: "m".into(),
        interface_input: "json".into(),
        interface_output: "text".into(),
        capabilities: vec![],
        contract_id: "c".repeat(8),
        eval_run_ids: vec!["run-1".into()],
        provenance: Provenance {
            trace_ids: vec!["0".repeat(32)],
            reference: "daemon e2e test".into(),
            observations: 2,
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

/// Genuine `.cbin` bytes the fake recompile "emits".
fn artifact_bytes() -> Vec<u8> {
    let mut entries = BTreeMap::new();
    entries.insert(
        MANIFEST_ENTRY.to_owned(),
        manifest().canonical_json().into_bytes(),
    );
    entries.insert(MODULE_ENTRY.to_owned(), ECHO_MODULE.as_bytes().to_vec());
    Artifact::new(entries).to_bytes()
}

/// One synthetic single-span trace of task `t`, span `model_call("m")` — the
/// shape `auto run` ingests on deopt (auto-cli `ingest_deopt_observation`).
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

struct Fixture {
    _dir: tempfile::TempDir,
    store: PathBuf,
    contract_path: PathBuf,
    contract: Contract,
    registry_root: PathBuf,
    /// prebuilt artifact bytes on disk, the source the fake recompile copies
    src: PathBuf,
}

/// A fresh tempdir with a store carrying `distinct_inputs` distinct-input
/// traces, the contract (written and parsed), an (unopened) registry root,
/// and prebuilt artifact bytes on disk.
fn fixture(distinct_inputs: usize) -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = dir.path().join("store.db");
    {
        let mut s = Store::open(&store).expect("open store");
        for i in 0..distinct_inputs {
            s.ingest(&span_trace(
                (i as u128) + 1,
                json!({ "x": i }),
                json!("out"),
            ))
            .expect("ingest");
        }
    }
    let contract_path = dir.path().join("router.contract.toml");
    std::fs::write(&contract_path, CONTRACT_TOML).expect("write contract");
    let contract = auto_contract::parse::load(&contract_path).expect("parse contract");
    let registry_root = dir.path().join("registry");
    let src = dir.path().join("prebuilt.cbin");
    std::fs::write(&src, artifact_bytes()).expect("write prebuilt artifact");
    Fixture {
        _dir: dir,
        store,
        contract_path,
        contract,
        registry_root,
        src,
    }
}

fn config(fx: &Fixture, recompile_argv: Vec<String>, once: bool) -> DaemonConfig {
    DaemonConfig {
        store: fx.store.clone(),
        contract: fx.contract_path.clone(),
        registry_root: fx.registry_root.clone(),
        recompile_argv,
        poll_interval_ms: 0,
        once,
        // wave-5 fields default to the wave-4 behavior; tests that exercise
        // them set the field directly on the returned config.
        watermark_path: None,
        supervise: false,
    }
}

fn published_ids(registry_root: &Path) -> Vec<String> {
    Registry::open(registry_root)
        .expect("open registry")
        .list()
        .expect("list")
        .into_iter()
        .map(|e| e.id)
        .collect()
}

/// Same probe as crates/auto-cli/tests/cli.rs and auto-passes distillation.
fn find_python() -> Option<String> {
    for candidate in ["python3", "python"] {
        let probe = Command::new(candidate).arg("--version").output();
        if matches!(probe, Ok(o) if o.status.success()) {
            return Some(candidate.to_owned());
        }
    }
    None
}

/// Recompile argv: a python one-liner that copies `src` bytes to the `{out}`
/// path. `src` is baked into the launcher (race-free — the daemon sets no env
/// on the child), mirroring the auto-passes stub-trainer pattern.
fn copy_argv(python: &str, src: &Path) -> Vec<String> {
    let launcher = format!(
        "import sys\n\
         data = open({src:?}, 'rb').read()\n\
         open(sys.argv[1], 'wb').write(data)\n",
        src = src.to_str().expect("utf-8 path"),
    );
    vec![python.to_owned(), "-c".into(), launcher, "{out}".into()]
}

/// Recompile argv that writes to stderr and exits nonzero.
fn failing_argv(python: &str) -> Vec<String> {
    vec![
        python.to_owned(),
        "-c".into(),
        "import sys\nsys.stderr.write('boom: recompile refused by gate')\nsys.exit(1)\n".into(),
        "{out}".into(),
    ]
}

/// Recompile argv that exits 0 but writes no artifact.
fn silent_argv(python: &str) -> Vec<String> {
    vec![
        python.to_owned(),
        "-c".into(),
        "import sys\nsys.stdout.write('did nothing useful')\n".into(),
        "{out}".into(),
    ]
}

/// Recompile argv that fails its first `fail_times` invocations (nonzero exit)
/// and then succeeds by copying `src` to `{out}`. A `counter` file on disk
/// carries the invocation count across the separate subprocesses, so the same
/// argv models a flaky gate that clears after a few tries (the supervised
/// retry case). `src` and `counter` are baked into the launcher — race-free,
/// the daemon sets no env on the child.
fn flaky_argv(python: &str, src: &Path, counter: &Path, fail_times: u32) -> Vec<String> {
    let launcher = format!(
        "import sys\n\
         cp = {counter:?}\n\
         try:\n\
         \x20   n = int(open(cp).read().strip())\n\
         except OSError:\n\
         \x20   n = 0\n\
         n += 1\n\
         open(cp, 'w').write(str(n))\n\
         if n <= {fail_times}:\n\
         \x20   sys.stderr.write('boom: flaky gate attempt %d' % n)\n\
         \x20   sys.exit(1)\n\
         data = open({src:?}, 'rb').read()\n\
         open(sys.argv[1], 'wb').write(data)\n",
        counter = counter.to_str().expect("utf-8 path"),
        src = src.to_str().expect("utf-8 path"),
    );
    vec![python.to_owned(), "-c".into(), launcher, "{out}".into()]
}

#[test]
fn run_cycle_publishes_the_recompiled_artifact() {
    let Some(python) = find_python() else {
        eprintln!("SKIPPED run_cycle_publishes_the_recompiled_artifact: no python interpreter");
        return;
    };
    let fx = fixture(2);
    let cfg = config(&fx, copy_argv(&python, &fx.src), true);

    let outcome = run_cycle(&cfg, &fx.contract, None).expect("cycle recompiles");
    assert_eq!(outcome.count, 2);
    let id = outcome.published.expect("an artifact was published");
    assert_eq!(id.len(), 64, "content id is 64 hex chars");
    assert_eq!(published_ids(&fx.registry_root), vec![id]);
}

#[test]
fn no_growth_is_a_noop_without_running_anything() {
    // No python: the no-op path returns before touching recompile_argv, so a
    // bogus command proves nothing was spawned.
    let fx = fixture(2);
    let cfg = config(
        &fx,
        vec!["definitely-not-a-real-program".into(), "{out}".into()],
        true,
    );
    let outcome = run_cycle(&cfg, &fx.contract, Some(2)).expect("no-op cycle");
    assert_eq!(outcome.count, 2);
    assert_eq!(outcome.published, None);
    assert!(published_ids(&fx.registry_root).is_empty());
}

#[test]
fn growth_triggers_recompile_and_content_addressing_dedupes() {
    let Some(python) = find_python() else {
        eprintln!("SKIPPED growth_triggers_recompile_and_content_addressing_dedupes: no python");
        return;
    };
    let fx = fixture(2);
    let cfg = config(&fx, copy_argv(&python, &fx.src), true);

    // fresh daemon: first nonzero count recompiles
    let first = run_cycle(&cfg, &fx.contract, None).expect("first cycle");
    assert_eq!(first.count, 2);
    let first_id = first.published.expect("first publish");

    // same count: no-op
    let noop = run_cycle(&cfg, &fx.contract, Some(2)).expect("noop cycle");
    assert_eq!(noop.count, 2);
    assert_eq!(noop.published, None);

    // a new deopt observation lands, growing the distinct-input count
    {
        let mut s = Store::open(&fx.store).expect("reopen store");
        s.ingest(&span_trace(99, json!({ "x": 99 }), json!("out")))
            .expect("ingest new observation");
    }
    let grown = run_cycle(&cfg, &fx.contract, Some(2)).expect("grown cycle");
    assert_eq!(grown.count, 3, "the new observation grew the count");
    let grown_id = grown.published.expect("recompile ran on growth");

    // identical emitted bytes → identical content id → the registry deduped:
    // redundant recompiles are harmless (ADR-0013).
    assert_eq!(grown_id, first_id);
    assert_eq!(
        published_ids(&fx.registry_root),
        vec![first_id],
        "one id despite two recompiles",
    );
}

#[test]
fn nonzero_recompile_exit_is_a_recompile_error() {
    let Some(python) = find_python() else {
        eprintln!("SKIPPED nonzero_recompile_exit_is_a_recompile_error: no python");
        return;
    };
    let fx = fixture(2);
    let cfg = config(&fx, failing_argv(&python), true);

    match run_cycle(&cfg, &fx.contract, None) {
        Err(DaemonError::Recompile {
            status,
            stderr_tail,
        }) => {
            assert!(
                stderr_tail.contains("boom: recompile refused by gate"),
                "tail: {stderr_tail}"
            );
            assert_ne!(status, "not started", "the command ran and exited nonzero");
        }
        other => panic!("expected Recompile error, got {other:?}"),
    }
    assert!(
        published_ids(&fx.registry_root).is_empty(),
        "a failed recompile publishes nothing"
    );
}

#[test]
fn recompile_that_writes_nothing_is_no_artifact() {
    let Some(python) = find_python() else {
        eprintln!("SKIPPED recompile_that_writes_nothing_is_no_artifact: no python");
        return;
    };
    let fx = fixture(2);
    let cfg = config(&fx, silent_argv(&python), true);

    match run_cycle(&cfg, &fx.contract, None) {
        Err(DaemonError::NoArtifact { out }) => {
            assert!(out.ends_with(".cbin"), "out path: {out}");
        }
        other => panic!("expected NoArtifact error, got {other:?}"),
    }
    assert!(published_ids(&fx.registry_root).is_empty());
}

#[test]
fn missing_out_placeholder_is_refused_before_spawning() {
    // The command names a program that does not exist: had the daemon tried to
    // run it we would see a "spawn failed" Recompile error. Getting a Config
    // error instead proves the missing {out} was refused first (a config fault,
    // wave-5 split) — and no python is needed.
    let fx = fixture(2);
    let cfg = config(&fx, vec!["definitely-not-a-real-program-xyz".into()], true);

    match run_cycle(&cfg, &fx.contract, None) {
        Err(DaemonError::Config { detail }) => {
            assert!(detail.contains("{out}"), "detail: {detail}");
        }
        other => panic!("expected a pre-run Config refusal, got {other:?}"),
    }
    assert!(published_ids(&fx.registry_root).is_empty());
}

#[test]
fn daemon_once_runs_a_single_cycle_and_publishes() {
    let Some(python) = find_python() else {
        eprintln!("SKIPPED daemon_once_runs_a_single_cycle_and_publishes: no python");
        return;
    };
    let fx = fixture(2);
    let cfg = config(&fx, copy_argv(&python, &fx.src), true);

    daemon(cfg).expect("daemon once");
    assert_eq!(
        published_ids(&fx.registry_root).len(),
        1,
        "once mode published exactly one artifact"
    );
}

// --- wave 5: persistent watermark -----------------------------------------

/// The exact gap the wave-4 e2e documented: a persistent watermark makes a
/// same-count cycle a no-op **across separate daemon instances** (not just
/// within one process). Instance 1 recompiles and writes the watermark file;
/// instance 2, pointed at the same file, reads it and no-ops on the unchanged
/// count — so a second `--once` process no longer recompiles redundantly.
#[test]
fn watermark_persists_across_daemon_instances() {
    let Some(python) = find_python() else {
        eprintln!("SKIPPED watermark_persists_across_daemon_instances: no python interpreter");
        return;
    };
    let fx = fixture(2);
    let wm = fx._dir.path().join("watermark.json");

    // Instance 1: fresh (no watermark file yet) → recompiles, writes watermark.
    let mut cfg1 = config(&fx, copy_argv(&python, &fx.src), true);
    cfg1.watermark_path = Some(wm.clone());
    daemon(cfg1).expect("first instance recompiles");
    assert_eq!(
        published_ids(&fx.registry_root).len(),
        1,
        "instance 1 published"
    );
    let written = std::fs::read_to_string(&wm).expect("watermark file exists after publish");
    assert_eq!(
        written, r#"{"watermark_version":0,"last_compiled_count":2}"#,
        "watermark records the compiled count"
    );

    // Instance 2: same watermark file, unchanged count. A bogus recompile
    // command proves nothing is spawned — the no-op path returns first.
    let mut cfg2 = config(
        &fx,
        vec!["definitely-not-a-real-program".into(), "{out}".into()],
        true,
    );
    cfg2.watermark_path = Some(wm.clone());
    daemon(cfg2).expect("second instance no-ops across the persisted watermark");
    assert_eq!(
        published_ids(&fx.registry_root).len(),
        1,
        "no redundant recompile across instances: still one artifact"
    );
}

/// A corrupt watermark file is a loud startup error, never a silent fresh
/// start — a wrong watermark would skip recompiles. No python needed: the
/// read happens before any cycle. A bogus recompile command proves the daemon
/// never reached a cycle.
#[test]
fn corrupt_watermark_is_a_loud_error() {
    let fx = fixture(2);
    let wm = fx._dir.path().join("watermark.json");
    std::fs::write(&wm, b"not a watermark").expect("write junk");

    let mut cfg = config(
        &fx,
        vec!["definitely-not-a-real-program".into(), "{out}".into()],
        true,
    );
    cfg.watermark_path = Some(wm.clone());
    match daemon(cfg) {
        Err(DaemonError::Watermark { path, .. }) => {
            assert_eq!(path, wm.display().to_string());
        }
        other => panic!("expected Watermark error, got {other:?}"),
    }
    assert!(
        published_ids(&fx.registry_root).is_empty(),
        "a corrupt watermark publishes nothing"
    );
}

/// A configured-but-missing watermark file is a fresh start: the daemon
/// recompiles and then writes the file, so the *next* instance can no-op.
#[test]
fn missing_watermark_starts_fresh_and_writes_one() {
    let Some(python) = find_python() else {
        eprintln!("SKIPPED missing_watermark_starts_fresh_and_writes_one: no python interpreter");
        return;
    };
    let fx = fixture(2);
    let wm = fx._dir.path().join("watermark.json");
    assert!(!wm.exists());

    let mut cfg = config(&fx, copy_argv(&python, &fx.src), true);
    cfg.watermark_path = Some(wm.clone());
    daemon(cfg).expect("fresh start recompiles");
    assert_eq!(
        published_ids(&fx.registry_root).len(),
        1,
        "fresh start published"
    );
    assert!(
        wm.exists(),
        "the fresh start wrote a watermark for next time"
    );
}

// --- wave 5: supervised mode ----------------------------------------------

/// Supervised mode retries a *retryable* failure with backoff instead of
/// exiting. A flaky recompile that fails twice then succeeds must publish on
/// the third cycle. Driven through the bounded `run_loop` (max_cycles = 3) so
/// the otherwise-infinite supervised loop terminates; `poll_interval_ms = 0`
/// keeps the backoff sleeps at zero.
#[test]
fn supervised_retries_flaky_recompile_until_it_succeeds() {
    let Some(python) = find_python() else {
        eprintln!("SKIPPED supervised_retries_flaky_recompile_until_it_succeeds: no python");
        return;
    };
    let fx = fixture(2);
    let counter = fx._dir.path().join("attempts.txt");
    let mut cfg = config(&fx, flaky_argv(&python, &fx.src, &counter, 2), false);
    cfg.supervise = true;

    // Exactly three cycles: fail (attempt 1), fail (attempt 2), succeed (3).
    crate::run::run_loop(&cfg, &fx.contract, None, Some(3))
        .expect("supervised loop rides out the flaky failures");
    assert_eq!(
        published_ids(&fx.registry_root).len(),
        1,
        "the third cycle recompiled and published"
    );
    assert_eq!(
        std::fs::read_to_string(&counter).expect("counter").trim(),
        "3",
        "the recompile ran exactly three times",
    );
}

/// Even supervised, a config-shaped error exits loudly — retrying it is a
/// tight useless loop. A missing `{out}` placeholder is a `Config` fault,
/// refused before anything is spawned; `run_loop` must return it, not retry.
#[test]
fn supervised_still_exits_loud_on_a_config_error() {
    let fx = fixture(2);
    // No `{out}` in the argv, and the program name is bogus — but the Config
    // refusal happens before any spawn, so no python is needed.
    let mut cfg = config(&fx, vec!["definitely-not-a-real-program".into()], false);
    cfg.supervise = true;

    match crate::run::run_loop(&cfg, &fx.contract, None, Some(10)) {
        Err(DaemonError::Config { detail }) => {
            assert!(detail.contains("{out}"), "detail: {detail}");
        }
        other => panic!("expected a Config error even when supervised, got {other:?}"),
    }
    assert!(published_ids(&fx.registry_root).is_empty());
}
