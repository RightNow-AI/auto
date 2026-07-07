# AUTO-BENCH F4 — policy-routing (decision over rules + thresholds)

F4 of the compilation benchmark (evals/bench/DESIGN.md). The reference agent
makes ONE REAL frontier call per run (gpt-5.4-mini, `agent.py`) that routes a
purchase-approval request into **approve | review | reject**, and records the
call's own measured usage on the `route` span via the reserved attrs
(spec/trace.md §3). Auto records it, runs the determinism census, and
**distills** it — because the routing rule is a decision over amount
thresholds and vendor tiers, which the 17-op extraction DSL cannot express (no
arithmetic, no threshold comparison, no branching; spec/synthesis.md §2). So
`auto compile` refuses honestly and distillation (tree/mlp; spec/distillation.md)
is the compile path that works.

What F4 stresses (DESIGN.md task table): **distillation** (the tree/mlp rung),
**weighted witnesses** (ADR-0031), and **imbalanced classes** — the corpus is
deliberately skewed, so majority-vote and witness-mass training can diverge and
both are reported.

## the policy and the corpus

The system prompt states this policy VERBATIM (see `agent.py`):

- **APPROVE** if amount < $500, OR if amount < $5000 and the vendor is a
  **preferred** vendor.
- **REJECT** if amount >= $20000, OR the vendor is a **blocked** vendor.
- Otherwise **REVIEW**.

`corpus.jsonl` is 40 FROZEN requests (`{"id","request"}` only), each naming an
amount in USD, a department, and a vendor tier (preferred | standard | new |
blocked). Amounts and wording vary so the rule is not a template a
straight-line DSL could pattern-match — the threshold logic already puts it out
of DSL reach; the varied phrasing seals it.

**Intended class skew (deliberately imbalanced):**

| class | count | region |
|---|---|---|
| approve | 24 | amount < $500 (any non-blocked tier), or < $5000 & preferred |
| review  | 10 | $500 <= amount < $20000, not caught by the approve/reject rules |
| reject  |  6 | amount >= $20000 (3), or blocked vendor with amount >= $500 (3) |

`validate_corpus.py` re-derives each request's intended label and **asserts**
this 24 / 10 / 6 split (the class-skew gate). Its policy resolves the one
genuine overlap in the literal text — amount < $500 AND a blocked vendor — by
letting **rejects take precedence**, and it asserts that NO corpus request
falls in that overlap region. Because the corpus avoids it, the intended label
is unambiguous under either reading of the policy's clause ordering.

The intended labels are used ONLY to verify the authored skew. The RECORDED
labels come from the model and are what the contract and the differential gate
actually judge. F4 is expected to be **high-but-not-perfect** deterministic;
that residue is exactly why `differential_min_agreement_milli = 900` is declared
(below), not an exact gate.

## anti-gaming: contract committed before any compile

`routing.contract.toml` is committed NOW, before the first recording, with
every threshold declared from the task definition:
`one_of {approve, review, reject}`, `[acceptance]
differential_min_agreement_milli = 900`, `max_latency_ms_p95 = 30000`. The two
`match="exact"` examples have FROZEN inputs (corpus lines f4-004 and f4-035)
but their **outputs are `<FROM-RECORDED-REALITY>` placeholders** — the
orchestrator fills them from the FIRST recording pass (wave-3 lesson: examples
come from recorded reality, never an author's guess). Compiling against an
unfilled placeholder fails the gate loudly, on purpose.

## files

- `corpus.jsonl` — 40 frozen requests.
- `agent.py` — one gpt-5.4-mini `route` call inside the traced closure; usage
  attrs from real usage; reply lowercased/stripped and validated against the
  three classes (an off-policy reply raises → honest error span, carrying the
  paid call's cost). Task `bench-f4`.
- `routing.contract.toml` — the emit contract (thresholds declared; example
  outputs are placeholders).
- `validate_corpus.py` — network-free class-skew + structural gate.
- `README.md` — this file.

## gates (CI-safe, no network)

```
python -m py_compile evals/bench/f4-routing/agent.py
python -m py_compile evals/bench/f4-routing/validate_corpus.py
python evals/bench/f4-routing/validate_corpus.py   # prints + asserts 24/10/6
```

Measured on this corpus: both files compile; validator prints
`approve 24 / review 10 / reject 6 [ok]` and exits 0. Everything below the gate
is OPERATOR-RUN: it spends real money and needs `OPENAI_API_KEY` (env or repo
`.env`) plus a built `auto` binary.

## orchestrator runbook (paid; live-fired by the orchestrator)

Parameters (the orchestrator sets them): `STORE` = trace store, `SESSION` =
`bench-f4` (the ADR-0010 ledger session for any auto-mediated paid leg), `CAP`
= USD spend cap for those legs (benchmark hard cap $5.00 total; F4 estimate
~$0.006 for 2x40 recordings + a few deopt calls). The agent's own recording
call reads `OPENAI_API_KEY` directly and records cost on the span attrs (the
ticket-triage direct-call pattern); the orchestrator aggregates those attrs for
the F4 spend line. Commands are POSIX sh (run under Git Bash on the Windows
host); `auto` = the built binary (`target/debug/auto` or on PATH).

```
STORE=evals/bench/f4-routing/f4.db
SESSION=bench-f4
CAP=0.25                       # ceiling for auto-mediated paid legs, not a target
AUTO=target/debug/auto

# --- 1. record 2 passes x 40 = 80 paid gpt-5.4-mini calls (H2 witnesses) ----
# extract the request strings from the frozen corpus, in order:
python -c "import json;[print(json.loads(l)['request']) for l in open('evals/bench/f4-routing/corpus.jsonl',encoding='utf-8') if l.strip()]" > f4-requests.txt
# NOTE: do NOT `set -e` this loop. A rare off-policy reply is recorded as an
# honest error span and exits nonzero; the errored group is excluded from
# witnessing and skipped by distillation — one such span must not abort a pass.
for pass in 1 2; do
  while IFS= read -r req; do
    "$AUTO" record --store "$STORE" -- python evals/bench/f4-routing/agent.py "$req" || true
  done < f4-requests.txt
done

# --- 2. census (H2): the determinism report over the route spans -----------
"$AUTO" report --task bench-f4 --store "$STORE"
# read off: effectful spans (should be 40 witnessed >=2), deterministic vs
# divergent fraction, top divergent signatures. This is the F4 row of H2.

# --- 3. fill the contract example placeholders FROM the recording ----------
# Look up the recorded output for f4-004 and f4-035 (they printed label=... at
# record time; or query the store) and replace the two <FROM-RECORDED-REALITY>
# outputs in routing.contract.toml with the recorded words (expected "approve"
# and "reject" — confirm from the recording, do not assume). Commit that edit.

# --- 4. extraction MUST refuse (the honest motivation for distillation) -----
# The DSL has no arithmetic/threshold op, so enumerative compile cannot spell
# the rule; with divergent references under the default --divergent-pick it
# refuses even earlier. Either way: nonzero exit, NO artifact. ($0, no network.)
"$AUTO" compile --contract evals/bench/f4-routing/routing.contract.toml \
  --store "$STORE" --out /tmp/f4-x.cbin --runs-dir evals/runs || echo "extraction refused (expected)"
test ! -f /tmp/f4-x.cbin

# --- 5a. distill: WEIGHTED witnesses (headline), memorization holdout 0 -----
"$AUTO" distill --contract evals/bench/f4-routing/routing.contract.toml \
  --store "$STORE" \
  --trainer "python crates/auto-passes/trainer/tree_train.py" \
  --model-kind tree --input-field request \
  --divergent-pick weighted --holdout 0 \
  --out evals/bench/f4-routing/f4-weighted.cbin --runs-dir evals/runs
# reports: weighted-witness row/input/divergent counts; distilled train/holdout
# accuracy + classes; weighted_train_accuracy over total witness weight; and
# the gate verdict (PASS iff differential agreement >= 900 milli on the
# distinct witnessed inputs — divergent groups count against it, ADR-0018/0031).

# --- 5b. CONTROL: most-common witness (majority vote), same holdout 0 -------
"$AUTO" distill --contract evals/bench/f4-routing/routing.contract.toml \
  --store "$STORE" \
  --trainer "python crates/auto-passes/trainer/tree_train.py" \
  --model-kind tree --input-field request \
  --divergent-pick most-common --holdout 0 \
  --out evals/bench/f4-routing/f4-mostcommon.cbin --runs-dir evals/runs
# REPORT BOTH verdicts + both agreement rates. Per ADR-0031 the two agree per
# group and differ only where trigram features collide (one leaf holding
# several groups): most-common maximizes group-vote agreement, weighted
# maximizes witness-mass agreement. State the measured difference — including
# "no measurable difference on this corpus" if the artifacts are identical.

# --- 5c. generalization attempt: holdout 0.25 (verdict either way) ---------
# --min-holdout-accuracy 0 lets the tree emit and delegates acceptance to the
# real gate (differential + the declared 900 threshold — the sole authority,
# spec/distillation.md §5); holdout_accuracy is then reported as PROVENANCE.
"$AUTO" distill --contract evals/bench/f4-routing/routing.contract.toml \
  --store "$STORE" \
  --trainer "python crates/auto-passes/trainer/tree_train.py" \
  --model-kind tree --input-field request \
  --divergent-pick weighted --holdout 0.25 --min-holdout-accuracy 0 \
  --out evals/bench/f4-routing/f4-gen.cbin --runs-dir evals/runs
# Report holdout_accuracy (does a tree trained on 75% reproduce the held-out
# 25%?) AND the emit verdict, whatever they are. With the DEFAULT floor (1.0)
# a sub-100% holdout instead surfaces as an honest trainer refusal (exit 2) —
# that refusal is itself a valid recorded result (cf. ticket-triage at n=60).

# --- 6. verify the recorded reference (trace mode; H3 reference side) -------
# (examples must be filled first, step 3) writes a content-addressed eval run.
"$AUTO" verify --contract evals/bench/f4-routing/routing.contract.toml \
  --store "$STORE" --runs-dir evals/runs

# --- 7. H4 probes: calibrated ignorance on the distilled artifact -----------
# The distilled artifact is guarded on its witnessed "request" inputs (v1
# Jaccard by default; add --guard-embedding at distill time for the v2 cosine
# wire). Run the 10 frozen probes below and record, per probe: guard PROCEED
# (tier-1 answer) vs ABSTAIN (exit 3), and — for OOD with --tier0 — the
# deopt-answer correctness after fallback. false-proceed on OOD is the cardinal
# failure H4 exists to bound; false-abstain on in-dist is wasted deopt.
```

### H4 probe set (frozen — none appear in `corpus.jsonl`)

**5 in-distribution held-out** (same shape, new values; guard should PROCEED →
tier-1). Expected intended labels in brackets:

1. `$180 for team notebooks, marketing, standard vendor.` [approve]
2. `$2,100 for a monitoring dashboard, ops, preferred vendor.` [approve]
3. `$1,100 for office chairs, HR, standard vendor.` [review]
4. `$33,000 for a fleet of servers, IT, standard vendor.` [reject]
5. `$800 for supplies from a blocked vendor, facilities.` [reject]

**5 out-of-distribution** (disjoint vocabulary; guard should TRIP → ABSTAIN, or
DEOPT to tier-0 when configured):

6. `Summarize the Q3 board deck in three bullet points.`
7. `What's the weather forecast for Denver this weekend?`
8. `Write a haiku about autumn leaves.`
9. `def add(a, b): return a + b`
10. `Translate 'good morning' into French.`

```
# in-distribution (expect a routed label, exit 0):
"$AUTO" run --artifact evals/bench/f4-routing/f4-weighted.cbin \
  --input '{"request":"$180 for team notebooks, marketing, standard vendor."}'

# OOD, abstain (no tier-0 → exit 3, the calibrated refusal):
"$AUTO" run --artifact evals/bench/f4-routing/f4-weighted.cbin \
  --input '{"request":"Write a haiku about autumn leaves."}'

# OOD, deopt to a spend-capped frontier tier-0 (rides ADR-0010 rails;
# session=bench-f4, capped; the deopt observation ratchets back into STORE):
"$AUTO" run --artifact evals/bench/f4-routing/f4-weighted.cbin \
  --input '{"request":"Write a haiku about autumn leaves."}' \
  --tier0 "frontier:gpt-5.4-mini" --spend-cap-usd "$CAP" --session "$SESSION" \
  --store "$STORE"
```

## honest expectations (results are whatever is measured)

- **Census (H2):** most `route` spans should witness as deterministic;
  divergent signatures (the same request labeled two ways across passes) are
  the recorded residue the 900 threshold prices in. Report per-family and pool
  into H2.
- **Distill (H3):** with `--holdout 0` the tree memorizes the witnessed
  labels; the emit verdict is PASS iff agreement >= 900 milli. Weighted vs
  most-common: report both; a measurable difference requires trigram-feature
  collisions across differently-labeled inputs (ADR-0031) and may not occur on
  40 lexically-distinct requests — say so if it does not.
- **Generalization:** a decision boundary that is fundamentally arithmetic
  (an amount threshold) is not well captured by axis-aligned splits over
  character trigrams; holdout 0.25 is expected to fall short of 100% — the
  guard + ratchet, not tree generalization, are the coverage story (same lesson
  as ticket-triage / inbox-agent). Whatever the number, it is reported as
  provenance, never as the gate.
- **H4:** in-distribution probes should PROCEED and route; OOD probes should
  ABSTAIN (low false-proceed). Every abstained call's cost is its deopt cost
  (or its refusal, if no tier-0) — never dropped from aggregates (DESIGN.md).

## upgrade path (recorded, not claimed here)

`--model-kind mlp` (single-hidden-layer relu MLP over the same frozen trigram
features; spec/distillation.md §8) is available for residue a single tree's
splits underfit; it trains on Modal (torch, GPU-optional) and faces the
identical emit gate. The headline and CI path for F4 is the **tree**. Class
balancing (as opposed to witness weighting) is a deliberately different,
still-open choice (ADR-0031 alternatives; spec/adr/open-questions.md).
