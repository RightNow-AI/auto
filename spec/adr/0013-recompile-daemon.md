# ADR-0013: the recompile daemon — the ratchet as a service

status: accepted · scope: `crates/auto-daemon` (build-out over the frozen seam), `spec/runtime.md` §4

## context

The ratchet — *nothing figured out twice* — has a closing step. `auto run`
deopts to tier-0 on a novel input and ingests the answer as a new observation
(spec/runtime.md §4); something must then recompile so that once-novel input is
answered on tier-1. In v0 that step was **manual**: an operator (or the e2e)
re-ran `auto compile` / `auto distill` by hand, and no process watched the
store. The constitution's runtime recompiles on its own (CLAUDE.md: "guard trip
→ deopt → capture trace → recompile … nothing figured out twice"). This ADR
records the daemon that automates the close — the v0 floor of that ratchet: a
single-node poller over one contract scope.

The frozen seam (`DaemonConfig`, `DaemonError`, committed earlier this wave)
fixed the surface: the `{out}` placeholder, `once` mode, and the five honest
failure kinds (`Store`, `Contract`, `Recompile`, `NoArtifact`, `Publish`). This
ADR records the build-out decisions over that seam.

## decision

1. **The recompile is the real pipeline as a subprocess — never a
   reimplementation.** The daemon shells out to an operator-configured argv
   (`recompile_argv`), which is the real `auto compile` / `auto distill`. The
   **emit gate runs inside that subprocess**, so the daemon can only publish an
   artifact the gate already passed; it never decides parity itself. This is
   the load-bearing choice: the gate must have exactly one implementation
   (alternatives).

2. **Watch = the distinct-input count of the contract's span scope.** The
   signal is exactly `auto_backend::differential::gather_observations(store,
   contract).groups.len()` — the *same* distinct-input count the emit gate's
   differential pass grades a recompile against, so what the daemon watches and
   what a recompile is judged on are one number. An empty store (no traces yet
   for the task) is **zero, not an error** — the daemon may be started before
   any deopt lands. A non-span scope (whole-task, or a region unverifiable
   against traces) cannot be watched and is surfaced as a `Contract` fault; the
   frozen error set has no dedicated "unwatchable contract" variant.

3. **Recompile when the count grows past an in-memory watermark.**
   `should_recompile(last, current) = current > last.unwrap_or(0) && current >
   0`. A fresh daemon (`last = None`) treats the first nonzero count as
   recompile-worthy — the operator started it to get an artifact from the
   evidence already present. The watermark is **process memory only**: a
   restart re-observes `None` and recompiles once redundantly. That is stated
   and harmless (decision 4).

4. **Publish only what the gate emitted; content-addressing makes redundancy
   harmless.** Each cycle substitutes `{out}` with a fresh temp path, runs the
   command, confirms the artifact was written, and `Registry::add`s it. Because
   `add` is content-addressed, a redundant recompile of unchanged evidence
   emits byte-identical bytes and is a **registry no-op** — so the one
   redundant recompile after a restart, or a spurious poll, costs nothing but
   CPU. A missing `{out}` placeholder is refused **before** anything is spawned;
   a nonzero exit fails the cycle with a bounded stderr tail (≤400 chars); a
   command that exits 0 but writes nothing is `NoArtifact`. Every failure path
   publishes nothing.

5. **`once` mode and a poll loop; process kill is the stop.** `once = true`
   runs exactly one cycle (deterministic for tests, scriptable for e2e legs).
   Otherwise the daemon loops — cycle, advance the watermark if it recompiled,
   sleep `poll_interval_ms` — threading the in-memory watermark. There is **no
   clean-shutdown path** in v0: process kill is the stop. A cycle error
   propagates and stops the daemon (fail loud), rather than retry into a storm.

6. **Unsigned publish in v0.** The daemon holds no signing key, so it
   `add`s unsigned (`Registry::add(path, false)`); signing would need a key in
   the registry (`add(_, true)` is `NoKey` without `keygen`). Signing is an
   operator step (`registry keygen` then a signed republish) or future work —
   the daemon does not silently run unsigned where a signature was expected; it
   simply never signs in v0.

## alternatives considered

**Library-linking the emit gate.** Call `verify_and_emit` directly instead of
shelling out. Rejected: that logic lives in `auto-cli` (the `compile`/`distill`
commands), not a reusable library. Pulling it into the daemon would **fork THE
gate** — two implementations of the one thing that must never diverge, exactly
the correctness surface the constitution guards. Shelling out to the real
binary keeps a single gate; the cost is a subprocess and an `{out}` file
contract, which is cheap and explicit.

**File-watching (inotify / `ReadDirectoryChangesW`) instead of polling.** React
to store writes rather than poll. Rejected for v0: the store is sqlite (WAL), so
a file-watch fires on every internal write, not on distinct-input *growth* —
we'd re-count on each event anyway. Polling the count directly is simpler and
the count is the real signal. Event-driven watching is an efficiency upgrade,
not a correctness one.

**A persistent (on-disk) watermark.** Would avoid the single redundant recompile
after a restart. Rejected for v0: content-addressing already makes that
recompile a registry no-op (decision 4), and a persistent watermark adds a state
file to keep coherent with the registry and the store. Additive, unneeded.

**Log-and-continue on a cycle error (with backoff).** Keep the daemon alive
through a failed recompile. Rejected for v0: a gate that keeps blocking (e.g. a
new observation the contract cannot satisfy) would spin a tight failure loop and
bury the error. Failing loud and stopping is the honest v0 behavior; supervised
continue-with-backoff is a recorded upgrade.

## consequences

- The ratchet closes with no human in the loop: deopt ingests, the daemon
  recompiles through the real gate and publishes, and the next run of that input
  answers on tier-1.
- The daemon never reimplements the gate and never publishes an ungated
  artifact; a failed recompile publishes nothing and stops the daemon.
- No CLI wiring yet: `auto-daemon` is a library with a `daemon()` entry point;
  there is no `auto daemon` subcommand, so `spec/runtime.md` §7 still lists "no
  auto-recompile daemon" for the *CLI/runtime surface*. Wiring the subcommand, a
  persistent watermark, file-watching, signed publish, and continue-on-error are
  recorded upgrades.

## sources

Internal, all read for this change:

- `spec/runtime.md` §4 (the ratchet and the manual-recompile gap this closes),
  §7 (the CLI-surface limits list).
- `crates/auto-backend/src/differential.rs` — `gather_observations`, the
  distinct-input grouping the watch count reads.
- `crates/auto-registry/src/lib.rs` — `Registry::open` / `add`, the
  content-addressed store and unsigned-add / `NoKey` behavior.
- `crates/auto-cli/src/main.rs` — `ingest_deopt_observation`, the synthetic
  single-span observation shape the daemon watches for.
- `crates/auto-serve/src/api.rs` tests — the ECHO artifact fixture (module bytes
  stored verbatim; the registry validates the container + manifest and never
  compiles the module) mirrored by `crates/auto-daemon/tests/daemon.rs`.

The `std::process::Command` (stdout/stderr capture + wait-for-exit) and
`std::thread::sleep` behavior the daemon relies on is exercised directly by the
crate's own tests (nonzero-exit stderr capture, missing-artifact detection), not
taken on faith — so no external citation is load-bearing here.

## amendment — wave 5

status: accepted · scope: `crates/auto-daemon` (two of the
recorded upgrades from the accepted body: a persistent watermark and
supervised continue-on-error).

The decisions above stand. Wave 5 builds two upgrades this ADR itself recorded
as future work ("A persistent (on-disk) watermark" and "Log-and-continue on a
cycle error (with backoff)" under *alternatives considered*), plus the error
refinement each needs. Both are **additive** and **default to the wave-4
behavior** — `DaemonConfig` gains two fields, `watermark_path: Option<PathBuf>`
(default `None`) and `supervise: bool` (default `false`), and with both at their
defaults every wave-4 semantic is byte-identical (the CI e2e, which passes
neither, is unchanged).

### 7. Persistent watermark (`watermark_path`, optional)

`None` keeps decision 3's **in-memory** watermark exactly. `Some(path)` makes
the last-compiled distinct-input count survive a restart:

- **read once at startup.** Missing file → a fresh start (`None` watermark,
  decision 3's first-nonzero-count behavior). A present file is parsed strictly
  (`{"watermark_version":0,"last_compiled_count":<usize>}`, unknown fields
  rejected); anything unreadable, off-schema, or of an unknown version is a
  **loud** `DaemonError::Watermark`, never a silent fresh start. This direction
  is deliberate: reading a corrupt watermark as "fresh" is harmless (one
  redundant recompile), but reading it as some *wrong count* would silently skip
  real recompiles — the one failure the ratchet must never make — so a
  watermark we cannot fully trust stops the daemon instead of guessing.
- **written after every publishing cycle** (both `once` and loop), and only
  then — a no-op cycle writes nothing.
- **atomicity guarantee.** The write is *temp-file-then-rename*: bytes go to a
  sibling `<path>.tmp-<pid>-<n>` in the same directory, then `std::fs::rename`
  moves it over the target. Rename is the single commit point — atomic within a
  directory on both platforms (POSIX `rename`; Windows `MoveFileEx` with
  `REPLACE_EXISTING`, which `std::fs::rename` uses) — so a crash mid-write
  leaves either the old file or the complete new file, **never a torn read**. A
  failed rename removes the temp file and reports the error; the guarantee is
  the no-torn-file invariant, not that the temp file survives a power cut. This
  is why decision 4's "content-addressing already dedupes the one redundant
  recompile" is not a reason to skip the state file *when the operator opts in*:
  the redundant recompile is only free on unchanged evidence, and a real
  restart-heavy deployment would rather not re-run the gate at all.

Why still optional, not the default: the in-memory watermark remains the
honest zero-config floor (a restart's one redundant recompile is a registry
no-op, decision 4). The persistent file is opt-in state the operator chooses to
keep coherent with the store and registry.

### 8. Supervised mode (`supervise`, opt-in), and the retryable/config error split

`false` keeps decision 5's fail-loud loop: any cycle error stops the daemon.
`true` changes the response **by error class**: a *retryable* error is logged
and retried after a backoff; a *config-shaped* error still stops the daemon
loudly. The split is the load-bearing part — decision 5 rejected blanket
continue-on-error precisely because "a gate that keeps blocking would spin a
tight failure loop and bury the error." Retrying only the errors that a *later
cycle could plausibly clear* keeps that protection:

- **retryable** (external-world I/O; the next cycle may differ): `Store` (the
  store file/contents change), `Recompile` (a real spawn failure or nonzero gate
  exit — a flaky or resource-starved gate may pass later), `NoArtifact` (the
  command may write next time), `Publish` (a transient registry/filesystem
  fault).
- **not retryable** (config-shaped; recurs *identically* regardless of world
  state, so a retry is the exact tight useless loop decision 5 warned about):
  `Contract` (the contract will not parse / is an unwatchable scope), `Config`
  (the recompile argv is misconfigured), `Watermark` (the state file is
  corrupt). These stop the daemon even under `supervise`.

Backoff is `poll_interval_ms · 2^consecutive_failures`, capped at 60_000 ms and
reset to the normal interval on the next success — computed by a pure
`next_delay(consecutive_failures, base, cap)` (saturating, so no overflow) that
is unit-tested independently of the loop. The loop itself is exercised by a
bounded test hook: the poll loop is factored into a `pub(crate)` `run_loop(…,
max_cycles)` (production passes `None` = unbounded; tests pass `Some(n)` to run
exactly `n` cycles), so a "fail twice then succeed" recompile is tested end to
end without an infinite loop. The public `daemon()` signature is unchanged.

### error-set refinement (additive)

Two additive `DaemonError` variants back the above, and one pre-existing case
is reclassified:

- **`Watermark { path, detail }`** — a corrupt/untrusted watermark file (new).
- **`Config { detail }`** — a misconfigured recompile argv (new). The missing
  `{out}` refusal, which decision 4 surfaced as `Recompile { status: "not
  started" }` only because the frozen seam then had no config variant, now
  returns `Config`. This is not cosmetic: the refusal is config-shaped (the
  argv is fixed for the daemon's lifetime), so it must sit on the *not
  retryable* side of §8 — which requires distinguishing it from a real
  `Recompile` **by variant**, not by sniffing a status string. `Recompile`
  remains for real spawn/exit failures. No variant was removed; a caller
  matching `Recompile` for the misconfig case (only this crate's own tests did)
  is updated to match `Config`.
