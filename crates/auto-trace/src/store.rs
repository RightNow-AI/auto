//! Sqlite-backed trace store (schema version 3, owned by this crate).
//!
//! The store is the rust side's single source of truth: JSONL files are the
//! emission format, the store is where analysis reads from. Digests are
//! computed here at ingest (never trusted from the wire). Schema changes bump
//! `PRAGMA user_version` and get a migration decision — never a silent one.
//!
//! Schema history:
//! - v1: traces + spans.
//! - v2 (ADR-0025): three additive nullable columns on `traces` for
//!   task-level I/O (`task_input`, `task_output`,
//!   `task_output_recorded_at_ms`).
//! - v3 (ADR-0030): one additive column `partial` on `traces` — a trace
//!   ingested from a torn tail (`ingest_partial`) records `partial = 1`;
//!   `ingest` records `0`. Partial traces are excluded from determinism
//!   witnessing and from verification evidence (`load_task`); `load_task_all`
//!   surfaces them so a consumer can exclude AND report doing so.
//!
//! Migrations run in sequence on open (v1→v2→v3), each an
//! `ALTER TABLE ... ADD COLUMN` guarded by a `pragma table_info` check so a
//! migration interrupted between the ALTER and the version bump resumes
//! instead of failing; any failure is a loud error. Old rows read as their
//! pre-migration default (no task I/O; `partial = 0`, i.e. complete). Older
//! builds refuse a newer store loudly (version mismatch) — never a silent
//! misread.

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::Connection;
use serde_json::Value;

use crate::TraceError;
use crate::jsonl::validate_spans;
use crate::model::{
    Span, SpanId, SpanKind, TaskOutput, Trace, TraceHeader, TraceId, canonical_json,
};

const STORE_VERSION: i64 = 3;

const SCHEMA: &str = "
BEGIN;
CREATE TABLE traces(
  trace_id TEXT PRIMARY KEY,
  task TEXT NOT NULL,
  started_at_ms INTEGER NOT NULL,
  sdk TEXT NOT NULL,
  attrs TEXT NOT NULL,
  task_input TEXT,
  task_output TEXT,
  task_output_recorded_at_ms INTEGER,
  partial INTEGER NOT NULL DEFAULT 0
) STRICT;
CREATE TABLE spans(
  trace_id TEXT NOT NULL REFERENCES traces(trace_id),
  span_id INTEGER NOT NULL,
  parent_span_id INTEGER,
  seq INTEGER NOT NULL,
  kind TEXT NOT NULL,
  name TEXT NOT NULL,
  input TEXT NOT NULL,
  input_digest TEXT NOT NULL,
  output TEXT,
  output_digest TEXT NOT NULL,
  error TEXT,
  started_at_ms INTEGER NOT NULL,
  duration_ms INTEGER NOT NULL,
  attrs TEXT NOT NULL,
  PRIMARY KEY(trace_id, span_id)
) STRICT;
CREATE INDEX spans_signature ON spans(kind, name, input_digest);
PRAGMA user_version = 3;
COMMIT;
";

/// The v1 → v2 migration (ADR-0025): additive nullable columns only, so v1
/// data is untouched and every v1 row simply reads as "no task I/O recorded".
fn migrate_v1_to_v2(conn: &Connection) -> Result<(), TraceError> {
    let mut existing: Vec<String> = Vec::new();
    {
        let mut stmt = conn.prepare("PRAGMA table_info(traces)")?;
        let names = stmt.query_map([], |r| r.get::<_, String>(1))?;
        for name in names {
            existing.push(name?);
        }
    }
    let mut batch = String::from("BEGIN;\n");
    for (column, ty) in [
        ("task_input", "TEXT"),
        ("task_output", "TEXT"),
        ("task_output_recorded_at_ms", "INTEGER"),
    ] {
        if !existing.iter().any(|c| c == column) {
            batch.push_str(&format!("ALTER TABLE traces ADD COLUMN {column} {ty};\n"));
        }
    }
    batch.push_str("PRAGMA user_version = 2;\nCOMMIT;\n");
    conn.execute_batch(&batch)?;
    Ok(())
}

/// The v2 → v3 migration (ADR-0030): one additive column `partial` on
/// `traces`, `NOT NULL DEFAULT 0`, so every existing row reads as complete
/// (`partial = 0`). Guarded by a `pragma table_info` check so an interrupted
/// migration resumes; failures are loud.
fn migrate_v2_to_v3(conn: &Connection) -> Result<(), TraceError> {
    let mut existing: Vec<String> = Vec::new();
    {
        let mut stmt = conn.prepare("PRAGMA table_info(traces)")?;
        let names = stmt.query_map([], |r| r.get::<_, String>(1))?;
        for name in names {
            existing.push(name?);
        }
    }
    let mut batch = String::from("BEGIN;\n");
    if !existing.iter().any(|c| c == "partial") {
        batch.push_str("ALTER TABLE traces ADD COLUMN partial INTEGER NOT NULL DEFAULT 0;\n");
    }
    batch.push_str("PRAGMA user_version = 3;\nCOMMIT;\n");
    conn.execute_batch(&batch)?;
    Ok(())
}

/// A trace loaded from the store, tagged with whether it was ingested from a
/// torn tail (ADR-0030). `load_task` returns complete traces only;
/// `load_task_all` returns these so a caller can exclude partials AND report
/// that it did — never silently thinner evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredTrace {
    pub trace: Trace,
    pub partial: bool,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (and initialize if fresh) a store at `path`. Older stores migrate
    /// in place, in sequence (v1→v2→v3; see module docs); a failed migration is
    /// a loud error, and a store newer than this build is refused loudly.
    pub fn open(path: &Path) -> Result<Self, TraceError> {
        let conn = Connection::open(path)?;
        // WAL for sane concurrent reader behavior; the pragma returns a row
        let _mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        conn.execute("PRAGMA foreign_keys=ON", [])?;
        let mut version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version == 0 {
            conn.execute_batch(SCHEMA)?;
            version = STORE_VERSION;
        }
        if version == 1 {
            migrate_v1_to_v2(&conn)?;
            version = 2;
        }
        if version == 2 {
            migrate_v2_to_v3(&conn)?;
            version = 3;
        }
        if version != STORE_VERSION {
            return Err(TraceError::StoreVersionMismatch {
                found: version,
                supported: STORE_VERSION,
            });
        }
        Ok(Self { conn })
    }

    /// Ingest one parsed trace atomically as a COMPLETE record. Re-ingesting a
    /// trace id is an error, not an upsert — traces are immutable records.
    pub fn ingest(&mut self, trace: &Trace) -> Result<(), TraceError> {
        self.ingest_inner(trace, false)
    }

    /// Ingest a trace recovered from a torn tail (ADR-0030), marking it
    /// PARTIAL. A partial trace is excluded from determinism witnessing and
    /// from verification evidence (`load_task`); `load_task_all` surfaces it.
    /// Otherwise identical to [`ingest`] (immutable, digests computed here).
    pub fn ingest_partial(&mut self, trace: &Trace) -> Result<(), TraceError> {
        self.ingest_inner(trace, true)
    }

    fn ingest_inner(&mut self, trace: &Trace, partial: bool) -> Result<(), TraceError> {
        validate_spans(&trace.spans)?;
        let tx = self.conn.transaction()?;
        let trace_key = trace.header.trace_id.to_string();
        let already: bool = tx
            .query_row(
                "SELECT 1 FROM traces WHERE trace_id = ?1",
                [&trace_key],
                |_| Ok(true),
            )
            .map(|_| true)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(false),
                other => Err(other),
            })?;
        if already {
            return Err(TraceError::DuplicateTrace(trace.header.trace_id));
        }
        tx.execute(
            "INSERT INTO traces(trace_id, task, started_at_ms, sdk, attrs,
                                task_input, task_output, task_output_recorded_at_ms, partial)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                trace_key,
                trace.header.task,
                to_i64(trace.header.started_at_ms, "trace started_at_ms")?,
                trace.header.sdk,
                attrs_json(&trace.header.attrs),
                trace.header.task_input.as_ref().map(canonical_json),
                trace
                    .header
                    .task_output
                    .as_ref()
                    .map(|o| canonical_json(&o.value)),
                trace
                    .header
                    .task_output
                    .as_ref()
                    .map(|o| to_i64(o.recorded_at_ms, "task_output recorded_at_ms"))
                    .transpose()?,
                i64::from(partial),
            ],
        )?;
        {
            let mut insert = tx.prepare(
                "INSERT INTO spans(trace_id, span_id, parent_span_id, seq, kind, name,
                                   input, input_digest, output, output_digest, error,
                                   started_at_ms, duration_ms, attrs)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            )?;
            for span in &trace.spans {
                insert.execute(rusqlite::params![
                    trace_key,
                    to_i64(span.span_id.0, "span_id")?,
                    span.parent_span_id
                        .map(|p| to_i64(p.0, "parent_span_id"))
                        .transpose()?,
                    to_i64(span.seq, "seq")?,
                    span.kind.wire(),
                    span.name,
                    canonical_json(&span.input),
                    span.input_digest(),
                    span.output.as_ref().map(canonical_json),
                    span.output_digest(),
                    span.error,
                    to_i64(span.started_at_ms, "span started_at_ms")?,
                    to_i64(span.duration_ms, "duration_ms")?,
                    attrs_json(&span.attrs),
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// All task labels present, sorted.
    pub fn tasks(&self) -> Result<Vec<String>, TraceError> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT task FROM traces ORDER BY task")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Trace ids for a task, oldest first.
    pub fn trace_ids(&self, task: &str) -> Result<Vec<TraceId>, TraceError> {
        let mut stmt = self.conn.prepare(
            "SELECT trace_id FROM traces WHERE task = ?1 ORDER BY started_at_ms, trace_id",
        )?;
        let rows = stmt.query_map([task], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for raw in rows {
            let raw = raw?;
            out.push(
                TraceId::parse(&raw).ok_or_else(|| TraceError::CorruptStore {
                    why: format!("bad trace_id {raw:?} in traces table"),
                })?,
            );
        }
        Ok(out)
    }

    /// Load one full trace. Partiality (ADR-0030) is not surfaced here — a
    /// caller asking for a specific id gets the record regardless; use
    /// `load_task_all` when the partial flag matters.
    pub fn load_trace(&self, id: TraceId) -> Result<Trace, TraceError> {
        Ok(self.load_trace_tagged(id)?.0)
    }

    /// Load one full trace together with its partial flag (ADR-0030).
    fn load_trace_tagged(&self, id: TraceId) -> Result<(Trace, bool), TraceError> {
        let key = id.to_string();
        let header = self
            .conn
            .query_row(
                "SELECT task, started_at_ms, sdk, attrs,
                        task_input, task_output, task_output_recorded_at_ms, partial
                 FROM traces WHERE trace_id = ?1",
                [&key],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, Option<String>>(5)?,
                        r.get::<_, Option<i64>>(6)?,
                        r.get::<_, i64>(7)?,
                    ))
                },
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => TraceError::UnknownTrace(id),
                other => TraceError::Sqlite(other),
            })?;
        let (task, started_at_ms, sdk, attrs_raw, task_input, task_output, task_output_at, partial) =
            header;
        let task_output = match (task_output, task_output_at) {
            (Some(raw), Some(at)) => Some(TaskOutput {
                value: parse_json(&raw)?,
                recorded_at_ms: from_i64(at, "task_output recorded_at_ms")?,
            }),
            (None, None) => None,
            _ => {
                return Err(TraceError::CorruptStore {
                    why: "task_output and task_output_recorded_at_ms must be null together"
                        .to_owned(),
                });
            }
        };
        let header = TraceHeader {
            trace_id: id,
            task,
            started_at_ms: from_i64(started_at_ms, "trace started_at_ms")?,
            sdk,
            attrs: parse_attrs(&attrs_raw)?,
            task_input: task_input.as_deref().map(parse_json).transpose()?,
            task_output,
        };

        let mut stmt = self.conn.prepare(
            "SELECT span_id, parent_span_id, seq, kind, name, input, output, error,
                    started_at_ms, duration_ms, attrs
             FROM spans WHERE trace_id = ?1 ORDER BY seq",
        )?;
        let rows = stmt.query_map([&key], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, i64>(8)?,
                r.get::<_, i64>(9)?,
                r.get::<_, String>(10)?,
            ))
        })?;
        let mut spans = Vec::new();
        for row in rows {
            let (span_id, parent, seq, kind, name, input, output, error, started, duration, attrs) =
                row?;
            spans.push(Span {
                span_id: SpanId(from_i64(span_id, "span_id")?),
                parent_span_id: parent
                    .map(|p| from_i64(p, "parent_span_id").map(SpanId))
                    .transpose()?,
                seq: from_i64(seq, "seq")?,
                kind: SpanKind::from_wire(&kind).ok_or_else(|| TraceError::CorruptStore {
                    why: format!("unknown span kind {kind:?} in spans table"),
                })?,
                name,
                input: parse_json(&input)?,
                output: output.as_deref().map(parse_json).transpose()?,
                error,
                started_at_ms: from_i64(started, "span started_at_ms")?,
                duration_ms: from_i64(duration, "duration_ms")?,
                attrs: parse_attrs(&attrs)?,
            });
        }
        Ok((Trace { header, spans }, partial != 0))
    }

    /// Load every COMPLETE trace of a task, oldest first. Torn-tail partials
    /// (ADR-0030) are excluded, so verification and replay never rest on a
    /// truncated record. Errors if the task has NO traces at all; a task with
    /// only partial traces returns an empty vec (its partials are surfaced via
    /// `load_task_all`). On a store with no partial traces this is
    /// byte-identical to the pre-ADR-0030 `load_task`.
    pub fn load_task(&self, task: &str) -> Result<Vec<Trace>, TraceError> {
        Ok(self
            .load_task_all(task)?
            .into_iter()
            .filter(|st| !st.partial)
            .map(|st| st.trace)
            .collect())
    }

    /// Load every trace of a task, oldest first, each tagged with its partial
    /// flag (ADR-0030). Errors if the task has no traces — an empty analysis
    /// input should be loud, not a silent zero. Consumers that must exclude
    /// partials AND report doing so (the determinism report; verification) read
    /// through here.
    pub fn load_task_all(&self, task: &str) -> Result<Vec<StoredTrace>, TraceError> {
        let ids = self.trace_ids(task)?;
        if ids.is_empty() {
            return Err(TraceError::UnknownTask(task.to_owned()));
        }
        ids.into_iter()
            .map(|id| {
                self.load_trace_tagged(id)
                    .map(|(trace, partial)| StoredTrace { trace, partial })
            })
            .collect()
    }
}

fn attrs_json(attrs: &BTreeMap<String, String>) -> String {
    serde_json::to_string(attrs).expect("string map serialization cannot fail")
}

fn parse_attrs(raw: &str) -> Result<BTreeMap<String, String>, TraceError> {
    serde_json::from_str(raw).map_err(|e| TraceError::CorruptStore {
        why: format!("bad attrs json: {e}"),
    })
}

fn parse_json(raw: &str) -> Result<Value, TraceError> {
    serde_json::from_str(raw).map_err(|e| TraceError::CorruptStore {
        why: format!("bad value json: {e}"),
    })
}

fn to_i64(v: u64, what: &'static str) -> Result<i64, TraceError> {
    i64::try_from(v).map_err(|_| TraceError::ValueOutOfRange { what })
}

fn from_i64(v: i64, what: &'static str) -> Result<u64, TraceError> {
    u64::try_from(v).map_err(|_| TraceError::CorruptStore {
        why: format!("negative {what}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SpanKind;

    fn sample_trace(id: u128, task: &str, output: &str) -> Trace {
        Trace {
            header: TraceHeader {
                trace_id: TraceId(id),
                task: task.to_owned(),
                started_at_ms: 1_000,
                sdk: "test/0".into(),
                attrs: BTreeMap::from([("host".to_owned(), "unit".to_owned())]),
                task_input: None,
                task_output: None,
            },
            spans: vec![
                Span {
                    span_id: SpanId(1),
                    parent_span_id: None,
                    seq: 1,
                    kind: SpanKind::Span,
                    name: "step".into(),
                    input: serde_json::json!({}),
                    output: None,
                    error: None,
                    started_at_ms: 1_000,
                    duration_ms: 50,
                    attrs: BTreeMap::new(),
                },
                Span {
                    span_id: SpanId(2),
                    parent_span_id: Some(SpanId(1)),
                    seq: 2,
                    kind: SpanKind::ToolCall,
                    name: "wordcount".into(),
                    input: serde_json::json!({"text": "a b"}),
                    output: Some(serde_json::json!(output)),
                    error: None,
                    started_at_ms: 1_001,
                    duration_ms: 3,
                    attrs: BTreeMap::new(),
                },
            ],
        }
    }

    fn open_temp() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(&dir.path().join("t.db")).expect("open store");
        (dir, store)
    }

    #[test]
    fn ingest_and_load_roundtrip() {
        let (_dir, mut store) = open_temp();
        let trace = sample_trace(1, "toy", "2");
        store.ingest(&trace).unwrap();
        let loaded = store.load_trace(TraceId(1)).unwrap();
        assert_eq!(loaded, trace);
    }

    #[test]
    fn duplicate_ingest_rejected() {
        let (_dir, mut store) = open_temp();
        let trace = sample_trace(1, "toy", "2");
        store.ingest(&trace).unwrap();
        assert!(matches!(
            store.ingest(&trace),
            Err(TraceError::DuplicateTrace(TraceId(1)))
        ));
    }

    #[test]
    fn tasks_and_trace_ids_listed_sorted() {
        let (_dir, mut store) = open_temp();
        store.ingest(&sample_trace(2, "beta", "2")).unwrap();
        store.ingest(&sample_trace(1, "alpha", "2")).unwrap();
        store.ingest(&sample_trace(3, "beta", "2")).unwrap();
        assert_eq!(store.tasks().unwrap(), vec!["alpha", "beta"]);
        assert_eq!(
            store.trace_ids("beta").unwrap(),
            vec![TraceId(2), TraceId(3)]
        );
    }

    #[test]
    fn unknown_trace_and_task_are_loud() {
        let (_dir, store) = open_temp();
        assert!(matches!(
            store.load_trace(TraceId(9)),
            Err(TraceError::UnknownTrace(TraceId(9)))
        ));
        assert!(matches!(
            store.load_task("ghost"),
            Err(TraceError::UnknownTask(_))
        ));
    }

    #[test]
    fn future_store_version_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v9.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA user_version = 9;").unwrap();
        }
        assert!(matches!(
            Store::open(&path),
            Err(TraceError::StoreVersionMismatch { found: 9, .. })
        ));
    }

    #[test]
    fn reopen_preserves_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        {
            let mut store = Store::open(&path).unwrap();
            store.ingest(&sample_trace(1, "toy", "2")).unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.tasks().unwrap(), vec!["toy"]);
    }

    // --- task-level I/O (ADR-0025) -------------------------------------

    fn task_io_trace(id: u128) -> Trace {
        let mut t = sample_trace(id, "toy", "2");
        t.header.task_input = Some(serde_json::json!({"doc": "d"}));
        t.header.task_output = Some(crate::model::TaskOutput {
            value: serde_json::json!({"words": 2}),
            recorded_at_ms: 1_250,
        });
        t
    }

    #[test]
    fn task_io_roundtrips_through_the_store() {
        let (_dir, mut store) = open_temp();
        let trace = task_io_trace(1);
        store.ingest(&trace).unwrap();
        assert_eq!(store.load_trace(TraceId(1)).unwrap(), trace);
    }

    /// The exact v1 schema as shipped, for migration testing.
    const V1_SCHEMA: &str = "
    BEGIN;
    CREATE TABLE traces(
      trace_id TEXT PRIMARY KEY,
      task TEXT NOT NULL,
      started_at_ms INTEGER NOT NULL,
      sdk TEXT NOT NULL,
      attrs TEXT NOT NULL
    ) STRICT;
    CREATE TABLE spans(
      trace_id TEXT NOT NULL REFERENCES traces(trace_id),
      span_id INTEGER NOT NULL,
      parent_span_id INTEGER,
      seq INTEGER NOT NULL,
      kind TEXT NOT NULL,
      name TEXT NOT NULL,
      input TEXT NOT NULL,
      input_digest TEXT NOT NULL,
      output TEXT,
      output_digest TEXT NOT NULL,
      error TEXT,
      started_at_ms INTEGER NOT NULL,
      duration_ms INTEGER NOT NULL,
      attrs TEXT NOT NULL,
      PRIMARY KEY(trace_id, span_id)
    ) STRICT;
    CREATE INDEX spans_signature ON spans(kind, name, input_digest);
    PRAGMA user_version = 1;
    COMMIT;
    ";

    #[test]
    fn v1_store_migrates_and_old_rows_read_as_no_task_io() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v1.db");
        {
            // a genuine v1 store with one v1-era row
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(V1_SCHEMA).unwrap();
            conn.execute(
                "INSERT INTO traces(trace_id, task, started_at_ms, sdk, attrs)
                 VALUES ('00000000000000000000000000000001', 'toy', 1000, 'test/0',
                         '{\"host\":\"unit\"}')",
                [],
            )
            .unwrap();
        }
        let mut store = Store::open(&path).expect("v1 store opens via migration");
        let loaded = store.load_trace(TraceId(1)).unwrap();
        assert_eq!(loaded.header.task_input, None);
        assert_eq!(loaded.header.task_output, None);
        // the v1 row reads as complete (partial defaults to 0 through v3)
        assert!(!store.load_task_all("toy").unwrap()[0].partial);
        // the migrated store is fully current: task I/O ingests and reloads
        let trace = task_io_trace(2);
        store.ingest(&trace).unwrap();
        assert_eq!(store.load_trace(TraceId(2)).unwrap(), trace);
        let version: i64 = store
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, STORE_VERSION);
    }

    #[test]
    fn interrupted_migration_resumes() {
        // simulate a crash between ALTER TABLE and the version bump: columns
        // exist but user_version is still 1 — reopening must succeed
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("half.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(V1_SCHEMA).unwrap();
            conn.execute_batch(
                "ALTER TABLE traces ADD COLUMN task_input TEXT;
                 ALTER TABLE traces ADD COLUMN task_output TEXT;",
            )
            .unwrap();
        }
        let store = Store::open(&path).expect("half-migrated v1 store resumes");
        let version: i64 = store
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, STORE_VERSION);
    }

    // --- torn-tail partial flag (ADR-0030) ------------------------------

    /// The exact v2 schema as shipped (pre-ADR-0030), for migration testing.
    const V2_SCHEMA: &str = "
    BEGIN;
    CREATE TABLE traces(
      trace_id TEXT PRIMARY KEY,
      task TEXT NOT NULL,
      started_at_ms INTEGER NOT NULL,
      sdk TEXT NOT NULL,
      attrs TEXT NOT NULL,
      task_input TEXT,
      task_output TEXT,
      task_output_recorded_at_ms INTEGER
    ) STRICT;
    CREATE TABLE spans(
      trace_id TEXT NOT NULL REFERENCES traces(trace_id),
      span_id INTEGER NOT NULL,
      parent_span_id INTEGER,
      seq INTEGER NOT NULL,
      kind TEXT NOT NULL,
      name TEXT NOT NULL,
      input TEXT NOT NULL,
      input_digest TEXT NOT NULL,
      output TEXT,
      output_digest TEXT NOT NULL,
      error TEXT,
      started_at_ms INTEGER NOT NULL,
      duration_ms INTEGER NOT NULL,
      attrs TEXT NOT NULL,
      PRIMARY KEY(trace_id, span_id)
    ) STRICT;
    CREATE INDEX spans_signature ON spans(kind, name, input_digest);
    PRAGMA user_version = 2;
    COMMIT;
    ";

    #[test]
    fn partial_flag_roundtrips_and_load_task_excludes_it() {
        let (_dir, mut store) = open_temp();
        store.ingest(&sample_trace(1, "toy", "2")).unwrap();
        store.ingest_partial(&sample_trace(2, "toy", "2")).unwrap();

        // load_trace is partial-agnostic: both records load by id
        assert!(store.load_trace(TraceId(2)).is_ok());

        // load_task returns COMPLETE traces only
        let complete = store.load_task("toy").unwrap();
        assert_eq!(complete.len(), 1);
        assert_eq!(complete[0].header.trace_id, TraceId(1));

        // load_task_all surfaces both, tagged
        let all = store.load_task_all("toy").unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!((all[0].partial, all[1].partial), (false, true));
    }

    #[test]
    fn task_with_only_partials_loads_empty_but_not_unknown() {
        let (_dir, mut store) = open_temp();
        store.ingest_partial(&sample_trace(1, "torn", "2")).unwrap();
        // the task exists (so not UnknownTask), but has no complete traces
        assert!(store.load_task("torn").unwrap().is_empty());
        assert_eq!(store.load_task_all("torn").unwrap().len(), 1);
        assert!(matches!(
            store.load_task("ghost"),
            Err(TraceError::UnknownTask(_))
        ));
    }

    #[test]
    fn v2_store_migrates_to_v3_and_old_rows_read_complete() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v2.db");
        {
            // a genuine v2 store with one v2-era row (no `partial` column)
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(V2_SCHEMA).unwrap();
            conn.execute(
                "INSERT INTO traces(trace_id, task, started_at_ms, sdk, attrs)
                 VALUES ('00000000000000000000000000000001', 'toy', 1000, 'test/0',
                         '{\"host\":\"unit\"}')",
                [],
            )
            .unwrap();
        }
        let mut store = Store::open(&path).expect("v2 store opens via migration");
        // the pre-ADR-0030 row reads as complete
        assert!(!store.load_task_all("toy").unwrap()[0].partial);
        // the migrated store is fully v3: partial ingest works and reloads
        store.ingest_partial(&sample_trace(2, "toy", "9")).unwrap();
        let all = store.load_task_all("toy").unwrap();
        assert_eq!(all.iter().filter(|st| st.partial).count(), 1);
        let version: i64 = store
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 3);
    }

    #[test]
    fn interrupted_v2_to_v3_migration_resumes() {
        // crash between the partial ADD COLUMN and the version bump: column
        // exists but user_version is still 2 — reopening must resume, not fail
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("half3.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(V2_SCHEMA).unwrap();
            conn.execute_batch("ALTER TABLE traces ADD COLUMN partial INTEGER NOT NULL DEFAULT 0;")
                .unwrap();
        }
        let store = Store::open(&path).expect("half-migrated v2 store resumes");
        let version: i64 = store
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, STORE_VERSION);
    }
}
