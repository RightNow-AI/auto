# inbox-agent — the multi-step economics demo (E1 at agent scale)

Per run: THREE real gpt-5.4-mini calls (classify -> priority -> summarize)
plus one local tool (team lookup) and ordinary python glue. The agent is
the constant; the interpreter under it is what compiles away:

- `AGENT_MODE=frontier` (default) — real chat calls; `OPENAI_BASE`
  overrides the API base, so the SAME code records through `auto proxy`.
- `AGENT_MODE=compiled` — each model call POSTs to `auto serve`
  (`AUTO_SERVE_BASE`, `ID_CLASSIFY`/`ID_PRIORITY`/`ID_SUMMARIZE`), with
  zero glue changes.

Measured (paper/log.md wave 5; paper/evidence/
economics-inbox-agent.json): baseline mean 2,933 ms and 188 µ$
per run -> compiled mean 173 ms and 0 µ$ marginal (p50 21x, p95 40x);
determinism on identical repeated traffic: classify 100%, priority 95%,
summarize 20% (the free-text residue), 70.0% of raw frontier calls overall.

**Operator-run only, never CI** — every frontier-mode run makes three paid
calls (~190 µ$). Protocol: record 60 baseline runs (`auto record` per
corpus line), write the per-step contracts from recorded reality, `auto
distill --holdout 0` each step (summarize needs `--buckets 4096` — the
1024-bucket tree honestly fails the gate on 60 free-text classes),
`registry add` all three, start `auto serve`, re-run the corpus with
`AGENT_MODE=compiled`, and compare root-span latencies + span cost attrs
across the two stores. Full command sequence in paper/log.md wave 5.
