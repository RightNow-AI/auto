//! Append-only spend ledger.
//!
//! One JSONL line per paid call. The ledger is the single source of truth for
//! how much a session has spent; a call that succeeds against the paid API but
//! whose ledger line cannot be written is treated as fatal (the caller does not
//! get the response), because an unrecorded paid call is exactly the state the
//! CLAUDE.md guardrail forbids. Likewise a present-but-unparseable ledger is
//! fatal on read: a wrong running total is how a cap gets silently blown.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::FrontierError;

/// Environment variable read by [`SpendLedger::from_env_or_default`].
pub const LEDGER_PATH_ENV: &str = "AUTO_SPEND_LEDGER";

/// One append-only ledger line: exactly one paid call.
///
/// Field order and names are the on-disk schema — do not reorder or rename.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerEntry {
    /// Wall-clock time the line was written, unix epoch milliseconds.
    pub ts_unix_ms: u64,
    /// Session scope this spend counts against.
    pub session: String,
    /// Model that served the call (as the provider reported it).
    pub model: String,
    /// Free-text purpose tag (e.g. `cegis`, `tier0`).
    pub purpose: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Measured cost in micro-USD, from the pinned price table.
    pub cost_usd_micros: u64,
    /// sha256 hex of the canonical request JSON (provenance / dedupe key).
    pub request_digest: String,
}

/// Current wall-clock time as unix epoch milliseconds (0 if the clock is before
/// the epoch, which cannot happen on a sane host).
pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A spend ledger backed by a JSONL file at `path`.
///
/// The file and its parent directory are created lazily on first append, so an
/// as-yet-unused ledger reads as zero spend rather than an error.
#[derive(Debug, Clone)]
pub struct SpendLedger {
    path: PathBuf,
}

impl SpendLedger {
    /// A ledger backed by an explicit path.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Convenience constructor: use `$AUTO_SPEND_LEDGER` if set and non-empty,
    /// otherwise `~/.auto/spend.jsonl`. Reads the environment, so it is not used
    /// by tests (which construct [`SpendLedger::new`] with a temp path instead).
    pub fn from_env_or_default() -> Result<Self, FrontierError> {
        let path = resolve_ledger_path(std::env::var(LEDGER_PATH_ENV).ok())?;
        Ok(Self::new(path))
    }

    /// The backing file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one line, creating the parent directory and file if missing. Any
    /// I/O or serialization failure is a fatal [`FrontierError::Ledger`].
    pub fn append(&self, entry: &LedgerEntry) -> Result<(), FrontierError> {
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|e| io_err("create ledger dir", parent, &e))?;
        }
        let mut line = serde_json::to_string(entry).map_err(|e| FrontierError::Ledger {
            detail: format!("serialize ledger entry: {e}"),
        })?;
        line.push('\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| io_err("open ledger", &self.path, &e))?;
        file.write_all(line.as_bytes())
            .map_err(|e| io_err("append ledger", &self.path, &e))?;
        file.flush()
            .map_err(|e| io_err("flush ledger", &self.path, &e))?;
        Ok(())
    }

    /// Parse every line. A missing file is zero spend (`Ok([])`). A present but
    /// unparseable line is fatal — the totals can no longer be trusted.
    pub fn read_all(&self) -> Result<Vec<LedgerEntry>, FrontierError> {
        let text = match fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_err("read ledger", &self.path, &e)),
        };
        let mut out = Vec::new();
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let entry: LedgerEntry =
                serde_json::from_str(line).map_err(|e| FrontierError::Ledger {
                    detail: format!(
                        "corrupt ledger line {} in {}: {e}",
                        i + 1,
                        self.path.display()
                    ),
                })?;
            out.push(entry);
        }
        Ok(out)
    }

    /// Total µ$ spent across the WHOLE ledger (every session).
    pub fn total_usd_micros(&self) -> Result<u64, FrontierError> {
        Ok(self
            .read_all()?
            .iter()
            .fold(0u64, |acc, e| acc.saturating_add(e.cost_usd_micros)))
    }

    /// Total µ$ spent by a single `session`.
    pub fn session_total_usd_micros(&self, session: &str) -> Result<u64, FrontierError> {
        Ok(self
            .read_all()?
            .iter()
            .filter(|e| e.session == session)
            .fold(0u64, |acc, e| acc.saturating_add(e.cost_usd_micros)))
    }
}

fn io_err(op: &str, path: &Path, e: &std::io::Error) -> FrontierError {
    FrontierError::Ledger {
        detail: format!("{op} {}: {e}", path.display()),
    }
}

/// Resolve the default ledger path from an (already read) env value.
/// Pure — takes the env value as an argument so it is testable without touching
/// the process environment.
fn resolve_ledger_path(env_value: Option<String>) -> Result<PathBuf, FrontierError> {
    if let Some(v) = env_value
        && !v.is_empty()
    {
        return Ok(PathBuf::from(v));
    }
    // home_dir() is un-deprecated and Windows-correct since Rust 1.85; the
    // workspace MSRV is 1.96, so this is the supported cross-platform path.
    let home = std::env::home_dir().ok_or_else(|| FrontierError::Ledger {
        detail: format!(
            "cannot resolve home directory for default spend ledger (~/.auto/spend.jsonl); set ${LEDGER_PATH_ENV}"
        ),
    })?;
    Ok(home.join(".auto").join("spend.jsonl"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(session: &str, cost: u64) -> LedgerEntry {
        LedgerEntry {
            ts_unix_ms: 1_720_000_000_000,
            session: session.to_owned(),
            model: "claude-haiku-4-5".to_owned(),
            purpose: "test".to_owned(),
            input_tokens: 10,
            output_tokens: 20,
            cost_usd_micros: cost,
            request_digest: "deadbeef".to_owned(),
        }
    }

    #[test]
    fn append_then_read_round_trips_and_totals_accumulate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = SpendLedger::new(dir.path().join("spend.jsonl"));

        ledger.append(&entry("s1", 100)).expect("append 1");
        ledger.append(&entry("s1", 250)).expect("append 2");
        ledger.append(&entry("s2", 40)).expect("append 3");

        let all = ledger.read_all().expect("read");
        assert_eq!(all.len(), 3);
        assert_eq!(all[0], entry("s1", 100));

        assert_eq!(ledger.total_usd_micros().expect("total"), 390);
        assert_eq!(
            ledger.session_total_usd_micros("s1").expect("s1 total"),
            350
        );
        assert_eq!(ledger.session_total_usd_micros("s2").expect("s2 total"), 40);
        assert_eq!(
            ledger.session_total_usd_micros("absent").expect("absent"),
            0
        );
    }

    #[test]
    fn missing_ledger_reads_as_zero_spend() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = SpendLedger::new(dir.path().join("never-written.jsonl"));
        assert!(ledger.read_all().expect("read").is_empty());
        assert_eq!(ledger.total_usd_micros().expect("total"), 0);
        assert_eq!(ledger.session_total_usd_micros("s1").expect("s1"), 0);
    }

    #[test]
    fn corrupt_line_is_fatal_on_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("spend.jsonl");
        let ledger = SpendLedger::new(&path);
        ledger.append(&entry("s1", 100)).expect("append good line");
        // Append a garbage line directly (a test fixture writing a corrupt file).
        fs::write(
            &path,
            format!(
                "{}\nthis is not json\n",
                serde_json::to_string(&entry("s1", 100)).unwrap()
            ),
        )
        .expect("write corrupt file");

        let err = ledger.read_all().expect_err("corrupt line must be fatal");
        assert!(matches!(err, FrontierError::Ledger { .. }));
        // The totals must also refuse (fail-closed), not silently under-count.
        assert!(ledger.total_usd_micros().is_err());
        assert!(ledger.session_total_usd_micros("s1").is_err());
    }

    #[test]
    fn append_failure_surfaces_as_ledger_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Make the *parent* a regular file: creating the ledger dir must fail.
        let blocker = dir.path().join("iam-a-file");
        fs::write(&blocker, b"x").expect("write blocker file");
        let ledger = SpendLedger::new(blocker.join("spend.jsonl"));

        let err = ledger
            .append(&entry("s1", 100))
            .expect_err("append must fail");
        assert!(matches!(err, FrontierError::Ledger { .. }));
    }

    #[test]
    fn resolve_path_prefers_env_then_falls_back_to_home() {
        // Explicit env value wins.
        let p = resolve_ledger_path(Some("/custom/spend.jsonl".to_owned())).expect("env path");
        assert_eq!(p, PathBuf::from("/custom/spend.jsonl"));
        // Empty env value is ignored -> default under home/.auto/spend.jsonl.
        let d = resolve_ledger_path(Some(String::new())).expect("default path");
        assert!(d.ends_with(PathBuf::from(".auto").join("spend.jsonl")));
        let d2 = resolve_ledger_path(None).expect("default path");
        assert!(d2.ends_with(PathBuf::from(".auto").join("spend.jsonl")));
    }
}
