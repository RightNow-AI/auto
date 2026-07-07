//! Content-addressed eval-run records — the citable evidence unit.
//!
//! An eval run is the durable record of one verification: which contract,
//! which subject, what was checked, what the verdict was. Its id is the
//! sha-256 of the record's canonical JSON body, so a manifest citing an
//! eval-run id (S3+) pins the exact evidence, tamper-evidently. Records are
//! plain JSON files in a runs directory: inspectable, diffable, no database.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use auto_trace::model::{canonical_json, digest_hex};
use serde::{Deserialize, Serialize};

use crate::harness::{CheckStatus, VerificationReport};

/// Record format version; bump with an ADR.
pub const EVAL_RUN_VERSION: u32 = 0;

/// sha-256 hex of the record's canonical JSON body.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EvalRunId(pub String);

impl fmt::Display for EvalRunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckRecord {
    pub what: String,
    /// "pass" | "fail" | "unchecked"
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The serialized eval-run record. Field set is the format: additions bump
/// [`EVAL_RUN_VERSION`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalRun {
    pub eval_run_version: u32,
    pub contract_id: String,
    pub task: String,
    pub subject: String,
    /// "PASS" | "INCONCLUSIVE" | "FAIL"
    pub verdict: String,
    pub observations: usize,
    pub checks: Vec<CheckRecord>,
    /// supplied by the caller (the library reads no clocks)
    pub created_unix_ms: u64,
}

impl EvalRun {
    pub fn from_report(report: &VerificationReport, created_unix_ms: u64) -> Self {
        Self {
            eval_run_version: EVAL_RUN_VERSION,
            contract_id: report.contract_id.clone(),
            task: report.task.clone(),
            subject: report.subject.clone(),
            verdict: report.verdict.to_string(),
            observations: report.observations,
            checks: report
                .checks
                .iter()
                .map(|c| CheckRecord {
                    what: c.what.clone(),
                    status: match c.status {
                        CheckStatus::Passed => "pass".to_owned(),
                        CheckStatus::Failed => "fail".to_owned(),
                        CheckStatus::Unchecked => "unchecked".to_owned(),
                    },
                    detail: c.detail.clone(),
                })
                .collect(),
            created_unix_ms,
        }
    }

    /// Canonical body (sorted keys, compact) — what the id digests.
    pub fn canonical_body(&self) -> String {
        let value = serde_json::to_value(self).expect("record serialization cannot fail");
        canonical_json(&value)
    }

    pub fn id(&self) -> EvalRunId {
        EvalRunId(digest_hex(&self.canonical_body()))
    }
}

/// Write a record to `dir/<id>.json` (creating `dir` if needed). Returns the
/// path and the id. The file content is exactly the canonical body plus a
/// trailing newline, so `sha256(file minus newline) == id`.
pub fn write_eval_run(
    report: &VerificationReport,
    created_unix_ms: u64,
    dir: &Path,
) -> std::io::Result<(PathBuf, EvalRunId)> {
    let run = EvalRun::from_report(report, created_unix_ms);
    let id = run.id();
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{id}.json"));
    std::fs::write(&path, run.canonical_body() + "\n")?;
    Ok((path, id))
}

/// What one [`gc`] sweep did. Every eval-run file GC considered lands in
/// exactly one bucket, so `removed + kept + protected_kept` is the count of
/// candidate run files it saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GcReport {
    /// eval-run files deleted
    pub removed: usize,
    /// eval-run files retained by the keep-newest window — or, when an age
    /// bound is supplied to [`gc_with_age`], by being no older than the cutoff
    /// (age-kept). With no age bound this is exactly the window count, so the
    /// wave-8 meaning is unchanged.
    pub kept: usize,
    /// eval-run files retained *only* because they are protected
    /// (manifest-pinned): beyond the keep-newest window and — when an age bound
    /// is given — old enough that age would not have kept them either, so a pin
    /// is the sole reason they survived.
    pub protected_kept: usize,
}

/// What a size-bounded sweep ([`gc_with_limits`]) did (ADR-0020 size
/// amendment): the base [`GcReport`] plus the measured retained footprint.
/// Carrying `kept_bytes` and the requested ceiling lets a caller see whether
/// the ceiling was actually MET — floor and pinned records are never
/// size-evicted, so a pin/floor-heavy directory can stay over budget, and
/// honesty about that beats a falsely "met" ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GcSizeReport {
    pub report: GcReport,
    /// total bytes of the run files retained after the sweep
    pub kept_bytes: u64,
    /// the ceiling the caller requested, if any
    pub max_total_bytes: Option<u64>,
}

impl GcSizeReport {
    /// A ceiling was requested but the retained total still exceeds it —
    /// because only never-evicted records (floor, pinned) remain. The sweep
    /// removed everything it was allowed to; the operator must see the budget
    /// was not met rather than a false success.
    pub fn over_ceiling(&self) -> bool {
        self.max_total_bytes.is_some_and(|b| self.kept_bytes > b)
    }
}

/// Whether `stem` is a well-formed eval-run id: 64 lowercase-hex chars, the
/// shape [`digest_hex`] emits. GC deletes only files it could have written,
/// so anything else — other extensions, uppercase, wrong length, foreign
/// names — is never a candidate.
fn is_run_id(stem: &str) -> bool {
    stem.len() == 64 && stem.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Prune eval-run records under `runs_dir` by the keep-newest + pinned rule
/// (ADR-0020), with no age bound. Equivalent to
/// [`gc_with_age`]`(runs_dir, keep_newest, protected, None)`; kept as the
/// stable no-age entry point (the CLI's default, and every pre-age caller).
pub fn gc(
    runs_dir: &Path,
    keep_newest: usize,
    protected: &BTreeSet<String>,
) -> Result<GcReport, String> {
    gc_with_age(runs_dir, keep_newest, protected, None)
}

/// Prune eval-run records under `runs_dir` by the keep-newest + pinned +
/// age rule (ADR-0020, age amended), with no size ceiling. Delegates to
/// [`gc_with_limits`]`(.., None)` and returns its base [`GcReport`]; kept as
/// the stable age entry point (every pre-size caller and test).
pub fn gc_with_age(
    runs_dir: &Path,
    keep_newest: usize,
    protected: &BTreeSet<String>,
    older_than: Option<SystemTime>,
) -> Result<GcReport, String> {
    gc_with_limits(runs_dir, keep_newest, protected, older_than, None).map(|r| r.report)
}

/// Prune eval-run records under `runs_dir`, composing four retention rules:
/// the keep-newest window, manifest-pinned protection, an optional age bound,
/// and an optional total-size ceiling.
///
/// Retention rule (ADR-0020, age + size amended):
/// - Only `<id>.json` files whose stem is a 64-hex run id are candidates.
///   Any other file (or a directory that happens to match the name) is left
///   untouched and counted in no bucket — GC never deletes what it did not
///   write.
/// - Candidates are ordered newest-first by modified time. `keep_newest` is a
///   floor expressed as an mtime cutoff: every file at least as new as the
///   `keep_newest`-th newest is kept. When mtimes tie at that boundary all
///   tied files are kept, so the actual kept-by-window count can exceed
///   `keep_newest` — **ties keep more, never less**, and the outcome does not
///   depend on the order equal-mtime files are visited (a destructive pass
///   must never delete something that might be newer than a file it kept).
/// - **Age restricts deletion; it never extends it.** With `older_than =
///   Some(cutoff)` a record past the keep-newest floor is *eligible* for
///   deletion only if it is *strictly* older than `cutoff` (`modified <
///   cutoff`); a record at or newer than `cutoff` is kept even though it is
///   past the floor (ties at the cutoff keep, matching the window rule).
/// - Every id in `protected` is kept regardless of window, age, or size — even
///   far older than any allows. The protected set is the manifest-pinned runs
///   (`auto-registry`'s `Registry::pinned_eval_runs`).
/// - **Size ceiling.** After the floor, age, and pins decide the *eligible* set
///   (non-floor, non-pinned, and — when an age bound is given — strictly older
///   than the cutoff), removal is driven by `max_total_bytes`:
///   - `None`: every eligible record is removed. This reproduces
///     [`gc_with_age`] exactly (and, with `older_than = None`, wave-8 `gc`).
///   - `Some(ceiling)`: if the total size of the records that would be kept
///     exceeds `ceiling`, eligible records are removed **oldest-first**, in
///     whole mtime-tie groups (a coarse mtime tick is never partially
///     collected — ADR-0020 decision 5), until the retained total is within
///     `ceiling` or no eligible record remains. Everything retained is thus
///     strictly newer than everything the ceiling evicted. Floor and pinned
///     records are **never** size-evicted, so a directory whose floor+pins
///     alone exceed `ceiling` stays over budget — [`GcSizeReport::over_ceiling`]
///     reports that honestly rather than claim a met ceiling.
///
/// Composition is monotone in the safe direction: turning on age, or a ceiling,
/// or raising either, can only KEEP more — never delete a record the
/// floor+pins rule alone would have kept. `--keep 0 --max-total-bytes B` is the
/// pure size policy (keep newest under B, evicting oldest, pins always kept);
/// `--max-age-days D` with no ceiling is the pure age policy.
///
/// Buckets: an age-kept or size-spared record (retained, not by a pin) is
/// counted in `kept`. `protected_kept` stays "saved *only* by a pin": past the
/// floor, age-eligible, and not size-spared for any other reason. The partition
/// `removed + kept + protected_kept == candidate count` holds under any
/// `older_than` / `max_total_bytes`. `kept_bytes` is the retained footprint.
///
/// Time-dependence: `older_than` is a wall-clock instant the CALLER supplies —
/// the library reads no clock (ADR-0020 decision 6). The CLI derives it from
/// `SystemTime::now()`. `max_total_bytes` and file sizes are read from the same
/// `metadata` stat used for mtimes, so the ceiling adds no extra syscalls.
///
/// A missing `runs_dir` is `Ok` with an all-zero report. Read failures
/// (unreadable directory, un-stattable file) and deletion failures are loud
/// errors, never silent skips: a GC that hid a failed unlink could report
/// space reclaimed that was not, or mask a permissions/corruption problem.
pub fn gc_with_limits(
    runs_dir: &Path,
    keep_newest: usize,
    protected: &BTreeSet<String>,
    older_than: Option<SystemTime>,
    max_total_bytes: Option<u64>,
) -> Result<GcSizeReport, String> {
    let all_zero = |kept_bytes: u64| GcSizeReport {
        report: GcReport {
            removed: 0,
            kept: 0,
            protected_kept: 0,
        },
        kept_bytes,
        max_total_bytes,
    };

    let read = match std::fs::read_dir(runs_dir) {
        Ok(read) => read,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(all_zero(0)),
        Err(e) => return Err(format!("read {}: {e}", runs_dir.display())),
    };

    // Phase 1: collect every candidate with its mtime AND size (one stat each).
    // Stat everything before deleting anything, so the pass never races its own
    // removals.
    let mut runs: Vec<(SystemTime, u64, String, PathBuf)> = Vec::new();
    for dirent in read {
        let dirent = dirent.map_err(|e| format!("read {}: {e}", runs_dir.display()))?;
        let path = dirent.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !is_run_id(stem) {
            continue;
        }
        let meta = std::fs::metadata(&path).map_err(|e| format!("stat {}: {e}", path.display()))?;
        // A directory (or anything non-regular) named like a run file is not a
        // record GC wrote: skip it rather than attempt a nonsensical unlink.
        if !meta.is_file() {
            continue;
        }
        let modified = meta
            .modified()
            .map_err(|e| format!("mtime {}: {e}", path.display()))?;
        runs.push((modified, meta.len(), stem.to_owned(), path));
    }

    // Newest first. The cutoffs below are mtime values, so ordering among
    // equal-mtime files does not affect which are kept.
    runs.sort_by_key(|(mtime, _, _, _)| std::cmp::Reverse(*mtime));
    let total_bytes: u64 = runs.iter().map(|(_, size, _, _)| *size).sum();

    // The window as an mtime cutoff: `None` keeps nothing by window
    // (`keep_newest == 0` — protection/age/size only); a window that covers
    // every file is a fast path — floor records are never evicted (by age or
    // size), so nothing can be removed. The ceiling may stay exceeded here;
    // that is reported, not forced.
    let cutoff = if keep_newest == 0 {
        None
    } else if keep_newest >= runs.len() {
        return Ok(GcSizeReport {
            report: GcReport {
                removed: 0,
                kept: runs.len(),
                protected_kept: 0,
            },
            kept_bytes: total_bytes,
            max_total_bytes,
        });
    } else {
        Some(runs[keep_newest - 1].0)
    };

    // Phase 2: classify. Floor / age-young / pinned records are retained and
    // never size-evicted; everything else is eviction-ELIGIBLE. `runs` is
    // newest-first, so pushing eligible in order then reversing yields
    // oldest-first for removal.
    let mut report = GcReport {
        removed: 0,
        kept: 0,
        protected_kept: 0,
    };
    let mut kept_bytes = total_bytes;
    let mut eligible: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
    for (modified, size, id, path) in runs {
        let beyond_floor = cutoff.is_none_or(|c| modified < c);
        let age_young = older_than.is_some_and(|c| modified >= c);
        if !beyond_floor || age_young {
            // floor-kept, or age-kept young: retained, not by a pin.
            report.kept += 1;
        } else if protected.contains(&id) {
            report.protected_kept += 1;
        } else {
            eligible.push((modified, size, path));
        }
    }
    let eligible_total = eligible.len();

    // Phase 3: remove eligible oldest-first, in whole mtime-tie groups, while
    // over the ceiling. With `max_total_bytes == None` the ceiling check is
    // never satisfied, so every eligible record is removed — exactly the
    // age/window rule (`gc_with_age`).
    eligible.reverse(); // oldest-first
    let mut i = 0;
    while i < eligible.len() {
        if max_total_bytes.is_some_and(|ceiling| kept_bytes <= ceiling) {
            break; // within budget: keep this record and every newer one
        }
        // remove the entire oldest remaining tie group atomically.
        let group_mtime = eligible[i].0;
        while i < eligible.len() && eligible[i].0 == group_mtime {
            let (_, size, path) = &eligible[i];
            std::fs::remove_file(path).map_err(|e| format!("remove {}: {e}", path.display()))?;
            report.removed += 1;
            kept_bytes -= *size;
            i += 1;
        }
    }
    // eligible records not removed are retained (not by a pin).
    report.kept += eligible_total - report.removed;

    Ok(GcSizeReport {
        report,
        kept_bytes,
        max_total_bytes,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::harness::{Check, CheckStatus, Verdict};

    fn sample_report() -> VerificationReport {
        VerificationReport {
            contract_id: "c".repeat(64),
            task: "toy".into(),
            subject: "test".into(),
            verdict: Verdict::Pass,
            observations: 2,
            checks: vec![Check {
                what: "example \"a\"".into(),
                status: CheckStatus::Passed,
                detail: None,
            }],
        }
    }

    #[test]
    fn id_is_stable_for_same_content() {
        let a = EvalRun::from_report(&sample_report(), 1_000);
        let b = EvalRun::from_report(&sample_report(), 1_000);
        assert_eq!(a.id(), b.id());
    }

    #[test]
    fn different_timestamp_is_a_different_run() {
        let a = EvalRun::from_report(&sample_report(), 1_000);
        let b = EvalRun::from_report(&sample_report(), 2_000);
        assert_ne!(a.id(), b.id());
    }

    #[test]
    fn written_file_matches_id_and_parses_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (path, id) = write_eval_run(&sample_report(), 1_000, dir.path()).expect("write");
        let content = std::fs::read_to_string(&path).expect("read");
        assert_eq!(
            auto_trace::model::digest_hex(content.trim_end_matches('\n')),
            id.0
        );
        let parsed: EvalRun = serde_json::from_str(&content).expect("parse");
        assert_eq!(parsed.id(), id);
        assert_eq!(parsed.verdict, "PASS");
    }

    // --- gc ---------------------------------------------------------------

    /// A fixed, far-past base instant. mtimes are set explicitly (no clocks,
    /// no sleeps) so ordering is deterministic on every platform.
    fn base_time() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    fn set_mtime(path: &Path, at: SystemTime) {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open for set_modified");
        file.set_modified(at).expect("set_modified");
    }

    /// Write a run-shaped file `<id>.json` and stamp its modified time. gc
    /// reads names and mtimes only, so the body is irrelevant here.
    fn write_run_at(dir: &Path, id: &str, at: SystemTime) -> PathBuf {
        let path = dir.join(format!("{id}.json"));
        std::fs::write(&path, b"{}").expect("write run file");
        set_mtime(&path, at);
        path
    }

    fn no_protection() -> BTreeSet<String> {
        BTreeSet::new()
    }

    #[test]
    fn gc_missing_dir_is_ok_all_zeros() {
        let dir = tempfile::tempdir().expect("tempdir");
        let report = gc(&dir.path().join("nope"), 5, &no_protection()).expect("gc");
        assert_eq!(
            report,
            GcReport {
                removed: 0,
                kept: 0,
                protected_kept: 0
            }
        );
    }

    #[test]
    fn gc_keeps_newest_and_protected_beyond_window_removes_rest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = base_time();
        // oldest -> newest: a, b, c, d, e, one second apart (distinct mtimes).
        let ids: Vec<String> = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|s| s.repeat(64))
            .collect();
        for (i, id) in ids.iter().enumerate() {
            write_run_at(dir.path(), id, base + Duration::from_secs(i as u64 + 1));
        }
        // Protect the OLDEST (a): it must survive despite being past the window.
        let protected: BTreeSet<String> = std::iter::once(ids[0].clone()).collect();

        let report = gc(dir.path(), 2, &protected).expect("gc");
        assert_eq!(
            report,
            GcReport {
                removed: 2,
                kept: 2,
                protected_kept: 1
            }
        );
        let present = |id: &str| dir.path().join(format!("{id}.json")).exists();
        assert!(present(&ids[4]), "e kept by window");
        assert!(present(&ids[3]), "d kept by window");
        assert!(present(&ids[0]), "a kept by protection");
        assert!(!present(&ids[1]), "b removed");
        assert!(!present(&ids[2]), "c removed");
    }

    #[test]
    fn gc_keep_zero_keeps_only_protected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = base_time();
        let ids: Vec<String> = ["a", "b", "c"].iter().map(|s| s.repeat(64)).collect();
        for (i, id) in ids.iter().enumerate() {
            write_run_at(dir.path(), id, base + Duration::from_secs(i as u64 + 1));
        }
        let protected: BTreeSet<String> = std::iter::once(ids[1].clone()).collect();
        let report = gc(dir.path(), 0, &protected).expect("gc");
        assert_eq!(
            report,
            GcReport {
                removed: 2,
                kept: 0,
                protected_kept: 1
            }
        );
        assert!(dir.path().join(format!("{}.json", ids[1])).exists());
        assert!(!dir.path().join(format!("{}.json", ids[0])).exists());
        assert!(!dir.path().join(format!("{}.json", ids[2])).exists());
    }

    #[test]
    fn gc_equal_mtimes_keep_more_never_less() {
        let dir = tempfile::tempdir().expect("tempdir");
        let same = base_time();
        let ids: Vec<String> = ["a", "b", "c"].iter().map(|s| s.repeat(64)).collect();
        for id in &ids {
            write_run_at(dir.path(), id, same); // identical mtime: a full tie
        }
        // keep_newest = 1 is below the count, but the boundary mtime ties across
        // every file, so the cutoff keeps them all. Deterministic regardless of
        // directory iteration order: ties keep more, never less.
        let report = gc(dir.path(), 1, &no_protection()).expect("gc");
        assert_eq!(
            report,
            GcReport {
                removed: 0,
                kept: 3,
                protected_kept: 0
            }
        );
        for id in &ids {
            assert!(dir.path().join(format!("{id}.json")).exists());
        }
    }

    #[test]
    fn gc_leaves_non_run_files_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = base_time();
        // One genuine run file: old and unprotected, so gc removes it.
        let run = "f".repeat(64);
        write_run_at(dir.path(), &run, base + Duration::from_secs(1));
        // Files gc must never touch — it wrote none of these:
        std::fs::write(dir.path().join("readme.txt"), b"hi").unwrap(); // wrong extension
        std::fs::write(dir.path().join("notes.json"), b"{}").unwrap(); // json, non-hex stem
        let non_hex = format!("{}.json", "g".repeat(64)); // 64 chars but 'g' is not hex
        std::fs::write(dir.path().join(&non_hex), b"{}").unwrap();
        let upper = format!("{}.json", "A".repeat(64)); // uppercase hex is not our shape
        std::fs::write(dir.path().join(&upper), b"{}").unwrap();
        let wrong_ext = format!("{}.jsonx", "b".repeat(64)); // hex stem, wrong extension
        std::fs::write(dir.path().join(&wrong_ext), b"{}").unwrap();
        let dir_named_like_run = format!("{}.json", "9".repeat(64)); // a DIRECTORY, not a file
        std::fs::create_dir(dir.path().join(&dir_named_like_run)).unwrap();

        let report = gc(dir.path(), 0, &no_protection()).expect("gc");
        assert_eq!(
            report,
            GcReport {
                removed: 1,
                kept: 0,
                protected_kept: 0
            }
        );
        assert!(
            !dir.path().join(format!("{run}.json")).exists(),
            "the one real run file is collected"
        );
        assert!(dir.path().join("readme.txt").exists());
        assert!(dir.path().join("notes.json").exists());
        assert!(dir.path().join(&non_hex).exists());
        assert!(dir.path().join(&upper).exists());
        assert!(dir.path().join(&wrong_ext).exists());
        assert!(dir.path().join(&dir_named_like_run).is_dir());
    }

    #[test]
    fn gc_over_records_written_by_write_eval_run() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Two genuine records; distinct bodies (created_unix_ms) -> distinct ids.
        let (p_old, id_old) = write_eval_run(&sample_report(), 1_000, dir.path()).expect("old");
        let (p_new, id_new) = write_eval_run(&sample_report(), 2_000, dir.path()).expect("new");
        assert_ne!(id_old, id_new);
        // Stamp the order explicitly rather than trust write order.
        let base = base_time();
        set_mtime(&p_old, base + Duration::from_secs(1));
        set_mtime(&p_new, base + Duration::from_secs(2));

        let report = gc(dir.path(), 1, &no_protection()).expect("gc");
        assert_eq!(
            report,
            GcReport {
                removed: 1,
                kept: 1,
                protected_kept: 0
            }
        );
        assert!(dir.path().join(format!("{id_new}.json")).exists());
        assert!(!dir.path().join(format!("{id_old}.json")).exists());
    }

    // --- gc_with_age (ADR-0020 age amendment) -----------------------------

    #[test]
    fn gc_with_age_none_is_exactly_plain_gc() {
        // The keep-newest + protected scenario, run through gc_with_age with no
        // age bound: it must reproduce the plain-gc report exactly. (gc itself
        // delegates here with None, so this pins the no-age path byte-identical.)
        let dir = tempfile::tempdir().expect("tempdir");
        let base = base_time();
        let ids: Vec<String> = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|s| s.repeat(64))
            .collect();
        for (i, id) in ids.iter().enumerate() {
            write_run_at(dir.path(), id, base + Duration::from_secs(i as u64 + 1));
        }
        let protected: BTreeSet<String> = std::iter::once(ids[0].clone()).collect();
        let report = gc_with_age(dir.path(), 2, &protected, None).expect("gc");
        assert_eq!(
            report,
            GcReport {
                removed: 2,
                kept: 2,
                protected_kept: 1
            }
        );
    }

    #[test]
    fn gc_with_age_keeps_young_records_beyond_the_window() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = base_time();
        // oldest -> newest at base+1 .. base+4.
        let ids: Vec<String> = ["a", "b", "c", "d"].iter().map(|s| s.repeat(64)).collect();
        for (i, id) in ids.iter().enumerate() {
            write_run_at(dir.path(), id, base + Duration::from_secs(i as u64 + 1));
        }
        // keep_newest = 1 keeps only d (base+4); a, b, c are past the floor.
        // Age cutoff = base+3: "strictly older" removes a(+1) and b(+2); c(+3)
        // sits exactly at the cutoff, so it is KEPT (ties at the cutoff keep,
        // like the window boundary). Age restricted deletion to the truly old.
        let cutoff = base + Duration::from_secs(3);
        let report = gc_with_age(dir.path(), 1, &no_protection(), Some(cutoff)).expect("gc");
        assert_eq!(
            report,
            GcReport {
                removed: 2,
                kept: 2,
                protected_kept: 0
            }
        );
        let present = |id: &str| dir.path().join(format!("{id}.json")).exists();
        assert!(present(&ids[3]), "d kept by window");
        assert!(present(&ids[2]), "c kept by age (at the cutoff)");
        assert!(!present(&ids[0]), "a removed (strictly older than cutoff)");
        assert!(!present(&ids[1]), "b removed (strictly older than cutoff)");
    }

    #[test]
    fn gc_with_age_and_protection_compose() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = base_time();
        // keep_newest = 0 makes the floor keep nothing, so age and pins are the
        // ONLY retention — isolating their composition into distinct buckets.
        let ids: Vec<String> = ["a", "b", "c"].iter().map(|s| s.repeat(64)).collect();
        // a: old + protected ; b: old + unprotected ; c: young + unprotected.
        write_run_at(dir.path(), &ids[0], base + Duration::from_secs(1));
        write_run_at(dir.path(), &ids[1], base + Duration::from_secs(2));
        write_run_at(dir.path(), &ids[2], base + Duration::from_secs(5));
        let protected: BTreeSet<String> = std::iter::once(ids[0].clone()).collect();
        let cutoff = base + Duration::from_secs(4); // a, b older; c younger
        let report = gc_with_age(dir.path(), 0, &protected, Some(cutoff)).expect("gc");
        // a: kept only by its pin (old) -> protected_kept; b: old, unpinned ->
        // removed; c: young -> age-kept -> kept.
        assert_eq!(
            report,
            GcReport {
                removed: 1,
                kept: 1,
                protected_kept: 1
            }
        );
        assert!(
            dir.path().join(format!("{}.json", ids[0])).exists(),
            "a kept by pin"
        );
        assert!(
            !dir.path().join(format!("{}.json", ids[1])).exists(),
            "b removed (old, unpinned)"
        );
        assert!(
            dir.path().join(format!("{}.json", ids[2])).exists(),
            "c kept by age"
        );
    }

    #[test]
    fn gc_with_age_missing_dir_is_ok_all_zeros() {
        let dir = tempfile::tempdir().expect("tempdir");
        let report = gc_with_age(
            &dir.path().join("nope"),
            5,
            &no_protection(),
            Some(base_time()),
        )
        .expect("gc");
        assert_eq!(
            report,
            GcReport {
                removed: 0,
                kept: 0,
                protected_kept: 0
            }
        );
    }

    // --- gc_with_limits (ADR-0020 size amendment) -------------------------

    /// Write a run-shaped file of exactly `size` bytes and stamp its mtime.
    fn write_run_sized(dir: &Path, id: &str, at: SystemTime, size: usize) -> PathBuf {
        let path = dir.join(format!("{id}.json"));
        std::fs::write(&path, vec![b'x'; size]).expect("write sized run file");
        set_mtime(&path, at);
        path
    }

    fn ids_n(letters: &[&str]) -> Vec<String> {
        letters.iter().map(|s| s.repeat(64)).collect()
    }

    #[test]
    fn gc_with_limits_none_reproduces_gc_with_age() {
        // the age+protection compose scenario, but through gc_with_limits with
        // no ceiling: it must produce the identical base GcReport (pinning the
        // None path byte-identical to gc_with_age, which delegates here).
        let dir = tempfile::tempdir().expect("tempdir");
        let base = base_time();
        let ids = ids_n(&["a", "b", "c"]);
        write_run_at(dir.path(), &ids[0], base + Duration::from_secs(1)); // old + pinned
        write_run_at(dir.path(), &ids[1], base + Duration::from_secs(2)); // old + unpinned
        write_run_at(dir.path(), &ids[2], base + Duration::from_secs(5)); // young
        let protected: BTreeSet<String> = std::iter::once(ids[0].clone()).collect();
        let cutoff = base + Duration::from_secs(4);
        let out = gc_with_limits(dir.path(), 0, &protected, Some(cutoff), None).expect("gc");
        assert_eq!(
            out.report,
            GcReport {
                removed: 1,
                kept: 1,
                protected_kept: 1
            }
        );
        assert!(!out.over_ceiling(), "no ceiling requested");
    }

    #[test]
    fn gc_with_limits_ceiling_removes_oldest_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = base_time();
        // a..d oldest->newest, 100 bytes each (400 total).
        let ids = ids_n(&["a", "b", "c", "d"]);
        for (i, id) in ids.iter().enumerate() {
            write_run_sized(
                dir.path(),
                id,
                base + Duration::from_secs(i as u64 + 1),
                100,
            );
        }
        // keep 0, no age; ceiling 250: 400 -> remove a (300) -> remove b (200
        // <= 250, stop). c, d survive as the newest under budget.
        let out = gc_with_limits(dir.path(), 0, &no_protection(), None, Some(250)).expect("gc");
        assert_eq!(
            out.report,
            GcReport {
                removed: 2,
                kept: 2,
                protected_kept: 0
            }
        );
        assert_eq!(out.kept_bytes, 200);
        assert!(!out.over_ceiling());
        let present = |id: &str| dir.path().join(format!("{id}.json")).exists();
        assert!(!present(&ids[0]) && !present(&ids[1]), "oldest two evicted");
        assert!(present(&ids[2]) && present(&ids[3]), "newest two kept");
    }

    #[test]
    fn gc_with_limits_pins_survive_an_exceeded_ceiling_loudly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = base_time();
        let ids = ids_n(&["a", "b"]);
        write_run_sized(dir.path(), &ids[0], base + Duration::from_secs(1), 100); // pinned
        write_run_sized(dir.path(), &ids[1], base + Duration::from_secs(2), 100); // evictable
        let protected: BTreeSet<String> = std::iter::once(ids[0].clone()).collect();
        // ceiling 50 < a pinned record alone: evict b, but a is pinned and can
        // never be size-evicted, so the ceiling stays exceeded — reported.
        let out = gc_with_limits(dir.path(), 0, &protected, None, Some(50)).expect("gc");
        assert_eq!(
            out.report,
            GcReport {
                removed: 1,
                kept: 0,
                protected_kept: 1
            }
        );
        assert_eq!(out.kept_bytes, 100);
        assert!(out.over_ceiling(), "pin keeps the dir over budget, loudly");
        assert!(
            dir.path().join(format!("{}.json", ids[0])).exists(),
            "pin survives"
        );
        assert!(!dir.path().join(format!("{}.json", ids[1])).exists());
    }

    #[test]
    fn gc_with_limits_composes_with_age_and_keep() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = base_time();
        // a..d oldest->newest, 100 bytes each.
        let ids = ids_n(&["a", "b", "c", "d"]);
        for (i, id) in ids.iter().enumerate() {
            write_run_sized(
                dir.path(),
                id,
                base + Duration::from_secs(i as u64 + 1),
                100,
            );
        }
        // keep 1 -> d floor-kept. age cutoff base+3 -> c young-kept; a, b eligible.
        // ceiling 350: 400 -> remove oldest eligible a (300 <= 350, stop). b is
        // old (age-eligible) yet SPARED — size removed only as much as needed,
        // and age protected c. Shows size bounds age-eviction.
        let cutoff = base + Duration::from_secs(3);
        let out =
            gc_with_limits(dir.path(), 1, &no_protection(), Some(cutoff), Some(350)).expect("gc");
        assert_eq!(
            out.report,
            GcReport {
                removed: 1,
                kept: 3,
                protected_kept: 0
            }
        );
        assert_eq!(out.kept_bytes, 300);
        let present = |id: &str| dir.path().join(format!("{id}.json")).exists();
        assert!(!present(&ids[0]), "a evicted (oldest, over budget)");
        assert!(present(&ids[1]), "b spared (budget met after a)");
        assert!(
            present(&ids[2]) && present(&ids[3]),
            "c age-kept, d floor-kept"
        );
    }

    #[test]
    fn gc_with_limits_never_partially_collects_a_tie_group() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = base_time();
        // three eligible records share ONE mtime tick; one newer record.
        let tied = ids_n(&["a", "b", "c"]);
        for id in &tied {
            write_run_sized(dir.path(), id, base + Duration::from_secs(1), 100);
        }
        let newer = "d".repeat(64);
        write_run_sized(dir.path(), &newer, base + Duration::from_secs(3), 100);
        // keep 0, ceiling 250: 400 over budget -> the oldest group is the whole
        // {a,b,c} tie. It is removed ATOMICALLY (never keep 1-2 of a tie), which
        // takes the total to 100 (d). Keeping part of the tie to land nearer 250
        // is exactly what ADR-0020 decision 5 forbids.
        let out = gc_with_limits(dir.path(), 0, &no_protection(), None, Some(250)).expect("gc");
        assert_eq!(
            out.report,
            GcReport {
                removed: 3,
                kept: 1,
                protected_kept: 0
            }
        );
        assert_eq!(out.kept_bytes, 100);
        for id in &tied {
            assert!(
                !dir.path().join(format!("{id}.json")).exists(),
                "whole tie evicted"
            );
        }
        assert!(
            dir.path().join(format!("{newer}.json")).exists(),
            "newer kept"
        );
    }

    #[test]
    fn gc_with_limits_under_ceiling_removes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = base_time();
        let ids = ids_n(&["a", "b"]);
        for (i, id) in ids.iter().enumerate() {
            write_run_sized(
                dir.path(),
                id,
                base + Duration::from_secs(i as u64 + 1),
                100,
            );
        }
        // total 200 <= ceiling 500: nothing removed, even with keep 0.
        let out = gc_with_limits(dir.path(), 0, &no_protection(), None, Some(500)).expect("gc");
        assert_eq!(
            out.report,
            GcReport {
                removed: 0,
                kept: 2,
                protected_kept: 0
            }
        );
        assert_eq!(out.kept_bytes, 200);
        assert!(!out.over_ceiling());
    }

    #[test]
    fn gc_with_limits_missing_dir_is_ok_all_zeros() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = gc_with_limits(
            &dir.path().join("nope"),
            5,
            &no_protection(),
            None,
            Some(100),
        )
        .expect("gc");
        assert_eq!(
            out.report,
            GcReport {
                removed: 0,
                kept: 0,
                protected_kept: 0
            }
        );
        assert_eq!(out.kept_bytes, 0);
        assert!(!out.over_ceiling());
    }
}
