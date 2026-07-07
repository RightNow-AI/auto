# evals/bench — AUTO-BENCH v1 harness

The protocol is FROZEN in `DESIGN.md` (read it first). This directory
holds the per-family legs and the collector. **No file here spends money
when CI touches it**: every paid section is gated (`RECORD=1`, `JUDGE=1`,
or a driver the orchestrator fires) and rides store/session/cap
parameters. Sessions: `bench-f1` … `bench-f6`; benchmark-wide hard cap
$5.00 (DESIGN.md).

## results-dir convention

Each leg writes `<fam>-results.json` plus raw logs/census/artifacts into
the `OUT_DIR` you pass (suggestion: one shared dir, e.g.
`target/bench/results` while iterating, `paper/evidence/bench` for the
real firing). `collect.py` reads that directory; a missing family renders
NOT RUN. F3/F4 legs are manual runbooks (their READMEs); if you want them
in the collected tables, hand-write `f3-results.json` / `f4-results.json`
in the same minimal shape (`census`, `frontier_baseline`, `compile`,
`h4_probes` — copy the keys from a generated f1 file; every number must
carry its eval-run id or ledger line, per DESIGN.md).

## run order (record → census → compile → verify → serve/probes)

| leg | corpus | script / runbook | paid gate |
|---|---|---|---|
| F1 ticket-triage | `evals/ticket-triage/corpus.txt` (first 40) | `run-f1.sh` | `RECORD=1` (~80 calls) |
| F2 inbox-agent | `evals/inbox-agent/corpus.txt` (first 20; `F2_COUNT=40` restores the DESIGN pre-registration) | `run-f2.sh` | `RECORD=1` (~120 calls) |
| F3 field-extraction | `f3-extraction/corpus.jsonl` | `f3-extraction/README.md` (manual runbook) | operator-fired |
| F4 policy-routing | `f4-routing/corpus.jsonl` | `f4-routing/README.md` (manual runbook) | operator-fired |
| F5 summarize-strict | the **F2 store** (no own recording) | `run-f5.sh` | `JUDGE=1` (judge calls) |
| F6 novelty-stream | `f6-stream/stream.jsonl` | `f6-stream/README.md` (driver runbook) | driver leg, capped |

Order matters twice: **F2 before F5** (F5 distills the F2 store's
summarize residue), and each family's **contract fill before its
compile** — `bench-f1.contract.toml` and `bench-f5.contract.toml` (this
directory) plus the F3/F4/F6 contracts carry `<FROM-RECORDED-REALITY>`
placeholders the orchestrator fills from the first recording pass and
commits. `run-f1.sh` / `run-f5.sh` print the recorded outputs at the fill
gate and refuse to compile past a placeholder.

## reproduction one-liners (Git Bash, repo root; build once: `cargo build --release -p auto-cli`)

```sh
export AUTO=target/release/auto        # scripts default to target/debug/auto
R=target/bench/results                 # or paper/evidence/bench

# F1 — record (paid), then fill bench-f1.contract.toml, then offline rerun
RECORD=1 bash evals/bench/run-f1.sh "$R" target/bench/f1.db bench-f1 0.25
bash evals/bench/run-f1.sh "$R" target/bench/f1.db bench-f1 0.25

# F2 — record (paid), offline in the same invocation
RECORD=1 bash evals/bench/run-f2.sh "$R" target/bench/f2.db bench-f2 0.25

# F3 / F4 — follow the runbooks (paid legs marked there)
#   evals/bench/f3-extraction/README.md
#   evals/bench/f4-routing/README.md

# F5 — over the F2 store: fill bench-f5.contract.toml (the script prints
# the canonical pick), then fire the judge legs
bash evals/bench/run-f5.sh "$R" target/bench/f2.db bench-f5 0.25            # (a) only, $0
JUDGE=1 bash evals/bench/run-f5.sh "$R" target/bench/f2.db bench-f5 0.25    # (a)+(b)+(c)

# F6 — the ratchet curve (driver runbook in f6-stream/README.md)

# collect: the four headline sections, paper-ready
python evals/bench/collect.py "$R" --f6-summary paper/evidence/f6-summary.md \
  --out paper/evidence/bench/collected.md
```

Notes for hand-runs: quote probe/ticket text with SINGLE quotes (several
corpus lines contain `"` and `$`); the scripts themselves pass all text
via argv/JSON, never through shell interpolation. `GUARD_ALPHA_MILLI`
(default 1 = max-quantile) sets the guard alpha on every artifact a leg
builds — for H4's "both alphas" grid rerun the offline phase with
`GUARD_ALPHA_MILLI=200` into a second OUT_DIR; keep alphas identical
across families.

## what each script measures (and its declared expectations)

- **run-f1.sh** — census (H2); frontier baseline from the recorded span
  attrs (H3 reference side); the enum rung *attempted exactly as
  specified* with its expected honest refusal recorded verbatim (the v0
  DSL has no input-equality branching — `f6-stream/README.md` documents
  the same refusal for this span), then the distill rung (the pass that
  measured 60/60 parity in wave 3) emitting v1 (Jaccard) + v2
  (`--guard-embedding`) artifacts; trace-mode verify; a 20-call warm
  `auto run` latency probe (script-timer, one-shot process wall-clock);
  H4 probes from `probes-f1.jsonl` (5 held-out in-distribution + 5 OOD;
  exit 0 = answered, 3 = abstained).
- **run-f2.sh** — census incl. per-span-name determinism (the canonical
  report aggregates per kind; the per-span groupby mirrors
  determinism.rs and says so in the json); per-run + per-span frontier
  baseline; enum-synth compiles of classify + priority against their
  committed wave-5 contracts VERBATIM (expected: constrained emits —
  each contract carries one example input outside the first-20 window;
  the recorded verdicts are the results); H4 probes from
  `probes-f2.jsonl` against whatever emitted.
- **run-f5.sh** — three sub-runs against `bench-f5.contract.toml`
  (judged differential, min 800 milli, committed pre-run): (a) weighted
  without judge → INCONCLUSIVE expected (wave 9); (b) weighted +
  gpt-5.4-mini judge → verdict + measured agreement (wave 12 measured
  85% on this store shape); (c) most-common holdout-0 control + judge
  (wave 9 measured 20/20, 0 consults). Judge spend measured from the
  ADR-0010 ledger per sub-run.
- **collect.py** — H2/H3/H4 tables + refusals section (equal weight) +
  measured spend + embedded F6 summary (H1). `--self-test` renders FAKE
  fixtures from a temp dir and prints `SELF-TEST OK`.

## gates (offline, CI-safe — run before firing anything)

```sh
bash -n evals/bench/run-f1.sh evals/bench/run-f2.sh evals/bench/run-f5.sh
python -m py_compile evals/bench/collect.py
python evals/bench/collect.py --self-test     # prints SELF-TEST OK
python - <<'PY'
import json
for f in ("evals/bench/probes-f1.jsonl", "evals/bench/probes-f2.jsonl"):
    rows = [json.loads(l) for l in open(f, encoding="utf-8") if l.strip()]
    ids = [r["id"] for r in rows]
    assert len(ids) == len(set(ids)) == 10, f
    assert all(r["kind"] in ("heldout", "ood") and r["ticket"] for r in rows), f
print("probes ok")
PY
```

## logged deviations (DESIGN.md: never silently absorbed)

1. **F1 artifacts come from the distill rung** when enum refuses (the
   measured expectation): the track spec's `--synth enum` compile is
   attempted first and its refusal recorded verbatim as the enum rung's
   H3 result; latency/H4 probes then use the distilled artifacts. If
   enum ever emits, the script uses the enum artifacts and skips distill
   (self-correcting; the json's `artifact_rung` says which happened).
2. **F1 uses `bench-f1.contract.toml`**, not the wave-3
   `triage.contract.toml` (whose feature example input lies outside the
   first-40 bench window and whose examples were filled from the
   store). Thresholds declared pre-run in the contract header.
3. **F2 records 20 tickets** (track spec) vs DESIGN's ≥40 corpus rule;
   `F2_COUNT=40` restores the pre-registration. Logged in the results
   json either way.
