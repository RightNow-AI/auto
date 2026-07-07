# F6 — novelty-stream (H1, the ratchet curve; AUTO-BENCH v1 flagship)

A frozen 300-item ticket stream whose input distribution SHIFTS at known
positions. The system starts uncompiled; guards trip on novelty; deopts pay
tier-0 and are ingested; recompiles fold them back in — the thesis
prediction is a marginal-cost-per-item curve that decays in steps, each
step one compile cycle after a shift, against a flat arithmetic control.
Protocol frozen per `evals/bench/DESIGN.md` (H1); failures and refusals are
results, not noise.

**No file here makes a paid call by itself.** The driver shells out to
`auto record` / `auto run --tier0 frontier:…` / the recompile pipeline, all
spend-capped and ledgered (ADR-0010). The orchestrator fires the leg and
owns the cap. Everything CI-runnable (`--dry-run`, `--control`,
`summarize.py`) is offline by construction.

## the frozen stream (`stream.jsonl`)

300 lines `{"pos": N, "category": C, "ticket": T}` — `category` is the
designed segment label for analysis only; the model never sees it and it is
never used as grading truth.

| segment | mix | novelty injected |
|---|---|---|
| 1–49 | billing / bug / feature (18 distinct texts, recycled) | the base pool; repetition from the start — the ratchet pays once per distinct thought |
| 50–119 | old mix + **security** | 12 new texts, first at pos 50, all first-appear by 85 |
| 120–199 | old mix + **onboarding** | 12 new texts, first at pos 120, by 160 |
| 200–300 | old mix + **billing-fraud phrasing** | 14 new texts, first at pos 200, by 245 — a *phrasing* shift over the billing domain (fraud vocabulary), testing lexical-guard trips even where the topic overlaps |

56 distinct texts, every one recurring (min 2, mean ~5.4 occurrences);
after each shift the mix keeps the old categories — novelty is a fraction,
not a wholesale swap. Generated once (seed 20260705), committed, FROZEN:
regenerating it is a protocol deviation and must be logged as one.
Intra-category texts deliberately share vocabulary (billing: invoice /
charged / refund; security: login / password / two-factor …) so the
trigram guard's leave-one-out calibration is informative and cross-category
novelty actually trips (spec/runtime.md §2: disjoint witnesses calibrate a
guard that admits everything).

## identity — why the contract says `ticket-triage`, not `bench-f6`

One identity must hold through the whole loop (all verified by reading the
code, references in `stream.contract.toml`):

1. bootstrap recordings reuse `evals/ticket-triage/agent.py` VERBATIM (no
   second paid agent exists in this family) → traces carry
   `task="ticket-triage"`, span `model_call("triage")`;
2. compile/distill gather witnesses by `store.load_task(contract.task)` +
   span kind/name filter;
3. `auto run` deopt-ingestion labels the synthetic observation trace with
   the **artifact manifest's** task/scope, which the emit gate copied from
   the contract.

So the recompile contract MUST carry `task = "ticket-triage"`, scope span
`model_call` / `"triage"` — a `bench-f6`-named contract would gather zero
witnesses and orphan every deopt ingest. The F6/bench identity lives in the
spend-ledger **session** (`bench-f6`, DESIGN.md per-family sessions), the
file names, and the artifacts. (Deviation from DESIGN's shorthand "task
bench-f6, span model_call/classify", forced by measured ingestion
semantics; recorded here and in the final report.)

## why the default recompile is `distill` (tree), not `compile --synth enum`

The enumerative DSL is a straight-line pipeline with **no input-equality
branching** (`crates/auto-dsl/src/lib.rs`): it cannot express "this ticket
→ billing, that ticket → bug" for many distinct tickets — `ConstOut` is
only proposed when every observed output is identical. On this behavior
class `compile --synth enum` refuses honestly every cycle and the curve
never leaves bootstrap. `auto distill` with the frozen char-trigram tree
trainer is the pass that measurably reached 60/60 differential parity on
real-LLM triage labels (wave 3, `evals/ticket-triage/README.md`), gated by
the SAME emit gate. The driver's `--recompile-cmd` template keeps this an
operator choice; the default is the honest one.

## the arithmetic control (read before quoting the ratio)

`auto run` **requires `--artifact`** — there is no artifact-less pure
tier-0 invocation in the CLI — so a live "same stream, pure frontier"
second pass does not exist without writing new paid logic, which this
family refuses to do. The control is therefore ARITHMETIC, from measured
reality: each position is priced at the mean measured tier-0 cost of its
distinct ticket (its bootstrap/deopt purchases in the ratchet leg); a
ticket never paid there (only possible via a guard false-proceed on its
first appearance) is priced at the global mean paid cost and counted in an
`estimated` column. `driver.py --control` writes it as a CSV;
`summarize.py` derives the same rule when no control CSV is given. It is
labeled arithmetic everywhere it appears.

## files

- `stream.jsonl` — the frozen stream (above).
- `stream.contract.toml` — the recompile contract: committed BEFORE any
  compile; thresholds declared from the task definition
  (`differential_min_agreement_milli = 950`, `one_of` over the 6 labels,
  `max_latency_ms_p95 = 30000`); ONE exact example whose `output` is a
  marked `<FROM-RECORDED-REALITY>` placeholder the orchestrator fills from
  the FIRST recording of stream pos 1. The driver refuses to start a real
  leg while the placeholder is present.
- `driver.py` — walks the stream (details in its docstring): bootstrap via
  `auto record` until the first gate PASS, then guarded `auto run` with
  `--tier0 frontier:<model>`; recompile subprocess after every K new
  distinct inputs (default 8); per-position CSV + recompile-events CSV.
  Costs: bootstrap = the recorded span's reserved `cost_usd_micros` attr
  (sqlite read); deopt = session-scoped spend-ledger delta
  (`$AUTO_SPEND_LEDGER` or `~/.auto/spend.jsonl`); tier-1 = 0 marginal.
  Halts loudly on cap/key refusals (systemic); logs and continues past
  everything else. No resume mode: a halted leg reruns from a fresh store —
  that IS the honest rerun.
- `summarize.py` — CSV → markdown: window-25 means (cost, latency, tier-1
  hit %), shift annotations, ASCII cost bars + sparkline, recompile events,
  totals (ratchet vs arithmetic control + ratio), honesty notes.

## gates (offline; run these before firing anything)

```sh
python -m py_compile evals/bench/f6-stream/driver.py evals/bench/f6-stream/summarize.py
python evals/bench/f6-stream/driver.py --dry-run \
  --store target/f6-dry/store.db --artifact target/f6-dry/live.cbin \
  --cap 0 --csv target/f6-dry/ratchet.csv
python evals/bench/f6-stream/driver.py --control \
  --from-csv target/f6-dry/ratchet.csv --store x --artifact x --cap 0 \
  --csv target/f6-dry/control.csv
python evals/bench/f6-stream/summarize.py --csv target/f6-dry/ratchet.csv \
  --control-csv target/f6-dry/control.csv --out target/f6-dry/summary.md
```

The dry run is a labeled FAKE (stdout, CSV header, summary banner all say
so): deterministic fake tiers and pinned fake prices, zero binaries, zero
spend — it exists so the whole CSV→events→summary pipeline is inspectable
before a dollar moves.

## orchestrator runbook (the real leg)

From the repo root, Git Bash, relative paths only (msys mangles
colon-bearing absolute paths inside embedded argv). `OPENAI_API_KEY` via
env or repo `.env`. Expected spend ≈ 60–120 tier-0 calls ≈ $0.01
(DESIGN.md pre-registration); cap suggestion below is the ceiling, not the
target.

```sh
# 0. build once; gates green
cargo build --release -p auto-cli

# 1. seed the contract example from recorded reality (ONE paid call):
mkdir -p target/f6
target/release/auto record --store target/f6/f6.db -- \
  python evals/ticket-triage/agent.py "I was charged for two seats on this month's invoice but our workspace only has one member, please refund the extra seat."
#    stdout prints label=<answer>; put that answer into stream.contract.toml
#    replacing <FROM-RECORDED-REALITY>, commit the fill as the marked TODO
#    completion. Then DELETE the seed store (the leg must start empty):
rm target/f6/f6.db

# 2. the ratchet leg (the flagship; every paid call capped + ledgered)
python evals/bench/f6-stream/driver.py \
  --auto target/release/auto.exe \
  --store target/f6/f6.db \
  --artifact target/f6/live.cbin \
  --contract evals/bench/f6-stream/stream.contract.toml \
  --session bench-f6 --cap 1.00 \
  --csv paper/evidence/f6-ratchet.csv

# 3. census over the grown store (offline)
target/release/auto report --task ticket-triage --store target/f6/f6.db
target/release/auto verify --contract evals/bench/f6-stream/stream.contract.toml \
  --store target/f6/f6.db --runs-dir evals/runs
#    NOTE: trace-mode verify checks one_of over EVERY stored span — an
#    off-vocabulary tier-0 answer makes this verify FAIL. That is a result;
#    report it, do not tune the contract (anti-gaming).

# 4. control + summary (offline, arithmetic)
python evals/bench/f6-stream/driver.py --control \
  --from-csv paper/evidence/f6-ratchet.csv --store x --artifact x --cap 0 \
  --csv paper/evidence/f6-control.csv
python evals/bench/f6-stream/summarize.py --csv paper/evidence/f6-ratchet.csv \
  --control-csv paper/evidence/f6-control.csv --out paper/evidence/f6-summary.md

# 5. probes: the artifact generations are in evals/bench/f6-stream/artifacts/
#    (H4's OOD probes live in their own family legs; F6's events CSV +
#    per-position guard distances are its contribution to that story)
```

## honest limits (stated up front)

- **Tier-0 deopt answers are unverified reference authority** and the
  deopt prompt (built from the manifest identity, `auto-runtime/src/tier0.rs`)
  names no label vocabulary — the model may answer outside the declared
  six labels or with different casing. The next recompile will faithfully
  reproduce whatever it said (differential is byte-exact vs the witness);
  the `one_of` property surfaces the drift at trace-mode verify time as an
  honest FAIL. The curve measures COST decay; answer correctness under
  shift is H4's measurement.
- The contract's single example binds the gate to the pos-1 ticket's first
  recorded label; if later witnesses of that ticket diverge and
  `most-common` flips the majority, every subsequent recompile refuses —
  visible in the events CSV, honest, and unlikely (F1 measured this span
  100% deterministic at n=60).
- tier-1 latency in the CSV is one-shot `auto run` wall time (spawn + wasm
  compile included), the deployment-shaped number; the resident runner
  (spec/runtime.md §9) is the measured-elsewhere upgrade.
- Guard trips/proceeds are lexical (trigram distance). Early generations
  (fewer than ~19 witnesses at alpha 0.1) calibrate to the leave-one-out
  max — the conformal quantile only bites as the witness set grows
  (spec/runtime.md §2).
