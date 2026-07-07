# ADR-0020: eval-run retention — keep-newest + manifest-pinned protection, loud deletes, own-files-only

status: accepted · scope: `crates/auto-contract`
(`evalrun::gc`, pure fs), `crates/auto-registry` (`Registry::pinned_eval_runs`,
one additive helper), `crates/auto-cli` (a `runs-gc` subcommand — wiring),
`spec/adr/open-questions.md`.

## context

Since S2 the runs directory has grown without bound: every verification writes
`<runs_dir>/<id>.json` (`id` = sha-256 of the canonical body) and nothing ever
prunes it. Recorded twice in open-questions — "Eval-run retention" (contracts)
and "GC / retention" (registry).

The forcing constraint — why this needs care, not `rm` — is provenance. A
manifest pins `eval_run_ids` as the PASS evidence that gated its emit (the
manifest is the trust layer, CLAUDE.md). Deleting a pinned run severs an
artifact from the evidence its parity claim rests on. So a retention sweep must
know what the registry protects *before* it deletes anything, and must fail
rather than guess when it cannot read that protection set completely.

## decision

1. **Keep-newest window + manifest-pinned protection.**
   `evalrun::gc(runs_dir, keep_newest, protected) -> Result<GcReport, String>`
   retains (a) the newest `keep_newest` run files by modified time and (b)
   every id in `protected`, and removes the rest. Protection outranks the
   window: a pinned run older than the window survives. `protected` is caller-
   supplied; `auto-registry::pinned_eval_runs` builds it from a live registry.
   `GcReport { removed, kept, protected_kept }` partitions the candidates seen
   (`removed + kept + protected_kept` == candidate count); `protected_kept`
   counts runs saved *only* by a pin (beyond the window), so an operator can
   see whether protection actually did work.

2. **`pinned_eval_runs` re-verifies, and refuses on any corruption.** It walks
   `Registry::list`, re-reads each artifact, re-checks the content id (the same
   content check `get` performs), parses the manifest, and unions
   `eval_run_ids`. A corrupt entry — unparseable container, content-id
   mismatch, or unreadable manifest — propagates as the usual `RegistryError`;
   there is no partial set. A protection set that silently dropped a
   corrupt-but-real manifest's pins could let GC delete a run that artifact
   still cites, so GC must never run against a registry it cannot fully read.
   Signature authenticity is deliberately out of scope here: content integrity
   is what makes a manifest's pins trustworthy, and a bad signature surfaces
   through `get`, not through the protection set (an unsigned artifact still
   contributes its pins).

3. **GC deletes only files it could have written.** Candidates are
   `<stem>.json` where `stem` is 64 lowercase-hex — the shape `digest_hex`
   emits. Everything else (other extensions, uppercase, wrong length, foreign
   names, and even a directory that happens to match the name) is left
   untouched and counted in no bucket. Retention is not a general directory
   cleaner; it prunes its own records only.

4. **Deletion and read failures are loud.** A failed unlink, an un-stattable
   file, or an unreadable directory is an `Err`, never a silent skip. A GC that
   hid a failed delete would report space reclaimed that was not, or mask a
   permissions/corruption problem the operator must see (honesty is
   load-bearing). The one benign case is a missing `runs_dir`: `Ok` with an
   all-zero report — nothing to prune is not a failure.

5. **Tie semantics: keep more, never less.** `keep_newest` is a floor expressed
   as an mtime cutoff — every file at least as new as the `keep_newest`-th
   newest is kept. When mtimes tie at that boundary all tied files are kept, so
   the kept count can exceed `keep_newest`, and the outcome does not depend on
   the order equal-mtime files are visited. A retention pass is destructive; on
   a tie it errs toward keeping, because deleting something that might be newer
   than a file it kept is the unrecoverable direction. Concretely: several runs
   written inside one coarse mtime tick are never partially collected.

6. **No clocks, no new deps.** `gc` reads modified times, sets none, and
   consults no wall clock (matching `evalrun`'s "the library reads no clocks").
   Tests stamp deterministic mtimes with std's `File::set_modified` (stable) —
   no `filetime` dependency and no sleeps, so the ordering assertions are exact
   on every platform.

## alternatives considered

**Age-based retention** (delete mtime older than a horizon `T`). Needs a clock
and a policy horizon and interacts with pinned protection the same way; the
count window is the smaller, fully-testable first cut. Left open.

**Size / count ceilings** (byte budget, or a hard max file count). A byte
ceiling needs a size scan and an eviction order beyond "oldest first"; deferred
with the age policies.

**`filetime` crate for deterministic test mtimes.** Rejected — std
`File::set_modified` covers it with no new dependency (NO new deps was a hard
constraint).

**`pinned_eval_runs` calling `get` into a scratch file.** `get` writes verified
bytes to an output path; using it only to read a manifest back would make a
read-only query write scratch files and need a scratch location (`tempfile` is
dev-only in `auto-registry`). Instead the helper performs `get`'s identical
content check in memory. The one property `get` adds — signature verification —
is intentionally not required (decision 2).

**Best-effort deletion** (skip failures, keep going). Rejected by the honesty
norm: a half-swept directory that under-reports removals is worse than a loud
stop.

## consequences

- The unbounded-growth item recorded at S2 closes for the count+pinned policy.
  Age/size policies and the two-store retention move (run lifetime tied to the
  registry) stay open — the latter tracked under the S7 registry "GC /
  retention" entry.
- Two stores must move together operationally: build the protected set from the
  *current* registry, then sweep — never against a stale set. The CLI wiring
  enforces the order (open registry → `pinned_eval_runs` → `gc`).
- Additive only: `evalrun` gains `gc` + `GcReport`, `auto-registry` gains one
  method. `EVAL_RUN_VERSION` and the manifest format are untouched — retention
  reads records, never rewrites them, so no version bump.

## amendment: age-restricted retention

The "Age-based retention" left open above lands here, as the count window's
companion rather than its replacement.

1. **Sibling function, not a new parameter.**
   `gc_with_age(runs_dir, keep_newest, protected, older_than: Option<SystemTime>)`
   carries the age bound; `gc(..)` becomes exactly
   `gc_with_age(.., None)`. The `gc` signature — and therefore every wave-8
   caller and test — is untouched, byte-identical. `GcReport` is unchanged (no
   new field, so its existing exhaustive matches and constructions still
   compile).

2. **Age RESTRICTS deletion; it never extends it.** With `older_than =
   Some(cutoff)`, a record past the keep-newest floor is removed only if it is
   *strictly* older than the cutoff (`modified < cutoff`) AND unprotected; a
   record at or newer than the cutoff is KEPT even though it is past the floor
   (ties at the cutoff keep, matching the window boundary). The removal
   predicate is `beyond_floor AND (bound_absent OR strictly_older) AND
   not_protected` — every clause can only make a deletion *less* likely. This is
   the safe direction for a destructive pass and the safe direction for
   composing two retention policies: turning the age bound on can never delete a
   record the floor+pins rule would have kept, so an operator can add
   `--max-age-days` to an existing `--keep` without auditing for new deletions.
   `--keep 0 --max-age-days D` is the pure age policy (delete everything older
   than D except pins); `--keep N` alone (no age) is wave-8 exactly.

3. **Accounting: age-kept folds into `kept`.** A record spared by age (past the
   floor, not older than the cutoff) is counted in `kept` — it was retained, and
   a pin was not needed. `protected_kept` keeps its meaning "saved *only* by a
   pin": past the floor AND old enough that age would not have kept it either.
   So a young *and* pinned record counts as `kept` (age alone would have saved
   it), which is why `protected_kept` stays an honest measure of what protection
   *did* — it never credits protection for a save age already made. The
   partition `removed + kept + protected_kept == candidate count` holds under any
   `older_than`. (Folding into `kept` rather than adding a fourth bucket is also
   what keeps `GcReport` — and the wave-8 tests that match it exhaustively —
   unchanged.)

4. **Time-dependence, and the library still reads no clock.** The cutoff is a
   wall-clock instant the CALLER supplies, exactly as `EvalRun` takes
   `created_unix_ms` from the caller (decision 6's "reads no clocks" holds:
   `gc_with_age` consults no wall clock, it only compares supplied instants to
   file mtimes). The CLI derives the cutoff from `SystemTime::now()` at
   invocation (`now - max_age_days`, saturating, clamped to `UNIX_EPOCH` so an
   absurd horizon cannot panic or wrap), so the same directory swept moments
   apart can delete different records as more of them cross the cutoff — an
   age sweep is a snapshot at its call time, not a stable function of the
   directory alone. Tests pass an explicit instant relative to a fixed base, so
   they stay deterministic with no clocks and no sleeps (still std
   `File::set_modified`, no `filetime`).

Still open after this: size / count ceilings (byte budget), and the two-store
retention move (run lifetime tied to the registry) — tracked under the S7
registry "GC / retention" entry.

## amendment: size-ceiling retention

The "Size / count ceilings" left open by the first amendment lands here, as a
byte budget composing with keep/pins/age rather than replacing any of them.

1. **Sibling function, base of the family.**
   `gc_with_limits(runs_dir, keep_newest, protected, older_than: Option<
   SystemTime>, max_total_bytes: Option<u64>) -> Result<GcSizeReport, String>`.
   `gc_with_age(..)` becomes `gc_with_limits(.., None).map(|r| r.report)` and
   `gc(..)` stays `gc_with_age(.., None)` — both signatures, and therefore
   every wave-8 / age caller and test, are untouched and byte-identical.
   `GcReport` is **unchanged** (its exhaustive matches and constructions still
   compile); the size result is a new `GcSizeReport { report: GcReport,
   kept_bytes: u64, max_total_bytes: Option<u64> }` with an `over_ceiling()`
   helper, returned only by `gc_with_limits`.

2. **The ceiling drives removal; floor, age, and pins bound what it may touch.**
   After the keep-newest floor, the age bound, and the pins decide the
   *eligible* set — non-floor, non-pinned, and (when an age bound is given)
   strictly older than the cutoff — removal is:
   - `max_total_bytes = None`: remove every eligible record. This is exactly
     `gc_with_age` (and, with `older_than = None`, wave-8 `gc`) — the None path
     is byte-identical, pinned by test.
   - `max_total_bytes = Some(ceiling)`: if the total size of the would-be-kept
     records exceeds `ceiling`, remove eligible records **oldest-first** until
     the retained total is within `ceiling` or no eligible record remains.
   So a young record (age-protected), a floor record, and a pinned record are
   **never** size-evicted. `--keep 0 --max-total-bytes B` is the pure size
   policy; `--keep N` with no ceiling is wave-8 exactly. Composition stays
   monotone in the safe direction (first amendment, decision 2): adding or
   raising a ceiling — like adding an age bound — can only KEEP more relative to
   the driver it bounds, never delete a record the floor+pins rule alone would
   have kept.

3. **Ties: whole mtime-groups, never split.** Size eviction removes eligible
   records in whole mtime-tie groups, oldest group first, checking the retained
   total before each group and stopping once within budget. A coarse mtime tick
   is therefore never *partially* collected (decision 5), and everything
   retained is strictly newer than everything the ceiling evicted — so the
   destructive pass never deletes a record the same age as one it kept. A tie
   group at the boundary is removed wholesale (the total may dip below the
   ceiling: the ceiling is a maximum, and undershooting retains *less*, the safe
   direction); the alternative — keeping a fully-removable older group while
   over budget — would contradict "remove until within the ceiling".

4. **A ceiling that cannot be met is reported, not forced.** Floor and pinned
   records are never size-evicted, so a directory whose floor+pins alone exceed
   the ceiling stays over budget. `gc_with_limits` reports this honestly:
   `GcSizeReport::kept_bytes` is the measured retained footprint and
   `over_ceiling()` is true when a ceiling was requested and `kept_bytes` still
   exceeds it. Measured honesty beats a falsely "met" ceiling (honesty is
   load-bearing) — the CLI surfaces it in the `runs-gc` message.

5. **Accounting and no-new-deps.** An age-kept or size-spared record (retained,
   not by a pin) counts in `GcReport.kept`; `protected_kept` keeps its meaning
   "saved *only* by a pin". The partition `removed + kept + protected_kept ==
   candidate count` holds under any `older_than` / `max_total_bytes`. Sizes come
   from the same `metadata` stat already used for mtimes, so the ceiling adds no
   syscalls and no dependency; the library still reads no clock (the age cutoff
   is caller-supplied, decision 6).

Still open after this: a hard max file *count* (a near-variant of the byte
budget), and the two-store retention move (run lifetime tied to the registry) —
tracked under the S7 registry "GC / retention" entry.
