//! One watch-and-maybe-recompile cycle: count distinct inputs, decide, and —
//! when the evidence has grown — run the operator's recompile command and
//! publish whatever artifact it emitted to the registry.
//!
//! The recompile is the **real** `auto` pipeline invoked as a subprocess
//! (spec/adr/0013): the daemon never reimplements the emit gate, so an
//! artifact reaches the registry only because the gate that ran inside the
//! subprocess passed. A nonzero exit fails the cycle loudly and publishes
//! nothing.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use auto_contract::Contract;
use auto_registry::Registry;

use crate::watch::{distinct_input_count, should_recompile};
use crate::{DaemonConfig, DaemonError};

/// The placeholder the operator puts in `recompile_argv` where the daemon
/// wants the emitted artifact written. Substituted per cycle with a fresh
/// temp path; its absence is a configuration error (the daemon would have no
/// way to find the artifact).
const OUT_PLACEHOLDER: &str = "{out}";

/// Longest stderr tail carried in a [`DaemonError::Recompile`] — enough to
/// diagnose a failed recompile without copying an unbounded child log.
const STDERR_TAIL_CHARS: usize = 400;

/// What one cycle observed and did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CycleOutcome {
    /// distinct-input count measured this cycle
    pub count: usize,
    /// the published artifact id, or `None` when the cycle was a no-op (the
    /// count did not grow past the watermark)
    pub published: Option<String>,
}

/// Run exactly one cycle against `contract` (already loaded from
/// `config.contract`). `last_compiled` is the watermark from the previous
/// recompile in this process, or `None` for a fresh daemon.
///
/// Success with `published: Some(id)` means a recompile ran and its artifact
/// was published; `published: None` means the count had not grown and nothing
/// ran. Every failure path is loud: a nonzero recompile exit
/// ([`DaemonError::Recompile`]), a recompile that wrote no artifact
/// ([`DaemonError::NoArtifact`]), or a registry publish failure
/// ([`DaemonError::Publish`]).
pub fn run_cycle(
    config: &DaemonConfig,
    contract: &Contract,
    last_compiled: Option<usize>,
) -> Result<CycleOutcome, DaemonError> {
    let count = distinct_input_count(&config.store, contract)?;
    if !should_recompile(last_compiled, count) {
        eprintln!(
            "auto-daemon: {count} distinct input(s), watermark {}; no new evidence, no recompile",
            watermark_label(last_compiled),
        );
        return Ok(CycleOutcome {
            count,
            published: None,
        });
    }

    // A fresh path per cycle: the recompile writes here, we read it back, and
    // the registry becomes the durable home (the temp file is then removed).
    let out = fresh_out_path();
    let argv = substitute_out(&config.recompile_argv, &out)?;
    run_recompile(&argv)?;
    if !out.exists() {
        return Err(DaemonError::NoArtifact {
            out: out.display().to_string(),
        });
    }
    let id = publish(&config.registry_root, &out)?;
    // The bytes now live in the content-addressed registry; the temp file is
    // redundant. Best-effort: a failed remove is not a cycle failure.
    let _ = std::fs::remove_file(&out);
    eprintln!("auto-daemon: recompiled at {count} distinct input(s); published artifact {id}");
    Ok(CycleOutcome {
        count,
        published: Some(id),
    })
}

/// A fresh per-cycle artifact output path under the system temp dir. The pid
/// plus a monotonic counter keep concurrent daemons and successive cycles
/// from colliding on the same file.
fn fresh_out_path() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("auto-daemon-{}-{n}.cbin", std::process::id()))
}

/// Replace every `{out}` in the recompile argv with `out`. The placeholder
/// MUST appear at least once — without it the daemon cannot tell the command
/// where to write the artifact, so a missing placeholder is refused **before
/// anything is spawned**. This is a config fault ([`DaemonError::Config`]),
/// not a [`DaemonError::Recompile`]: the argv is fixed for the daemon's
/// lifetime, so the refusal recurs identically and supervised mode must never
/// retry it (wave-5 split; it was a `Recompile { status: "not started" }` in
/// wave 4, ADR-0013 amendment).
fn substitute_out(argv: &[String], out: &Path) -> Result<Vec<String>, DaemonError> {
    let out = out.to_string_lossy();
    let mut found = false;
    let mut substituted = Vec::with_capacity(argv.len());
    for arg in argv {
        if arg.contains(OUT_PLACEHOLDER) {
            found = true;
            substituted.push(arg.replace(OUT_PLACEHOLDER, &out));
        } else {
            substituted.push(arg.clone());
        }
    }
    if !found {
        return Err(DaemonError::Config {
            detail: format!(
                "recompile_argv has no `{OUT_PLACEHOLDER}` placeholder; the daemon cannot tell \
                 the recompile command where to write the artifact"
            ),
        });
    }
    Ok(substituted)
}

/// Run the recompile subprocess. `argv[0]` is the program; the rest are its
/// arguments (no shell — arguments are passed verbatim, so a substituted path
/// with spaces or backslashes needs no quoting). A spawn failure or a nonzero
/// exit is [`DaemonError::Recompile`], carrying the exit status and a bounded
/// stderr tail.
fn run_recompile(argv: &[String]) -> Result<(), DaemonError> {
    // substitute_out returns Ok only when some arg held the placeholder, so
    // argv is non-empty here.
    let (program, args) = argv
        .split_first()
        .expect("substitute_out guarantees a non-empty argv");
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| DaemonError::Recompile {
            status: "spawn failed".to_owned(),
            stderr_tail: e.to_string(),
        })?;
    if !output.status.success() {
        return Err(DaemonError::Recompile {
            status: output.status.to_string(),
            stderr_tail: stderr_tail(&output.stderr, STDERR_TAIL_CHARS),
        });
    }
    Ok(())
}

/// Publish `artifact` to the registry at `registry_root`, returning the
/// content id. Unsigned: the daemon holds no signing key in v0 (signing is an
/// operator step — `registry keygen` then a signed republish; ADR-0013). A
/// redundant recompile emits byte-identical bytes, which `Registry::add`
/// dedupes.
fn publish(registry_root: &Path, artifact: &Path) -> Result<String, DaemonError> {
    let registry = Registry::open(registry_root).map_err(|e| DaemonError::Publish {
        root: registry_root.display().to_string(),
        detail: e.to_string(),
    })?;
    let outcome = registry
        .add(artifact, false)
        .map_err(|e| DaemonError::Publish {
            root: registry_root.display().to_string(),
            detail: e.to_string(),
        })?;
    Ok(outcome.id)
}

/// Last `max` characters of a child's stderr (lossy UTF-8, trailing
/// whitespace trimmed).
fn stderr_tail(stderr: &[u8], max: usize) -> String {
    let text = String::from_utf8_lossy(stderr);
    let trimmed = text.trim_end();
    let n = trimmed.chars().count();
    if n <= max {
        return trimmed.to_owned();
    }
    trimmed.chars().skip(n - max).collect()
}

fn watermark_label(last_compiled: Option<usize>) -> String {
    match last_compiled {
        Some(n) => n.to_string(),
        None => "none".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitute_out_replaces_every_placeholder() {
        let argv = vec![
            "python".to_owned(),
            "compile.py".to_owned(),
            "--out".to_owned(),
            OUT_PLACEHOLDER.to_owned(),
            format!("--backup={OUT_PLACEHOLDER}"),
        ];
        let out = Path::new("/tmp/artifact.cbin");
        let got = substitute_out(&argv, out).expect("substitution");
        assert_eq!(
            got,
            vec![
                "python".to_owned(),
                "compile.py".to_owned(),
                "--out".to_owned(),
                "/tmp/artifact.cbin".to_owned(),
                "--backup=/tmp/artifact.cbin".to_owned(),
            ]
        );
    }

    #[test]
    fn substitute_out_refuses_argv_without_placeholder() {
        let argv = vec!["python".to_owned(), "compile.py".to_owned()];
        match substitute_out(&argv, Path::new("/tmp/x.cbin")) {
            // A config fault (wave-5 split), never a Recompile: it recurs
            // identically and must not be retried under supervision.
            Err(DaemonError::Config { detail }) => {
                assert!(detail.contains("{out}"), "detail: {detail}");
            }
            other => panic!("expected a Config refusal, got {other:?}"),
        }
    }

    #[test]
    fn stderr_tail_returns_short_input_whole() {
        assert_eq!(stderr_tail(b"boom\n", STDERR_TAIL_CHARS), "boom");
        assert_eq!(stderr_tail(b"", STDERR_TAIL_CHARS), "");
    }

    #[test]
    fn stderr_tail_keeps_the_last_chars() {
        let long: String = std::iter::repeat_n('x', 500).collect();
        let tail = stderr_tail(long.as_bytes(), 400);
        assert_eq!(tail.chars().count(), 400);
        assert!(tail.chars().all(|c| c == 'x'));
    }
}
