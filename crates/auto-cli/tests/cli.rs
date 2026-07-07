//! Integration tests for the `auto` binary. std-only: no network, no extra
//! test deps. `CARGO_BIN_EXE_auto` is provided by cargo for [[bin]] targets.

use std::path::PathBuf;
use std::process::{Command, Output};

fn auto(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_auto"))
        .args(args)
        .output()
        .expect("spawn auto binary")
}

fn golden(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../auto-ir/tests/golden")
        .join(name);
    path.to_str().expect("utf-8 path").to_owned()
}

#[test]
fn inspect_prints_golden_graph() {
    let out = auto(&["inspect", &golden("tool_chain.air")]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("utf-8 stdout");
    assert!(stdout.contains("graph \"tool-chain\""));
    assert!(stdout.contains("tool_call(http.get)"));
    assert!(stdout.contains("[deterministic]"));
    assert!(stdout.contains("caps={net}"));
}

#[test]
fn inspect_prints_generative_effects() {
    let out = auto(&["inspect", &golden("generative_effects.air")]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("utf-8 stdout");
    assert!(stdout.contains("[generative]"));
    assert!(stdout.contains("caps={net,secrets}"));
    assert!(stdout.contains("mem={read,append}"));
    assert!(stdout.contains("max_tokens=4096"));
}

#[test]
fn inspect_rejects_non_ir_file() {
    // this crate's own manifest is a file that exists but is not IR
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let out = auto(&["inspect", manifest.to_str().expect("utf-8 path")]);
    assert!(!out.status.success(), "must exit nonzero on invalid input");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("invalid IR"), "stderr: {stderr}");
}

#[test]
fn inspect_rejects_missing_file() {
    let out = auto(&["inspect", "no/such/file.air"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot read"));
}

/// The echo module: implements the frozen ABI and returns its input bytes
/// unchanged. WAT text is a valid module input (wasmtime's wat feature).
const ECHO_WAT: &str = r#"(module
  (memory (export "memory") 2)
  (global $next (mut i32) (i32.const 4096))
  (func (export "alloc") (param i32) (result i32)
    global.get $next
    global.get $next local.get 0 i32.add
    global.set $next)
  (func (export "run") (param i32 i32) (result i64)
    local.get 0 i64.extend_i32_u i64.const 32 i64.shl
    local.get 1 i64.extend_i32_u i64.or))"#;

/// A wrong implementation: always returns "{}" (data segment at 0, len 2).
const WRONG_WAT: &str = r#"(module
  (memory (export "memory") 1)
  (data (i32.const 0) "{}")
  (global $next (mut i32) (i32.const 1024))
  (func (export "alloc") (param i32) (result i32)
    global.get $next
    global.get $next local.get 0 i32.add
    global.set $next)
  (func (export "run") (param i32 i32) (result i64)
    i64.const 2))"#;

const ECHO_CONTRACT: &str = concat!(
    "contract_version = 0\n",
    "task = \"cli-cbin\"\n\n",
    "[scope]\ntype = \"span\"\nkind = \"tool_call\"\nname = \"echo\"\n\n",
    "[interface]\ninput = \"json\"\noutput = \"json\"\n\n",
    "[[example]]\nname = \"seven\"\nmatch = \"exact\"\n",
    "input = { v = 7 }\noutput = { v = 7 }\n\n",
    "[[property]]\nkind = \"json_has_keys\"\ntarget = \"output\"\nkeys = [\"v\"]\n\n",
    "[budgets]\nmax_latency_ms_p95 = 5000\n",
);

/// Record an echo span twice through the real SDK, then drive the whole S3
/// surface: compile (gated emit) -> run (tier-1) -> inspect (artifact).
#[test]
fn compile_run_inspect_full_cycle() {
    let Some(python) = find_python() else {
        eprintln!("SKIPPED compile_run_inspect_full_cycle: no python interpreter found");
        return;
    };
    let dir = temp_dir("cbin-e2e");
    let store = dir.join("store.db");
    let sdk = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../sdk/python");
    let script = dir.join("child.py");
    std::fs::write(
        &script,
        format!(
            "import sys\nsys.path.insert(0, {sdk:?})\n\
             from auto_sdk import Tracer\n\
             with Tracer(task='cli-cbin') as t:\n\
             \x20   t.tool_call('echo', {{'v': 7}}, lambda: {{'v': 7}})\n",
            sdk = sdk.to_str().unwrap()
        ),
    )
    .unwrap();
    for _ in 0..2 {
        let out = auto(&[
            "record",
            "--store",
            store.to_str().unwrap(),
            "--",
            &python,
            script.to_str().unwrap(),
        ]);
        assert!(
            out.status.success(),
            "record failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let contract = dir.join("echo.contract.toml");
    std::fs::write(&contract, ECHO_CONTRACT).unwrap();
    let module = dir.join("echo.wat");
    std::fs::write(&module, ECHO_WAT).unwrap();
    let artifact = dir.join("echo.cbin");
    let runs = dir.join("runs");

    let out = auto(&[
        "compile",
        "--contract",
        contract.to_str().unwrap(),
        "--store",
        store.to_str().unwrap(),
        "--module",
        module.to_str().unwrap(),
        "--out",
        artifact.to_str().unwrap(),
        "--runs-dir",
        runs.to_str().unwrap(),
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "compile failed: {}\n{stdout}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("verdict: PASS"), "{stdout}");
    assert!(stdout.contains("artifact "), "{stdout}");
    assert!(artifact.is_file(), "artifact must exist after PASS");

    let out = auto(&[
        "run",
        "--artifact",
        artifact.to_str().unwrap(),
        "--input",
        "{\"v\":7}",
    ]);
    assert!(
        out.status.success(),
        "run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "{\"v\":7}",
        "tier-1 execution must reproduce the recorded mapping"
    );

    // nonconforming input is refused before execution
    let out = auto(&[
        "run",
        "--artifact",
        artifact.to_str().unwrap(),
        "--input",
        "not json",
    ]);
    assert!(!out.status.success());

    let out = auto(&["inspect", artifact.to_str().unwrap()]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("manifest v0"), "{stdout}");
    assert!(stdout.contains("capabilities: none"), "{stdout}");
    assert!(stdout.contains("module.wasm"), "{stdout}");
    assert!(stdout.contains("graph.air"), "{stdout}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// S4: without --module, compile synthesizes the implementation. With one
/// distinct recorded input the search finds the constant program — the
/// documented honesty boundary (spec/synthesis.md) — and the full artifact
/// cycle still works.
#[test]
fn compile_synthesizes_without_module() {
    let Some(python) = find_python() else {
        eprintln!("SKIPPED compile_synthesizes_without_module: no python interpreter found");
        return;
    };
    let dir = temp_dir("cbin-synth");
    let store = dir.join("store.db");
    let sdk = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../sdk/python");
    let script = dir.join("child.py");
    std::fs::write(
        &script,
        format!(
            "import sys\nsys.path.insert(0, {sdk:?})\n\
             from auto_sdk import Tracer\n\
             with Tracer(task='cli-cbin') as t:\n\
             \x20   t.tool_call('echo', {{'v': 7}}, lambda: {{'v': 7}})\n",
            sdk = sdk.to_str().unwrap()
        ),
    )
    .unwrap();
    let out = auto(&[
        "record",
        "--store",
        store.to_str().unwrap(),
        "--",
        &python,
        script.to_str().unwrap(),
    ]);
    assert!(out.status.success());

    let contract = dir.join("echo.contract.toml");
    std::fs::write(&contract, ECHO_CONTRACT).unwrap();
    let artifact = dir.join("synth.cbin");
    let out = auto(&[
        "compile",
        "--contract",
        contract.to_str().unwrap(),
        "--store",
        store.to_str().unwrap(),
        "--out",
        artifact.to_str().unwrap(),
        "--runs-dir",
        dir.join("runs").to_str().unwrap(),
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "synthesized compile failed: {}\n{stdout}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("synthesized: 1 op(s) over 1 distinct input(s)"),
        "{stdout}"
    );
    assert!(stdout.contains("verdict: PASS"), "{stdout}");

    let out = auto(&[
        "run",
        "--artifact",
        artifact.to_str().unwrap(),
        "--input",
        "{\"v\":7}",
    ]);
    assert!(
        out.status.success(),
        "run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "{\"v\":7}");

    let out = auto(&["inspect", artifact.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("program.json"), "{stdout}");
    assert!(stdout.contains("S4 synthesized compile"), "{stdout}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The emit gate is the product: a wrong implementation must be BLOCKED,
/// with the report saying why and no artifact written.
#[test]
fn compile_blocks_emit_for_wrong_module() {
    let Some(python) = find_python() else {
        eprintln!("SKIPPED compile_blocks_emit_for_wrong_module: no python interpreter found");
        return;
    };
    let dir = temp_dir("cbin-blocked");
    let store = dir.join("store.db");
    let sdk = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../sdk/python");
    let script = dir.join("child.py");
    std::fs::write(
        &script,
        format!(
            "import sys\nsys.path.insert(0, {sdk:?})\n\
             from auto_sdk import Tracer\n\
             with Tracer(task='cli-cbin') as t:\n\
             \x20   t.tool_call('echo', {{'v': 7}}, lambda: {{'v': 7}})\n",
            sdk = sdk.to_str().unwrap()
        ),
    )
    .unwrap();
    let out = auto(&[
        "record",
        "--store",
        store.to_str().unwrap(),
        "--",
        &python,
        script.to_str().unwrap(),
    ]);
    assert!(out.status.success());

    let contract = dir.join("echo.contract.toml");
    std::fs::write(&contract, ECHO_CONTRACT).unwrap();
    let module = dir.join("wrong.wat");
    std::fs::write(&module, WRONG_WAT).unwrap();
    let artifact = dir.join("wrong.cbin");

    let out = auto(&[
        "compile",
        "--contract",
        contract.to_str().unwrap(),
        "--store",
        store.to_str().unwrap(),
        "--module",
        module.to_str().unwrap(),
        "--out",
        artifact.to_str().unwrap(),
        "--runs-dir",
        dir.join("runs").to_str().unwrap(),
    ]);
    assert!(!out.status.success(), "wrong module must not compile");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("emit blocked"), "{stderr}");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("verdict: FAIL"),
        "the report must show the failure"
    );
    assert!(!artifact.exists(), "no artifact may exist after a FAIL");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn record_rejects_uninstrumented_command() {
    let dir = temp_dir("record-plain");
    let store = dir.join("store.db");
    // a real command that produces no trace: rustc --version exists wherever
    // this workspace builds
    let out = auto(&[
        "record",
        "--store",
        store.to_str().unwrap(),
        "--",
        "rustc",
        "--version",
    ]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("produced no trace"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn report_on_missing_store_fails() {
    let out = auto(&["report", "--task", "x", "--store", "no/such/store.db"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("does not exist"));
}

/// Full record -> report loop through a real python child process using the
/// real SDK. Skips (loudly) when python is not installed; CI always runs it.
#[test]
fn record_and_report_end_to_end() {
    let Some(python) = find_python() else {
        eprintln!("SKIPPED record_and_report_end_to_end: no python interpreter found");
        return;
    };
    let dir = temp_dir("record-e2e");
    let store = dir.join("store.db");
    let sdk = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../sdk/python");
    let script = dir.join("child.py");
    std::fs::write(
        &script,
        format!(
            "import sys\nsys.path.insert(0, {sdk:?})\n\
             from auto_sdk import Tracer\n\
             with Tracer(task='cli-e2e') as t:\n\
             \x20   t.tool_call('add', {{'a': 1, 'b': 2}}, lambda: 3)\n",
            sdk = sdk.to_str().unwrap()
        ),
    )
    .unwrap();

    for _ in 0..2 {
        let out = auto(&[
            "record",
            "--store",
            store.to_str().unwrap(),
            "--",
            &python,
            script.to_str().unwrap(),
        ]);
        assert!(
            out.status.success(),
            "record failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stdout).contains("recorded trace"));
    }

    let out = auto(&[
        "report",
        "--task",
        "cli-e2e",
        "--store",
        store.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "report failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("determinism report — task \"cli-e2e\" (2 traces)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("deterministic: 2 spans (100.0% of witnessed)"),
        "{stdout}"
    );

    // verify a contract over the recorded spans; writes an eval-run record
    let contract = dir.join("add.contract.toml");
    std::fs::write(
        &contract,
        concat!(
            "contract_version = 0\n",
            "task = \"cli-e2e\"\n\n",
            "[scope]\ntype = \"span\"\nkind = \"tool_call\"\nname = \"add\"\n\n",
            "[interface]\ninput = \"json\"\noutput = \"int\"\n\n",
            "[[example]]\nname = \"one-plus-two\"\nmatch = \"exact\"\n",
            "input = { a = 1, b = 2 }\noutput = 3\n\n",
            "[[property]]\nkind = \"num_range\"\ntarget = \"output\"\nmin = 0\nmax = 100\n\n",
            "[budgets]\nmax_latency_ms_p95 = 5000\n",
        ),
    )
    .unwrap();
    let runs_dir = dir.join("runs");
    let out = auto(&[
        "verify",
        "--contract",
        contract.to_str().unwrap(),
        "--store",
        store.to_str().unwrap(),
        "--runs-dir",
        runs_dir.to_str().unwrap(),
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "verify failed: {}\n{stdout}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("verdict: PASS"), "{stdout}");
    assert!(stdout.contains("eval run "), "{stdout}");
    assert!(
        std::fs::read_dir(&runs_dir).unwrap().count() == 1,
        "exactly one eval-run record expected"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn find_python() -> Option<String> {
    for candidate in ["python3", "python"] {
        let probe = Command::new(candidate).arg("--version").output();
        if matches!(probe, Ok(o) if o.status.success()) {
            return Some(candidate.to_owned());
        }
    }
    None
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("auto-cli-test-{label}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create test temp dir");
    dir
}

#[test]
fn help_discloses_s3_limits() {
    let out = auto(&["--help"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("hand-assisted"),
        "--help must disclose that S3 compile is hand-assisted: {stdout}"
    );
    assert!(
        stdout.contains("tier-1"),
        "--help must disclose that run is tier-1 only: {stdout}"
    );
}
