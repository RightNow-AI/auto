# ADR-0030: torn-tail recovery — explicit, opt-in, partial-marked; strict stays the default

status: accepted · scope: `crates/auto-trace`
(`jsonl` recovery entry points, store schema v3 + `partial`, determinism
report exclusion), `crates/auto-cli` (a `--recover-partial` flag on the
`record` re-parse path — wiring), `spec/trace.md` (§12), `spec/adr/open-questions.md`.

## context

Recorded at S1: the strict JSONL parser fails the WHOLE file on a torn last
line. A JSONL trace is written line-by-line, each record terminated by `\n`;
a hard kill (OOM, SIGKILL, power loss) mid-write leaves the final record
truncated with no trailing newline. `parse_file` reads the whole file and
`deny_unknown_fields`/JSON parsing rejects that final fragment, discarding
every fully-committed span before it. For a long agent run that is a lot of
real evidence thrown away for one interrupted write.

The fix is not to loosen the strict parser — a best-effort default is exactly
the "silent misread" the format's strictness exists to prevent (spec/trace.md
§3). It is a SEPARATE, explicitly opt-in recovery path that rescues the
committed prefix, marks the result partial, and says what it dropped — and
that carries the partial mark honestly through every consumer so a torn record
never masquerades as a complete one.

## decision

1. **Separate opt-in entry points; strict path unchanged and default.**
   `jsonl::parse_file_recovering(path) -> Result<RecoveredTrace, TraceError>`
   and its byte-level core `parse_bytes_recovering(&[u8])`. `parse_file` /
   `parse_str` are byte-for-byte unchanged and remain the default everywhere.
   `RecoveredTrace { trace, dropped: Option<DroppedTail> }`: `dropped == None`
   is exactly the strict result (a valid file recovers to itself, not marked
   partial); `Some(DroppedTail { line, bytes, reason })` means a torn final
   line was dropped and the trace is partial (`is_partial()`).

2. **Only an *unterminated* final line that fails to parse qualifies.** The
   torn-tail signature is a last line with no trailing `\n` (the terminator was
   never written). Concretely, recovery splits at the last newline into the
   committed `prefix` and the `tail`:
   - tail empty / whitespace-only (file ends cleanly): nothing torn — parse the
     whole file strictly, so **middle-line AND last-line corruption in a
     properly terminated file stay strict errors**.
   - tail present but the whole file still strict-parses: the last line was a
     COMPLETE record that merely lost its newline (killed after the bytes,
     before the `\n`) — keep it, **not** partial (`str::lines()` already yields
     it, matching the strict read).
   - tail present and the whole file does not parse: the final line is torn.
     Parse the committed `prefix` strictly. If the prefix fails, the corruption
     is in a committed (middle) line — a **strict error even in recovery
     mode**, propagated. A torn header leaves an empty prefix → `EmptyFile`
     (nothing to recover: a partial trace still needs a committed header).

   A **newline-terminated** corrupt final line is genuine corruption (the write
   completed), never a torn tail, and stays a strict error in both modes. Only
   the last write can be interrupted, so there is at most one torn tail; any
   earlier bad line is a committed (middle) corruption.

3. **Byte-level, so mid-UTF-8 truncation is handled.** The core takes `&[u8]`,
   not `&str`: a torn tail may be truncated inside a multibyte character. The
   committed prefix (up to and including the last `\n`) is always valid UTF-8 —
   `\n` (0x0A) never falls inside a UTF-8 multibyte sequence — so the prefix is
   decoded and parsed exactly as `read_to_string` would; the dropped tail's
   bytes need not be valid UTF-8. Non-UTF-8 in a *committed* line is corruption,
   not a torn tail, and is a loud error.

4. **The partial mark rides the store, not the `Trace`.** Store schema bumps
   v2 → v3: one additive column `partial INTEGER NOT NULL DEFAULT 0` on
   `traces`. `Store::ingest` writes `0` (complete); `Store::ingest_partial`
   writes `1`. Migration is in-place and chained (v1→v2→v3), each an
   `ALTER TABLE ... ADD COLUMN` guarded by a `pragma table_info` check so an
   interrupted migration resumes (the ADR-0025 pattern); old rows read as
   `partial = 0` (complete — pre-recovery traces were all complete); failures
   are loud; an older build refuses a v3 store loudly rather than misread it.
   `Trace` / `TraceHeader` and the v0 wire format are **untouched** —
   partiality is a store/analysis property, not wire data, so no struct-literal
   constructor across the workspace changes (contrast ADR-0025). The recovery
   result type (`RecoveredTrace`) and the tagged load type (`StoredTrace`)
   carry it instead.

5. **Determinism report: excluded from witnessing, counted, and named.**
   `determinism::report` reads `Store::load_task_all` (traces tagged with their
   partial flag), analyzes the COMPLETE traces only, and records the count of
   excluded partials on `DeterminismReport::excluded_partial_traces`. `render`
   emits one line — `partial traces excluded from witnessing (torn-tail,
   ADR-0030): N` — **only when N > 0**, so a report over a store with no
   partial traces is byte-identical to the pre-ADR-0030 output. `analyze` over
   already-loaded traces is unchanged (it treats its inputs as complete;
   `report` supplies the count). A partial trace never witnesses a signature and
   never shifts a deterministic verdict to divergent.

6. **Verification / replay never rest on a torn record.** `Store::load_task`
   returns COMPLETE traces only; `Store::load_task_all` surfaces all traces
   tagged. Because the verification harness and differential gather read through
   `load_task`, partial traces are excluded from verification evidence with no
   change to those call sites. On a store with no partials, `load_task` is
   byte-identical to before. A task with only partial traces returns an empty
   complete set (not `UnknownTask` — the task exists), so verification sees zero
   observations (Inconclusive), not torn evidence.

## alternatives considered

**Loosen the strict parser to skip bad lines by default.** Rejected — a
best-effort default is the silent misread strictness exists to prevent.
Recovery is explicit, opt-in, and its result is labelled partial.

**A `partial` field on `Trace` / `TraceHeader`.** Rejected — it is not wire
data, and (unlike ADR-0025's task I/O, which genuinely lives on the header) it
would force a `partial: false` on every `Trace { .. }` construction across
`auto-cli`, `auto-proxy`, `auto-daemon`, `auto-backend`, `auto-contract` — five
crates this change does not own. The store column + `StoredTrace`/`Recovered
Trace` wrappers keep the mark where it belongs and the blast radius at zero.

**Recover a newline-terminated corrupt final line, or several trailing lines.**
Rejected — a completed write (trailing `\n`) that is corrupt is real
corruption, not a torn tail; and only the final write can be interrupted, so
"several torn trailing lines" cannot arise. Exactly one unterminated final line
qualifies.

**Silent exclusion of partials from the report / verification.** Rejected by
the honesty norm — "never silently thinner evidence". The report counts and
names excluded partials; verification's exclusion is visible (an empty complete
set → Inconclusive, not a fabricated pass). The one remaining honesty
enhancement — a per-scope Unchecked *note* naming partials that a task/span
scope would have matched — lives in the harness (see consequences).

**A `Recovered`/`Partial` variant on `Trace` via an enum.** Rejected — same
constructor churn as a field, plus every existing `match`/field access would
need updating. A wrapper at the parse and load boundaries is additive.

## consequences

- The S1 crash-artifact gap closes for the common case: a hard kill mid-write
  now costs one interrupted record, not the whole run, when a caller opts into
  recovery. The trace is marked partial end-to-end.
- Wire format `v` stays 0 (no wire change). Store bumps to v3 with an in-place,
  resumable, chained migration; v1/v2 stores open unchanged and their rows read
  complete.
- `load_task` now means "complete traces only". Every current caller
  (`auto-contract` harness, `auto-backend` differential, the determinism
  report) gets the safe view for free; behavior is byte-identical on any store
  without partial traces.
- Remaining wiring, at the harness owner's site (this change does not own
  `auto-contract/src/harness.rs` or `auto-backend`): a per-scope **Unchecked
  note** naming partial traces whose task/span scope would have matched, so an
  operator sees evidence was thinner because of a torn record rather than
  inferring it from a lower observation count. `load_task_all` supplies exactly
  the tagged traces that note needs. Recorded in open-questions.
- CLI wiring: a `--recover-partial` flag on `auto record` (the only path by
  which JSONL files enter a store today — SDK-live emission goes straight to the
  store; `record` re-parses a `--keep-jsonl`-style file). Off by default;
  strict everywhere else.
