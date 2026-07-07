//! The persistent watermark: a tiny strict-JSON file recording the
//! distinct-input count the daemon last recompiled at, so the ratchet's "no
//! new evidence, no recompile" decision survives a restart (wave-5 upgrade to
//! ADR-0013's in-memory watermark).
//!
//! `None` path = wave-4 in-memory behavior, unchanged. `Some(path)`:
//!
//! - read at startup: missing file is a **fresh start** (`Ok(None)`); a present
//!   file that is unreadable, not the strict schema, or an unknown
//!   `watermark_version` is a **loud** [`DaemonError::Watermark`], never a
//!   silent "fresh" — a wrong watermark would silently skip recompiles;
//! - written **atomically** after every publishing cycle: a sibling temp file
//!   is written and then renamed over the target. A crash mid-write leaves the
//!   temp file, never a torn `path`; a reader sees either the old contents or
//!   the complete new contents (POSIX `rename` / Windows `MoveFileEx
//!   REPLACE_EXISTING`, both atomic within one directory).

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::DaemonError;

/// The only watermark schema version. A file carrying any other value is a
/// loud error, not a silent fresh start.
const WATERMARK_VERSION: u32 = 0;

/// The on-disk watermark. `deny_unknown_fields` makes an extra key a corrupt
/// file rather than a silently-ignored one: a watermark we cannot fully
/// account for must not be trusted to gate recompiles.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WatermarkFile {
    watermark_version: u32,
    last_compiled_count: usize,
}

/// Read the persistent watermark at `path`.
///
/// - `None` (not configured): `Ok(None)` — wave-4 in-memory behavior.
/// - configured but missing: `Ok(None)` — a fresh start; the operator pointed
///   at a path no daemon has written yet.
/// - configured, present, valid: `Ok(Some(count))`.
/// - configured, present, unreadable / not the strict schema / unknown
///   version: [`DaemonError::Watermark`], loud.
pub(crate) fn read_watermark(path: Option<&Path>) -> Result<Option<usize>, DaemonError> {
    let Some(path) = path else {
        return Ok(None);
    };
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        // The one non-error absence: the file has never been written. Every
        // other read failure (permissions, non-UTF-8 bytes, a directory) is a
        // watermark we were told to trust but cannot — loud.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(DaemonError::Watermark {
                path: path.display().to_string(),
                detail: format!("cannot read: {e}"),
            });
        }
    };
    let parsed: WatermarkFile =
        serde_json::from_str(&text).map_err(|e| DaemonError::Watermark {
            path: path.display().to_string(),
            detail: format!("not the strict watermark JSON schema: {e}"),
        })?;
    if parsed.watermark_version != WATERMARK_VERSION {
        return Err(DaemonError::Watermark {
            path: path.display().to_string(),
            detail: format!(
                "unknown watermark_version {} (this daemon reads and writes version \
                 {WATERMARK_VERSION})",
                parsed.watermark_version
            ),
        });
    }
    Ok(Some(parsed.last_compiled_count))
}

/// Atomically write `count` as the watermark at `path` (a no-op when `path` is
/// `None`).
///
/// Writes a sibling temp file (`<path>.tmp-<pid>-<n>`, guaranteed in the same
/// directory so the rename stays intra-filesystem) then renames it over the
/// target. The rename is the commit point: it either fully replaces the old
/// file or fails leaving the old file intact, so `path` is never observed torn.
pub(crate) fn write_watermark(path: Option<&Path>, count: usize) -> Result<(), DaemonError> {
    let Some(path) = path else {
        return Ok(());
    };
    let file = WatermarkFile {
        watermark_version: WATERMARK_VERSION,
        last_compiled_count: count,
    };
    // Compact, deterministic bytes: `{"watermark_version":0,"last_compiled_count":N}`.
    let json = serde_json::to_string(&file).map_err(|e| DaemonError::Watermark {
        path: path.display().to_string(),
        detail: format!("cannot serialize: {e}"),
    })?;

    let tmp = temp_sibling(path);
    std::fs::write(&tmp, json.as_bytes()).map_err(|e| DaemonError::Watermark {
        path: tmp.display().to_string(),
        detail: format!("cannot write temp file: {e}"),
    })?;
    std::fs::rename(&tmp, path).map_err(|e| {
        // A failed rename must not leak the temp file. Best-effort cleanup; the
        // rename failure itself is the reported error.
        let _ = std::fs::remove_file(&tmp);
        DaemonError::Watermark {
            path: path.display().to_string(),
            detail: format!("cannot atomically rename temp file into place: {e}"),
        }
    })
}

/// A unique temp path in the SAME directory as `path`, formed by appending a
/// suffix to the full path (so the parent directory is preserved without any
/// `parent()` edge cases). The pid plus a monotonic counter keep concurrent
/// daemons and successive writes from colliding.
fn temp_sibling(path: &Path) -> std::path::PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut os = path.as_os_str().to_owned();
    os.push(format!(".tmp-{}-{n}", std::process::id()));
    std::path::PathBuf::from(os)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wm_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("watermark.json")
    }

    #[test]
    fn none_path_is_none_and_writes_nothing() {
        // The wave-4 in-memory contract: no path configured means no read, no
        // write, and no file created.
        assert_eq!(read_watermark(None).expect("none reads none"), None);
        write_watermark(None, 7).expect("none write is a no-op");
    }

    #[test]
    fn missing_file_is_a_fresh_start() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = wm_path(&dir);
        assert!(!path.exists());
        assert_eq!(
            read_watermark(Some(&path)).expect("missing file reads none"),
            None
        );
    }

    #[test]
    fn round_trip_through_a_fresh_process_object() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = wm_path(&dir);
        write_watermark(Some(&path), 42).expect("write");
        // A distinct read — the exact "new daemon reads the file" step.
        assert_eq!(read_watermark(Some(&path)).expect("read"), Some(42));
    }

    #[test]
    fn write_replaces_an_existing_file() {
        // The atomic-write happy path, exercised twice: the temp+rename must
        // replace an existing target (Windows MoveFileEx REPLACE_EXISTING /
        // POSIX rename), and no temp file may linger.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = wm_path(&dir);
        write_watermark(Some(&path), 1).expect("first write");
        write_watermark(Some(&path), 2).expect("overwrite");
        assert_eq!(read_watermark(Some(&path)).expect("read"), Some(2));
        // Exactly one file remains in the dir: the watermark, no `.tmp-*`.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(leftovers, vec!["watermark.json".to_owned()], "no temp leak");
    }

    #[test]
    fn bytes_are_the_documented_compact_schema() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = wm_path(&dir);
        write_watermark(Some(&path), 3).expect("write");
        let text = std::fs::read_to_string(&path).expect("read text");
        assert_eq!(text, r#"{"watermark_version":0,"last_compiled_count":3}"#);
    }

    #[test]
    fn corrupt_json_is_a_loud_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = wm_path(&dir);
        std::fs::write(&path, b"this is not json").expect("write junk");
        match read_watermark(Some(&path)) {
            Err(DaemonError::Watermark { path: p, .. }) => {
                assert_eq!(p, path.display().to_string());
            }
            other => panic!("expected Watermark error, got {other:?}"),
        }
    }

    #[test]
    fn unknown_version_is_a_loud_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = wm_path(&dir);
        std::fs::write(&path, br#"{"watermark_version":1,"last_compiled_count":5}"#)
            .expect("write");
        match read_watermark(Some(&path)) {
            Err(DaemonError::Watermark { detail, .. }) => {
                assert!(detail.contains("watermark_version"), "detail: {detail}");
            }
            other => panic!("expected Watermark error, got {other:?}"),
        }
    }

    #[test]
    fn unknown_field_is_a_loud_error() {
        // deny_unknown_fields: a watermark we cannot fully account for is
        // corrupt, not "best-effort readable".
        let dir = tempfile::tempdir().expect("tempdir");
        let path = wm_path(&dir);
        std::fs::write(
            &path,
            br#"{"watermark_version":0,"last_compiled_count":5,"extra":true}"#,
        )
        .expect("write");
        match read_watermark(Some(&path)) {
            Err(DaemonError::Watermark { .. }) => {}
            other => panic!("expected Watermark error, got {other:?}"),
        }
    }

    #[test]
    fn wrong_type_is_a_loud_error() {
        // A negative / non-integer count cannot be a usize: corrupt, not fresh.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = wm_path(&dir);
        std::fs::write(
            &path,
            br#"{"watermark_version":0,"last_compiled_count":-1}"#,
        )
        .expect("write");
        match read_watermark(Some(&path)) {
            Err(DaemonError::Watermark { .. }) => {}
            other => panic!("expected Watermark error, got {other:?}"),
        }
    }
}
