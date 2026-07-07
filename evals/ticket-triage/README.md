# ticket-triage — the economics demo (E1, single-call scale)

The reference agent makes ONE REAL frontier call per run (gpt-5.4-mini,
`agent.py`) to route a support ticket into billing | bug | feature, and
records the call's own measured usage on the span via the reserved attrs
(spec/trace.md §3). Auto records it, distills it, and the emitted
artifact's manifest carries the comparison the demo exists to make:

    measured: compiled p50=0ms p95=0ms max=0ms; reference recorded p95=1177ms

Measured (paper/log.md wave 3;
paper/evidence/economics-ticket-triage.json): reference p50
736ms / p95 1177ms / mean 55µ$ per call → compiled <1ms in-process, 0µ$
marginal, 60/60 differential parity, guard abstaining beyond calibration.

**This eval is operator-run, never CI-run**: every `record` spends real
money (~55µ$/call) and needs `OPENAI_API_KEY` (env or repo `.env`). The
corpus is 60 tickets; the artifact compiles to the witnessed distribution
(`--holdout 0` — generalization at n=60 on real-LLM labels measurably
fails; the guard + ratchet are the coverage story, and the paper log says
so plainly).

## protocol

```
# determinism measurement (2 witnesses per input, 20 tickets = 40 calls)
head -20 evals/ticket-triage/corpus.txt > det20.txt
for pass in 1 2; do while IFS= read -r t; do
  auto record --store det.db -- python evals/ticket-triage/agent.py "$t"
done < det20.txt; done
auto report --task ticket-triage --store det.db

# compile store (60 calls), recorded-reference verification (budgets incl.
# cost/tokens from the reserved attrs), then the gated distill
while IFS= read -r t; do
  auto record --store triage.db -- python evals/ticket-triage/agent.py "$t"
done < evals/ticket-triage/corpus.txt
auto verify  --contract evals/ticket-triage/triage-recorded.contract.toml --store triage.db
auto distill --contract evals/ticket-triage/triage.contract.toml --store triage.db \
  --trainer "python crates/auto-passes/trainer/tree_train.py" \
  --input-field ticket --holdout 0 --out triage.cbin
auto run --artifact triage.cbin --input '{"ticket":"Why did my card get billed $49 when the checkout page said $39?"}'
```

Two contracts on purpose: `triage-recorded.contract.toml` carries the
cost/token budgets (claims about the recorded reference, checked in trace
mode), `triage.contract.toml` is the emit contract (a wasm subject has no
billing; see spec/adr/open-questions.md, "Reference-side budgets vs
subject-mode emit").
