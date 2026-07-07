# AUTO-BENCH v1 — the compilation benchmark (design, frozen before any run)

Every existing agent benchmark asks *how smart is the model*. This one asks
the question the thesis lives or dies on: **does the system ever pay for the
same thought twice?** It measures a property no capability eval touches —
whether an agent system can convert its own novel cognition into permanent,
verified, near-free skill, autonomously, while *knowing what it doesn't
know*. That property — cumulative, self-compiling competence with calibrated
abstention — is the honest core of what people point at when they say a
system behaves like a general intelligence, and it is measurable.

Design is frozen BEFORE execution. Protocol deviations get logged, never
silently absorbed. Every number in the results carries an eval-run id, a
ledger line, or both. Failures and refusals are results, not noise.

## the four headline measurements

**H1 — the ratchet curve (the flagship).** A task stream where the input
distribution SHIFTS at known points (new ticket categories, new document
shapes, new tool targets). The system starts fully interpreted (tier-0
frontier). Guards trip on novelty → deopt → trace → recompile → tier-1.
Plot, per stream position: marginal cost per task, latency per task, tier-1
hit fraction. The thesis prediction: **cost decays toward $0 in steps, each
step following a distribution shift by exactly one compile cycle; nothing is
ever figured out twice.** The control: the same stream run pure-frontier
(flat cost line). The gap between the curves is the entire economic claim,
in one chart. No agent benchmark measures this today.

**H2 — the determinism census.** Across ≥5 task families (below), the
witnessed-deterministic fraction of real frontier-agent behavior at default
temperature, per effectful span, ≥2 witnesses each. This is T1 at benchmark
scale: the measured claim that most agent cognition is secretly symbolic.
Reported per family AND pooled, with span counts — never extrapolated.

**H3 — parity-gated compression.** Per family: frontier baseline (cost µ$,
latency p50/p95) vs compiled artifact (same inputs), at MEASURED parity —
byte-equal differential where the behavior is deterministic, judged
differential at a declared agreement threshold where it is generative
(ADR-0021), with the judge itself capped and ledgered. Families whose
residue does NOT reach the declared threshold are reported as honest
refusals — a benchmark that cannot fail is not a benchmark.

**H4 — calibrated ignorance.** The property that separates this from a
lookup table: on held-out in-distribution inputs vs out-of-distribution
probes, per guard wire (v1 Jaccard, v2 cosine, both alphas): false-proceed
rate (silent wrongness — the cardinal failure), false-abstain rate (wasted
deopt), and the deopt-answer correctness after tier-0 fallback. A compiled
system is only trustworthy if a wrong "stay compiled" decision is rare and
measured.

## task families (each: record → census → compile → verify → serve)

| family | behavior class | exists | what it stresses |
|---|---|---|---|
| F1 ticket-triage | single-call classify | evals/ticket-triage | the 100% deterministic case; max compression |
| F2 inbox-agent | 3-call pipeline + tool + glue | evals/inbox-agent | multi-step regions, capability confinement, the 20% generative residue |
| F3 field-extraction | structured extraction (email → typed fields) | NEW | the "most agents are secretly parsers" claim, head-on |
| F4 policy-routing | decision over rules + thresholds | NEW (distill-agent grown) | distillation (tree/mlp), weighted witnesses, imbalanced classes |
| F5 summarize-strict | free-text under judged differential | evals/inbox-agent residue | the generative frontier: judged parity or honest refusal |
| F6 novelty-stream | F1∪F3 with 3 scheduled distribution shifts | NEW (harness) | H1 — the ratchet curve, daemon-driven, no human in the loop |

Corpus rules: ≥40 distinct inputs per family, ≥2 recording passes (witness
requirement), inputs designed once and frozen in this repo before any
compile is attempted. F6's shift schedule is frozen here: shifts at stream
positions 50, 120, 200 (novel categories injected); stream length 300.

## controls and anti-gaming rules

- The frontier baseline and the compiled run consume the SAME frozen inputs.
- No contract may be tuned after seeing its gate verdict (contracts are
  committed before the first compile; amendments = logged protocol
  deviations).
- The gate is the arbiter: no artifact ships a number unless its contract
  passed (or its refusal is the reported number).
- Judged thresholds declared in the contract BEFORE runs (id-bearing,
  ADR-0018/0021); the judge model named; judge spend ledgered under its own
  session.
- Abstentions are never dropped from latency/cost aggregates: an abstained
  call's cost is its deopt cost (or its refusal, if no tier-0).
- Every paid call rides the ADR-0010 rails; per-family session names
  (`bench-f1`…`bench-f6`); hard cap for the whole benchmark: **$5.00** of
  the owner's $25 authorization (measured estimate below is ~$0.60; the cap
  is the ceiling, not the target).
- Seeds, model snapshot ids, and prices pinned in the results file.

## deliverables

1. `evals/bench/` — corpora (frozen inputs), contracts, `run.sh` harness
   legs per family, the F6 stream driver.
2. `paper/bench-results.md` — the four headline tables/curves, every row
   with eval-run ids + ledger references; a "failures and refusals" section
   with equal typographic weight to the wins.
3. `paper/evidence/` — content-addressed eval runs + the F6 per-position
   CSV (cost, latency, tier, guard decision per stream item).
4. Reproduction: one command per family + documented cap; a stranger with
   an API key and $5 reproduces every number.

## spend estimate (pre-registered)

F1/F3/F4: ~80 calls each recording (2 passes × 40) ≈ 3 × 80 × ~60µ$ ≈ $0.015
F2: 40 runs × 3 calls × 2 passes ≈ 240 calls ≈ $0.020
F5: judged differential ≈ ≤40 judge calls ≈ $0.004
F6: 300-item stream, tier-0 share ≈ 60–120 calls ≈ $0.010; recompiles $0
Baselines (frontier side of H3): ≈ re-uses recordings (recorded = baseline).
Total ≈ **$0.05–0.10**; cap $5.00. All ledgered.

## what this benchmark does NOT claim

- Not a capability eval of the underlying model; the frontier model is the
  reference interpreter, not the subject.
- Not generalization beyond witnessed distributions: guards abstain there
  BY DESIGN, and H4 measures exactly that boundary.
- Corpora are designed (realistic but synthetic); the protocol is built so
  any operator can rerun it on their own recorded traffic — that rerun, not
  this corpus, is the production claim.
