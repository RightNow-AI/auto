//! The ratchet as a service — nothing figured out twice, with no human in
//! the loop.
//!
//! `auto run` deopts ingest new observations into the trace store; until
//! now an operator closed the loop by re-running `compile`/`distill` by
//! hand (spec/runtime.md §4). The daemon watches the store's distinct-input
//! count for one contract scope and, when the evidence grows, runs the
//! operator-configured recompile command (the REAL `auto` pipeline as a
//! subprocess — the gate stays exactly the gate) and publishes the emitted
//! artifact to the registry. Recompilation that fails the gate fails the
//! cycle loudly and publishes nothing.
//!
//! This file holds the frozen seam (config + error types). The watch loop,
//! the pure recompile decision, and the publish step are built in sibling
//! modules.

mod cycle;
mod run;
mod watch;
mod watermark;

#[cfg(test)]
mod e2e;

pub use cycle::{CycleOutcome, run_cycle};
pub use run::daemon;
pub use watch::{distinct_input_count, should_recompile};

use std::path::PathBuf;

/// One watched scope: a store, its contract, the recompile command, and the
/// registry the result publishes to.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// trace store to watch (the deopt ingestion target)
    pub store: PathBuf,
    /// contract whose scope defines the watched distinct-input count
    pub contract: PathBuf,
    /// registry the recompiled artifact is published to
    pub registry_root: PathBuf,
    /// recompile command argv; the placeholder `{out}` is replaced with the
    /// artifact output path the daemon expects the command to write
    pub recompile_argv: Vec<String>,
    /// poll interval between store checks, milliseconds
    pub poll_interval_ms: u64,
    /// run exactly one check-and-maybe-recompile cycle, then return —
    /// deterministic for tests and scriptable for e2e legs
    pub once: bool,
    /// persistent watermark file, or `None` for the wave-4 **in-memory**
    /// watermark (a restart re-observes `None` and recompiles once
    /// redundantly, which content-addressing dedupes — ADR-0013). `Some(path)`
    /// makes the last-compiled distinct-input count survive a restart: it is
    /// read at startup (missing = a fresh start; corrupt = a loud
    /// [`DaemonError::Watermark`], never a silent skip) and rewritten
    /// atomically after every publishing cycle (wave-5, ADR-0013 amendment).
    pub watermark_path: Option<PathBuf>,
    /// supervised mode. `false` is the wave-4 default: any cycle error stops
    /// the daemon loudly. `true`: a **retryable** cycle error
    /// ([`DaemonError::is_retryable`]) is logged and retried after an
    /// exponential backoff instead of exiting; a config-shaped error
    /// ([`DaemonError::Contract`], [`DaemonError::Config`],
    /// [`DaemonError::Watermark`]) still exits loudly even supervised, because
    /// retrying it is a tight useless loop (wave-5, ADR-0013 amendment).
    pub supervise: bool,
}

/// Every honest way a daemon cycle fails.
///
/// Wave 5 splits the failure kinds into two classes that supervised mode
/// (`DaemonConfig::supervise`) treats differently ([`DaemonError::is_retryable`]):
/// external-world I/O faults, which a later cycle may clear, versus
/// config-shaped faults, which recur identically on every retry.
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("cannot read store {store}: {detail}")]
    Store { store: String, detail: String },
    #[error("cannot load contract {contract}: {detail}")]
    Contract { contract: String, detail: String },
    /// The recompile argv is misconfigured (e.g. no `{out}` placeholder). A
    /// config fault, distinct from [`DaemonError::Recompile`]: it is detected
    /// **before** anything is spawned and recurs identically every cycle, so
    /// supervised mode must not retry it (wave 5; was a `Recompile { status:
    /// "not started" }` in wave 4).
    #[error("recompile command misconfigured: {detail}")]
    Config { detail: String },
    #[error("recompile command failed ({status}); stderr tail: {stderr_tail}")]
    Recompile { status: String, stderr_tail: String },
    #[error("recompile succeeded but wrote no artifact at {out}")]
    NoArtifact { out: String },
    #[error("cannot publish to registry {root}: {detail}")]
    Publish { root: String, detail: String },
    /// The persistent watermark file exists but cannot be trusted (unreadable,
    /// not the strict schema, or an unknown `watermark_version`). Loud on
    /// purpose: a wrong watermark would silently skip recompiles, so a file we
    /// cannot trust stops the daemon rather than being read as "fresh" (wave 5,
    /// ADR-0013 amendment).
    #[error("watermark file {path} is corrupt: {detail}")]
    Watermark { path: String, detail: String },
}

impl DaemonError {
    /// Whether supervised mode ([`DaemonConfig::supervise`]) should back off
    /// and retry this error rather than exit.
    ///
    /// **Retryable** (external-world I/O; a later cycle may clear it): [`Store`]
    /// (the store file/contents can change), [`Recompile`] (a real spawn or
    /// nonzero exit — the subprocess may behave differently next time),
    /// [`NoArtifact`] (the command may write the artifact next time),
    /// [`Publish`] (a transient registry/filesystem fault).
    ///
    /// **Not retryable** (config-shaped; recurs identically regardless of the
    /// world, so retrying is a tight useless loop): [`Contract`] (the file will
    /// not parse or is an unwatchable scope), [`Config`] (the recompile argv is
    /// misconfigured), [`Watermark`] (the state file is corrupt). These exit
    /// loudly even under supervision.
    ///
    /// [`Store`]: DaemonError::Store
    /// [`Recompile`]: DaemonError::Recompile
    /// [`NoArtifact`]: DaemonError::NoArtifact
    /// [`Publish`]: DaemonError::Publish
    /// [`Contract`]: DaemonError::Contract
    /// [`Config`]: DaemonError::Config
    /// [`Watermark`]: DaemonError::Watermark
    pub fn is_retryable(&self) -> bool {
        match self {
            DaemonError::Store { .. }
            | DaemonError::Recompile { .. }
            | DaemonError::NoArtifact { .. }
            | DaemonError::Publish { .. } => true,
            DaemonError::Contract { .. }
            | DaemonError::Config { .. }
            | DaemonError::Watermark { .. } => false,
        }
    }
}
