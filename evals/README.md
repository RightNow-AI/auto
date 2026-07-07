# evals

Contract-driven harnesses land in S2. What exists now:

- `toy-agent/` — the S1 reference task: a small instrumented agent with four
  deterministic effectful calls and one deliberately nondeterministic one
  (wall clock). `e2e.sh` records it twice and checks the determinism report's
  measured numbers (80.0% of witnessed spans deterministic, clock divergent).
  Runs in CI; no network.

No benchmark, parity, cost, or latency claim exists anywhere in this repo
until an eval run id can back it (CLAUDE.md: honesty is load-bearing).
