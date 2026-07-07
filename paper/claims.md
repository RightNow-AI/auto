# claims ledger

Status: **measured** (evidence attached) · **pending** (buildable, not yet
measured) · **hypothesis** (the thesis itself; needs real-world scale).
Every measured row names its evidence; toy-fixture rows say so.

## thesis claims

| # | claim | status | evidence |
|---|---|---|---|
| T1 | A large, measurable fraction of agent behavior is secretly deterministic ("most agent behavior is a parser") | **measured (first real-model datapoint)** — a live gpt-5.4-mini routing agent: **100.0%** witnessed-deterministic (40/40 spans, 20 tickets × 2 passes, default temperature); cross-store refinement: 59/60 tickets fully stable, one ambiguous ticket flipped 2/4 observations. Caveats: designed task, single-call agent, n=60. Toy-fixture 80.0% remains labeled inadmissible | log.md wave 3; `evidence/economics-ticket-triage.json` |
| T2 | Recorded-deterministic behavior can be compiled to verified artifacts ~100-1000x cheaper/faster than frontier interpretation at measured parity | **measured, complete ladder** — single call: ≥1177× (wave 3). Multi-step full-run: 2,933ms/188µ$ → 173ms/$0 via serve (17×, wave 5) → **0.94ms/$0 via resident runners (~3,100×, wave 7)**; per call warm p50 0.29ms. The constitution's $0.50/40s→<$0.001/<100ms target met and exceeded on this workload. All at 60/60 differential parity, guards abstaining beyond calibration; designed-corpus caveat stands | wave-3 + wave-5 evidence jsons; eval runs `d98b61b5…`/`5bc4d557…`; per-step determinism 100/95/20% |
| T3 | The compile loop closes: guard-tripped novelty deopts to tier-0, is captured, and recompiles to tier-1 (the ratchet) | **measured (toy)** | toy-agent e2e: far input trips guard → abstains exit 3 without tier-0 → deopts to pluggable oracle with `--tier0` → ingested → recompile serves it tier-1; CI-green on every merge since PR #4/#7 |

## mechanism claims (the compiler works as specified)

| # | claim | status | evidence |
|---|---|---|---|
| M1 | Verification-gated emission: a failing or unmeasurable contract blocks artifact existence | **measured** | wrong-impl blocked in toy e2e; extraction refusal in distill e2e; wave-1 MLP holdout 0.667 < 1.0 → exit 3, no artifact (log.md) |
| M2 | Differential replay: every emitted artifact reproduces every distinct recorded input exactly | **measured** | eval runs `evidence/1ed80f30….json` (36/36, local torch) and `evidence/a575b736….json` (36/36, Modal A10G cu130), commit 92d6c7b |
| M3 | Capability confinement is physical: artifacts have zero wasm imports; the loader refuses otherwise | **measured** | `module_compiles_with_zero_imports` tests across all three interpreters; loader refusal test; manifest `capabilities: none (pure)` |
| M4 | One-implementation/two-compilations: native evaluator and artifact interpreter are byte-equal on outputs | **measured** | interpreter_parity (9), model_parity (8), mlp_parity (10) test suites, CI every merge |
| M5 | Distillation acceptance cannot be gamed by hyperparameter/seed search: the gate replays all recorded inputs regardless of holdout | **measured** | wave-1: 3 winning configs found by sweep; emitted artifact still had to pass 36/36 differential (see M2) |
| M6 | Remote GPU training composes with the emit gate unchanged | **measured** | Modal A10G inside `auto distill` (`modal run -q`), torch 2.12.1+cu130, eval run `a575b736…`, artifact `a6d928f4…` |
| M7 | Determinism measurement itself (witnessed ≥2, error-disqualified) | **measured (toy)** | auto-trace determinism report unit+integration tests; toy e2e 80% figure |
| M8 | Byte-stable IR round-trip (flatbuffers, golden files) | **measured** | auto-ir proptest + goldens, CI every merge |
| M9 | Tamper evidence is structural: registry recomputes content digest on get | **measured** | registry tamper leg of toy e2e (bytes flipped → refused with id mismatch) |
| M10 | Cost/token budgets are verifiable from recorded traces without trusting the harness | **measured (mechanism)** | reserved span attrs → p95 checks, all-or-Inconclusive, malformed=Fail; 9 integration tests (PR #8); NOTE: attrs are the agent's own declaration — billing verification is an open question |
| M11 | A frontier model can propose DSL programs that the unchanged checker verifies and the unchanged gate emits (LLM-guided CEGIS) | **measured (toy, live)** | gpt-5.4 one-shot the 8-op fake-frontier rule → gate PASS, eval run `evidence/ff8f56e9….json`, artifact `2af3c998…`, $0.0049; gpt-5.4-mini honestly refused in 3 attempts (log.md wave 2) — a measured capability threshold |
| M12 | The deopt ratchet closes against a real frontier model | **measured (mechanism, live)** | guard trip → gpt-5.4-mini answered 1659ms → conformance-checked → ingested trace `1f0840b2…`; answer honestly wrong vs the hidden rule = the documented unverified-reference-authority semantics |
| M13 | The spend guardrail is hard: cap-0 refuses pre-send with a real key loaded; every paid call is ledgered | **measured (live)** | refusal `spent 0µ$ + worst-case 9966µ$ > cap 0µ$`, no artifact; 18/18 calls in `~/.auto/spend.jsonl` with purpose/model/usage/cost, session total $0.0277 of $25 |
| M14 | Judged matching is honest at the gate: no judge = never Pass; judge-yes emits saying JUDGED; judge-no blocks; judge calls capped + ledgered | **measured (toy, live)** | all three paths live-fired (log.md wave 8): INCONCLUSIVE/blocked at $0, PASS/emitted `afdba8aa…` (eval run `1127d673…`), FAIL/blocked (eval run `dee9401f…`); 2 ledger entries purpose `judge`, 164 µ$ total |
| M15 | Eval-run retention cannot break provenance: manifest-pinned runs survive any `--keep`; an unreadable registry blocks the sweep | **measured (mechanism)** | 9 gc/pinned tests (auto-contract + auto-registry, wave 8); live smoke removed 2/kept 1/protected-kept 0 |
| M16 | Judged differential is honest at the gate: no judge = never Pass even at 20/20 byte-equal; live judge-no arbitrations block emit below the declared threshold | **measured (live)** | wave-9 live-fire on the real wave-5 summarize residue: INCONCLUSIVE/$0, PASS 20/20 emitted `d58ccde6…` (eval run `d324aa7e…`), FAIL 10/20 blocked with 10 live judge arbitrations (log.md wave 9) |
| M17 | Registry transport preserves tamper evidence across the wire: content digest recomputed at both ends, signature verified against the served key, tampered server bytes refuse to pull | **measured** | 19 handler unit tests + loopback integration test + `evals/registry-remote/e2e.sh` green (push → pull fresh root → verify → tamper → refusal) |
| M18 | Guard geometry is measured, never assumed: v2 lexical cosine has a HIGHER noise floor than v1 Jaccard on short English docs at max quantile | **measured** | same store, same OOD probe: v1 trips 0.9421>0.8974; v2 admits 0.7525≤0.7590 at alpha 1 milli, trips at alpha 200 (0.7525>0.6967); witnessed inputs proceed at 0 in both (log.md wave 9) |
| M19 | Whole-task behavior is verifiable from recorded reality: task-scope contracts return real verdicts in trace mode; absent task I/O renders byte-identical wires/reports | **measured** | toy e2e task-contract PASS + task-level determinism section (wave 10); byte-identity golden tests both SDKs + store v1->v2 migration tests (ADR-0025) |
| M20 | Capability confinement survives embedding: in-process artifacts get tools only by exactly-declared host callables; every delta refuses at load; callable failure traps the artifact, never the host | **measured (live smoke)** | 5-case smoke on the wave-6 tool artifact (log.md waves 10+11); 9 executor_callback tests; ADR-0027 |

## economics claims

| # | claim | status | evidence |
|---|---|---|---|
| E1 | ~$0.50/40s per frontier agent run → <$0.001/<100ms compiled, at verified parity, on a real task | **pending** — the flagship experiment; unblocked by the $25/session cap once an API key is provided | protocol in log.md §planned-experiments |
| E2 | Compiled artifact latency is measured in ms end-to-end (wasm instance + inference) | **measured (toy scale)** | manifest `measured: compiled p50=3ms p95=3ms max=4ms` on the mlp artifact (36-obs replay, wave 1); reference recorded p95=0ms (subprocess timer floor — NOT a frontier baseline) |
| E3 | The latency ladder bottoms out in-process: embedding the tier-1 runner in the agent's own process removes spawn/HTTP/stdio overhead | **measured (toy fixture)** | python: p50 54.1 µs / 16,615 calls/s (pyo3, wave 9). node: p50 18.2 µs / 50,267 calls/s (napi, wave 10, echo fixture) — the V8 addon boundary measures cheaper than pyo3's on this box. Ladder: 736 ms frontier -> ~21 ms serve -> 290 µs stdio -> 54.1 µs py -> 18.2 µs node. Fixtures are trivial programs: the numbers measure the boundary, not inference |

## positioning notes (for related work — needs citations before the paper)

Ingredients with prior art, to be cited, not claimed as novel: CEGIS /
programming-by-example (SyGuS, FlashFill, TRANSIT/ESCHER — already cited in
ADR-0005), model distillation, model cascades / routing (FrugalGPT,
RouteLLM), JIT tiering with guards+deopt (V8/HotSpot), wasm capability
sandboxing (WASI), conformal abstention, agent skill libraries (Voyager,
LLM-as-tool-maker). The claimed contribution is the synthesis: an
effect-and-uncertainty-typed IR over recorded agent behavior; contracts as
emission-blocking type checker; confinement carried by the artifact;
measured-or-null manifests; and the deopt→record→recompile ratchet as one
toolchain. Verify no end-to-end equivalent exists at writing time.
