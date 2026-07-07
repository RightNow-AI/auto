# ADR-0002: trace emission and storage — JSONL per run + rust-owned sqlite

status: accepted · scope: `crates/auto-trace`, `sdk/python`, `spec/trace.md`

## context

S1 records real agent runs from python (and later typescript) processes and
analyzes them in rust. Requirements: crash-safe capture while the agent runs;
lossless carriage of recorded values (digest-based determinism grouping
breaks if values mutate in transit); local-first (CLAUDE.md: "local
parquet/sqlite first; clickhouse when volume demands"); no network anywhere
in the loop; strict, versioned formats.

## decision

Two layers with one owner each:

1. **Emission: one JSONL file per run**, written by the SDK — one flushed
   line per event, header first, format version field on every line. Spans
   are written at close but ordered by `seq` (assigned at open), so a crashed
   run loses at most its unclosed spans.
2. **Store: sqlite owned by the rust side** (`rusqlite` with the bundled
   sqlite, STRICT tables, WAL, `user_version` = 1). `auto record` ingests the
   JSONL after the child exits; digests are computed at ingest, never trusted
   from input; traces are immutable once ingested.

Supporting decisions: digests are implementation-local (never wire data), so
no cross-language canonical-JSON agreement is needed; `serde_json`'s
`float_roundtrip` feature is mandatory (the default parser is lossy at
extreme float magnitudes — found by the round-trip property test, not by
reading docs); `env_read` records digest+length, never the value.

## alternatives considered

**SDK writes sqlite directly.** One layer fewer, queryable mid-run. Rejected:
every SDK language then embeds sqlite and must agree on schema + write
discipline (WAL locking across concurrently-recorded runs, partial-write
semantics on crash); the store schema stops being rust-owned and becomes a
cross-language contract, which is exactly what version-skew bugs are made of.
JSONL keeps the SDK contract at "append canonical lines to a file".

**Parquet emission.** Matches the constitution's "parquet/sqlite first" and
is right for *bulk analytics later*. Rejected for the capture path: columnar
formats want batched, footer-finalized writes — an agent killed mid-run loses
the file, and streaming append is against the grain. Parquet export from the
store can be added when volume demands (open question).

**OpenTelemetry OTLP export.** Mature ecosystem, obvious reach. Rejected as
the *primary* path: it puts a collector (and usually a network hop) inside
the capture loop, violating local-first and "no network in the loop", and
OTLP's span model would still need our fields (inputs/outputs, seq,
substitution semantics) as attributes — we would parse our own data out of
someone else's envelope. We keep otel-*compatible semantics* (trace/span/
parent ids, nesting) so a bridge exporter stays cheap (open question).

**One JSON document per run.** Simplest to parse. Rejected: not crash-safe
(nothing is readable until the run ends and the document closes), and
requires buffering the whole run in memory.

## consequences

- Two formats to version (JSONL `v`, store `user_version`), each with an
  exact-match read policy and loud failures.
- Ingest is a copy (JSONL → sqlite): traces exist twice transiently. The raw
  file is deleted after ingest unless `--keep-jsonl`.
- The strict parser means a hard-killed agent's truncated final line fails
  the whole-file parse; a lossy recovery mode is deliberately deferred
  (open question) rather than silently shipped.

## sources

- `rusqlite` 0.40.1 (2026-06-06): <https://crates.io/crates/rusqlite> —
  bundled sqlite, STRICT tables. (0.40's `libsqlite3-sys` 0.38 requires
  `cfg_select!`, which forced the workspace toolchain to rust 1.96.1.)
- `serde` 1.0.228 (2025-09-27), `serde_json` 1.0.150 (2026-05-21) and its
  `float_roundtrip` feature: <https://crates.io/crates/serde_json> and
  <https://docs.rs/serde_json> ("Use sufficient precision when parsing fixed
  precision floats … to ensure that they maintain accuracy when
  round-tripped").
- `sha2` 0.11.0 (2026-03-25): <https://crates.io/crates/sha2>
- `pytest` 9.1.1: <https://pypi.org/project/pytest/> (sdk tests; python ≥3.10)
- OpenTelemetry trace semantics (id/parenting model mirrored, OTLP not
  adopted): <https://opentelemetry.io/docs/specs/otel/trace/>
