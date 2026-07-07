//! Distillation orchestration — the S5 driver (spec/distillation.md).
//!
//! [`distill`] hands recorded observations to an **external trainer
//! process** over a fixed CLI protocol and gates acceptance on the trainer's
//! own measured metrics: observations go out as canonical JSONL, the trainer
//! writes a frozen-format model json (auto-model) and prints one metrics
//! line, exit code 3 means "trained, but below the holdout threshold".
//! Nothing here trains and nothing here invents numbers: every field of
//! [`TrainerMetrics`] is parsed from the trainer's final stdout line, and
//! the returned model must pass [`auto_model::Model::from_json`] validation
//! before it is accepted.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Deserialize;

use crate::extraction::Observation;

/// Metrics reported by the trainer, deserialized from its final non-empty
/// stdout line.
///
/// `deny_unknown_fields` is intentionally OFF: trainers may add fields
/// (per-class breakdowns, library versions, timings) without breaking this
/// orchestrator. The fields below are the required core; a line missing any
/// of them is [`DistillError::BadMetrics`].
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TrainerMetrics {
    pub train_accuracy: f64,
    pub holdout_accuracy: f64,
    pub train_n: usize,
    pub holdout_n: usize,
    pub classes: Vec<String>,
    pub trainer: String,
    /// witness-mass train accuracy (ADR-0031); present only when the
    /// trainer engaged weights — never invented on this side
    #[serde(default)]
    pub weighted_train_accuracy: Option<f64>,
    /// total witness weight trained on (ADR-0031)
    #[serde(default)]
    pub train_weight: Option<usize>,
}

/// An accepted distillation: the trainer's model json (validated against the
/// frozen format) plus its measured metrics — never synthesized on this side.
#[derive(Debug, Clone, PartialEq)]
pub struct Distilled {
    pub model_json: String,
    pub metrics: TrainerMetrics,
}

/// Every honest way a distillation round can fail.
#[derive(Debug, thiserror::Error)]
pub enum DistillError {
    /// the trainer process could not be started (or the command was empty)
    #[error("cannot spawn trainer `{cmd}`: {error}")]
    TrainerSpawn { cmd: String, error: String },
    /// nonzero exit other than the below-threshold code 3
    #[error("trainer failed ({status}); stderr tail: {stderr_tail}")]
    TrainerFailed { status: String, stderr_tail: String },
    /// exit code 3: the trainer trained a model but measured holdout
    /// accuracy below the requested minimum — an honest rejection carrying
    /// the trainer's own number, never one computed here
    #[error("holdout accuracy {holdout_accuracy} below threshold; metrics: {metrics_line}")]
    BelowThreshold {
        holdout_accuracy: f64,
        metrics_line: String,
    },
    /// the trainer printed no non-empty stdout line to parse
    #[error("trainer printed no metrics line; stdout tail: {stdout_tail}")]
    NoMetrics { stdout_tail: String },
    /// the last non-empty stdout line is not a [`TrainerMetrics`] json
    #[error("metrics line does not parse: {error}")]
    BadMetrics { error: String },
    /// the trainer exited 0 but its `--out` file cannot be read
    #[error("cannot read trained model at {path}: {error}")]
    ModelUnreadable { path: String, error: String },
    /// [`auto_model::Model::from_json`] rejected the trainer's output
    #[error("trainer emitted an invalid model: {error}")]
    ModelInvalid { error: String },
    /// filesystem failure staging the observation JSONL
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Run one distillation round through an external trainer process.
///
/// Protocol (spec/distillation.md): `observations` are written to a temp
/// JSONL file — one canonical `{"input":…,"output":…}` object per line
/// (sorted keys, exact numbers) — and the trainer is invoked as
///
/// ```text
/// <trainer_cmd…> --observations <path> --out <model_path>
///                --holdout <holdout> --seed <seed>
///                --min-holdout-accuracy <min_holdout_accuracy>
///                [--input-field <input_field>]   # when input_field is Some
/// ```
///
/// The trainer owns training AND measurement: its LAST non-empty stdout
/// line must be the [`TrainerMetrics`] json. Exit 0 accepts — the model at
/// `--out` is read and validated with [`auto_model::Model::from_json`]
/// ([`DistillError::ModelInvalid`] on rejection). Exit 3 is the honest
/// below-threshold rejection, reported with the trainer's parsed holdout
/// accuracy; exit 3 without a parseable metrics line is
/// [`DistillError::NoMetrics`]/[`DistillError::BadMetrics`] — the number is
/// never invented. Any other nonzero exit is [`DistillError::TrainerFailed`]
/// with the last 400 chars of stderr. Temp files (std temp dir, pid +
/// process-wide counter suffix) are removed best-effort on every path.
pub fn distill(
    observations: &[Observation],
    trainer_cmd: &[String],
    input_field: Option<&str>,
    holdout: f64,
    seed: u64,
    min_holdout_accuracy: f64,
) -> Result<Distilled, DistillError> {
    distill_validated(
        observations,
        trainer_cmd,
        input_field,
        holdout,
        seed,
        min_holdout_accuracy,
        &|json| {
            auto_model::Model::from_json(json)
                .map(|_| ())
                .map_err(|e| e.to_string())
        },
    )
}

/// [`distill`] with a caller-supplied model validator — the dispatch point
/// for model kinds (decision tree vs mlp). The validator receives the
/// trainer's model json and returns a rejection reason on failure; nothing
/// else in the protocol changes.
pub fn distill_validated(
    observations: &[Observation],
    trainer_cmd: &[String],
    input_field: Option<&str>,
    holdout: f64,
    seed: u64,
    min_holdout_accuracy: f64,
    validate: &dyn Fn(&str) -> Result<(), String>,
) -> Result<Distilled, DistillError> {
    let rows: Vec<(serde_json::Value, serde_json::Value, usize)> = observations
        .iter()
        .map(|o| (o.input.clone(), o.output.clone(), 1))
        .collect();
    distill_weighted_validated(
        &rows,
        trainer_cmd,
        input_field,
        holdout,
        seed,
        min_holdout_accuracy,
        validate,
    )
}

/// [`distill_validated`] over weighted rows (ADR-0031): each row is
/// (input, output, witness_weight); weight-1 rows serialize without a
/// weight field, so an all-ones call is byte-identical to the weightless
/// protocol. Weighting selects training data only.
pub fn distill_weighted_validated(
    rows: &[(serde_json::Value, serde_json::Value, usize)],
    trainer_cmd: &[String],
    input_field: Option<&str>,
    holdout: f64,
    seed: u64,
    min_holdout_accuracy: f64,
    validate: &dyn Fn(&str) -> Result<(), String>,
) -> Result<Distilled, DistillError> {
    let Some((program, leading)) = trainer_cmd.split_first() else {
        return Err(DistillError::TrainerSpawn {
            cmd: String::new(),
            error: "empty trainer command".into(),
        });
    };

    let exchange = Exchange::new();
    write_observations(rows, &exchange.observations)?;

    let mut args: Vec<String> = leading.to_vec();
    args.extend([
        "--observations".into(),
        exchange.observations.display().to_string(),
        "--out".into(),
        exchange.model.display().to_string(),
        "--holdout".into(),
        holdout.to_string(),
        "--seed".into(),
        seed.to_string(),
        "--min-holdout-accuracy".into(),
        min_holdout_accuracy.to_string(),
    ]);
    if let Some(field) = input_field {
        args.extend(["--input-field".into(), field.to_owned()]);
    }

    let output =
        Command::new(program)
            .args(&args)
            .output()
            .map_err(|e| DistillError::TrainerSpawn {
                cmd: render_cmd(program, &args),
                error: e.to_string(),
            })?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if output.status.code() == Some(3) {
        let line = metrics_line(&stdout)?;
        let metrics = parse_metrics(line)?;
        return Err(DistillError::BelowThreshold {
            holdout_accuracy: metrics.holdout_accuracy,
            metrics_line: line.to_owned(),
        });
    }
    if !output.status.success() {
        return Err(DistillError::TrainerFailed {
            status: output.status.to_string(),
            stderr_tail: tail(&stderr, 400),
        });
    }

    let line = metrics_line(&stdout)?;
    let metrics = parse_metrics(line)?;
    let model_json =
        std::fs::read_to_string(&exchange.model).map_err(|e| DistillError::ModelUnreadable {
            path: exchange.model.display().to_string(),
            error: e.to_string(),
        })?;
    validate(&model_json).map_err(|error| DistillError::ModelInvalid { error })?;
    Ok(Distilled {
        model_json,
        metrics,
    })
}

// ---- private orchestration internals ----

/// The two exchange files for one round; dropped = best-effort removal on
/// every exit path (a never-written model file simply fails to remove).
struct Exchange {
    observations: PathBuf,
    model: PathBuf,
}

impl Exchange {
    /// Distinct paths per call: std temp dir + pid + process-wide counter.
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir();
        Self {
            observations: dir.join(format!("auto-distill-{pid}-{id}.observations.jsonl")),
            model: dir.join(format!("auto-distill-{pid}-{id}.model.json")),
        }
    }
}

impl Drop for Exchange {
    fn drop(&mut self) {
        for path in [&self.observations, &self.model] {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// One canonical `{"input":…,"output":…}` line per observation (sorted keys
/// — serde_json's map is ordered, `preserve_order` off workspace-wide).
/// One canonical `{"input":…,"output":…}` line per row, plus `"weight"`
/// when it is not 1 (ADR-0031): weightless runs stay byte-identical,
/// absent = 1 on the trainer side.
fn write_observations(
    rows: &[(serde_json::Value, serde_json::Value, usize)],
    path: &PathBuf,
) -> std::io::Result<()> {
    let mut lines = String::new();
    for (input, output, weight) in rows {
        let line = if *weight == 1 {
            serde_json::json!({ "input": input, "output": output })
        } else {
            serde_json::json!({ "input": input, "output": output, "weight": weight })
        };
        lines.push_str(&serde_json::to_string(&line).expect("Value serialization cannot fail"));
        lines.push('\n');
    }
    std::fs::write(path, lines)
}

/// The LAST non-empty stdout line (trimmed), or [`DistillError::NoMetrics`].
fn metrics_line(stdout: &str) -> Result<&str, DistillError> {
    stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .ok_or_else(|| DistillError::NoMetrics {
            stdout_tail: tail(stdout, 400),
        })
}

fn parse_metrics(line: &str) -> Result<TrainerMetrics, DistillError> {
    serde_json::from_str(line).map_err(|e| DistillError::BadMetrics {
        error: e.to_string(),
    })
}

/// Last `n` chars (not bytes: tails must never split a UTF-8 boundary).
fn tail(text: &str, n: usize) -> String {
    let count = text.chars().count();
    if count <= n {
        text.to_owned()
    } else {
        text.chars().skip(count - n).collect()
    }
}

/// Full command line for spawn-failure reports.
fn render_cmd(program: &str, args: &[String]) -> String {
    let mut parts = vec![program.to_owned()];
    parts.extend(args.iter().cloned());
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::json;

    use super::*;

    /// Stub trainer: python, stdlib-only, mode selected by the
    /// `AUTO_STUB_MODE` env var. `ok`/`below` write a hand-written VALID
    /// frozen-format model (3 nodes) and print a metrics line (after a noise
    /// line, so last-line selection is exercised); `below` exits 3.
    /// `garbage-metrics` prints a non-json line; `no-model` prints metrics
    /// but writes nothing; `bad-model` writes junk to --out; `junk-exit-2`
    /// exits 2 with stderr only; `silent` (default) exits 0 with no stdout.
    /// The metrics line carries an unknown field (`note`) on purpose and
    /// echoes argv in `trainer`, so tests can assert the CLI protocol.
    const STUB_TRAINER: &str = r#"import json, os, sys
mode = os.environ.get("AUTO_STUB_MODE", "silent")
a = dict(zip(sys.argv[1::2], sys.argv[2::2]))
if mode == "junk-exit-2":
    sys.stderr.write("boom: stub trainer junk\n"); sys.exit(2)
if mode == "garbage-metrics":
    print("epoch 1: loss going down, trust me"); sys.exit(0)
if mode == "silent":
    sys.exit(0)
n = sum(1 for line in open(a["--observations"], encoding="utf-8")
        if set(json.loads(line)) == {"input", "output"})
if mode == "bad-model":
    open(a["--out"], "w").write("not a model")
elif mode != "no-model":
    model = {"model_version": 0,
             "features": {"kind": "char_trigram_fnv1a", "buckets": 16, "input_field": "text"},
             "nodes": [{"split": {"feature": 3, "threshold": 0.5, "left": 1, "right": 2}},
                       {"leaf": {"label": "miss"}}, {"leaf": {"label": "hit"}}]}
    open(a["--out"], "w").write(json.dumps(model))
print("stub training log noise")
print(json.dumps({"train_accuracy": 1.0, "holdout_accuracy": 0.75, "train_n": n,
                  "holdout_n": 1, "classes": ["hit", "miss"],
                  "trainer": " ".join(["stub-trainer"] + sys.argv[1:]),
                  "note": "unknown field, must be tolerated"}))
sys.exit(3 if mode == "below" else 0)
"#;

    /// Same probe as crates/auto-cli/tests/cli.rs.
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
        let dir =
            std::env::temp_dir().join(format!("auto-distill-test-{label}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create test temp dir");
        dir
    }

    /// Write the stub into `dir` and build a trainer command that sets the
    /// mode env var for the child only (tests run in parallel; mutating this
    /// process's environment would race).
    fn stub_cmd(python: &str, dir: &Path, mode: &str) -> Vec<String> {
        let stub = dir.join("stub_trainer.py");
        std::fs::write(&stub, STUB_TRAINER).expect("write stub trainer");
        let launcher = format!(
            "import os;os.environ['AUTO_STUB_MODE']={mode:?};\
             exec(open({stub:?}, encoding='utf-8').read())",
            stub = stub.to_str().expect("utf-8 path"),
        );
        vec![python.to_owned(), "-c".into(), launcher]
    }

    fn observations() -> Vec<Observation> {
        [
            ("send the invoice", "action"),
            ("what is our refund policy", "question"),
            ("thanks, all good", "chitchat"),
        ]
        .into_iter()
        .map(|(text, label)| Observation {
            input: json!({ "text": text }),
            output: json!(label),
        })
        .collect()
    }

    /// Run distill against the stub in `mode`; `None` = python absent
    /// (skipped loudly).
    fn run_stub(label: &str, mode: &str, min: f64) -> Option<Result<Distilled, DistillError>> {
        let Some(python) = find_python() else {
            eprintln!("SKIPPED {label}: no python interpreter found");
            return None;
        };
        let dir = temp_dir(label);
        let cmd = stub_cmd(&python, &dir, mode);
        let result = distill(&observations(), &cmd, Some("text"), 0.25, 7, min);
        let _ = std::fs::remove_dir_all(&dir);
        Some(result)
    }

    #[test]
    fn happy_path_returns_validated_model_and_trainer_metrics() {
        let Some(result) = run_stub("ok", "ok", 0.5) else {
            return;
        };
        let distilled = result.expect("ok mode distills");
        let model = auto_model::Model::from_json(&distilled.model_json)
            .expect("model_json is valid frozen-format");
        assert_eq!(model.nodes.len(), 3);
        assert_eq!(model.features.input_field.as_deref(), Some("text"));

        // every metric is the trainer's; the unknown `note` field parsed fine
        let m = &distilled.metrics;
        assert_eq!(m.train_accuracy, 1.0);
        assert_eq!(m.holdout_accuracy, 0.75);
        assert_eq!(m.train_n, 3, "stub counts the JSONL observation lines");
        assert_eq!(m.holdout_n, 1);
        assert_eq!(m.classes, vec!["hit", "miss"]);
        // the stub echoes its argv in `trainer`: the CLI protocol, verified
        assert!(m.trainer.starts_with("stub-trainer"), "{}", m.trainer);
        for expected in [
            "--observations",
            "--out",
            "--holdout 0.25",
            "--seed 7",
            "--min-holdout-accuracy 0.5",
            "--input-field text",
        ] {
            assert!(
                m.trainer.contains(expected),
                "missing {expected:?} in {:?}",
                m.trainer
            );
        }
    }

    #[test]
    fn exit_3_is_below_threshold_with_the_trainers_number() {
        let Some(result) = run_stub("below", "below", 0.9) else {
            return;
        };
        match result {
            Err(DistillError::BelowThreshold {
                holdout_accuracy,
                metrics_line,
            }) => {
                assert_eq!(holdout_accuracy, 0.75, "the trainer's number, parsed");
                assert!(
                    metrics_line.contains("\"holdout_accuracy\""),
                    "{metrics_line}"
                );
            }
            other => panic!("expected BelowThreshold, got {other:?}"),
        }
    }

    #[test]
    fn unparseable_metrics_line_is_bad_metrics() {
        let Some(result) = run_stub("garbage-metrics", "garbage-metrics", 0.5) else {
            return;
        };
        assert!(
            matches!(result, Err(DistillError::BadMetrics { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn empty_stdout_is_no_metrics() {
        let Some(result) = run_stub("silent", "silent", 0.5) else {
            return;
        };
        assert!(
            matches!(result, Err(DistillError::NoMetrics { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn missing_model_file_is_model_unreadable() {
        let Some(result) = run_stub("no-model", "no-model", 0.5) else {
            return;
        };
        match result {
            Err(DistillError::ModelUnreadable { path, .. }) => {
                assert!(path.ends_with(".model.json"), "{path}");
            }
            other => panic!("expected ModelUnreadable, got {other:?}"),
        }
    }

    #[test]
    fn model_failing_from_json_is_model_invalid() {
        let Some(result) = run_stub("bad-model", "bad-model", 0.5) else {
            return;
        };
        match result {
            Err(DistillError::ModelInvalid { error }) => {
                assert!(error.contains("invalid model json"), "{error}");
            }
            other => panic!("expected ModelInvalid, got {other:?}"),
        }
    }

    #[test]
    fn nonzero_exit_other_than_3_is_trainer_failed_with_stderr_tail() {
        let Some(result) = run_stub("junk-exit-2", "junk-exit-2", 0.5) else {
            return;
        };
        match result {
            Err(DistillError::TrainerFailed {
                status,
                stderr_tail,
            }) => {
                assert!(status.contains('2'), "{status}");
                assert!(
                    stderr_tail.contains("boom: stub trainer junk"),
                    "{stderr_tail}"
                );
            }
            other => panic!("expected TrainerFailed, got {other:?}"),
        }
    }

    #[test]
    fn missing_trainer_binary_is_trainer_spawn() {
        let cmd = vec!["auto-distill-no-such-trainer-xyz".to_owned()];
        match distill(&observations(), &cmd, None, 0.2, 1, 0.9) {
            Err(DistillError::TrainerSpawn { cmd, .. }) => {
                assert!(cmd.contains("auto-distill-no-such-trainer-xyz"), "{cmd}");
            }
            other => panic!("expected TrainerSpawn, got {other:?}"),
        }
    }

    #[test]
    fn empty_trainer_command_is_trainer_spawn() {
        assert!(matches!(
            distill(&observations(), &[], None, 0.2, 1, 0.9),
            Err(DistillError::TrainerSpawn { .. })
        ));
    }

    #[test]
    fn io_errors_convert_into_the_io_variant() {
        // the one variant not reachable through the stub without unportable
        // filesystem sabotage: pin the From conversion instead
        let err = DistillError::from(std::io::Error::other("disk full"));
        assert!(matches!(err, DistillError::Io(_)));
        assert_eq!(err.to_string(), "disk full");
    }
}
