# CLAUDE.md, Auto (the AGI compiler)

read this fully before touching code. it records the vision, objects, pipeline, stack, norms, and guardrails we hold this project to. everyone working on the repo follows it.

## what this is

Auto converts interpreted cognition into compiled cognition. today an agent run re-derives its plan token by token on a frontier model: brilliant, expensive, unbounded, unauditable. Auto records that behavior, proves what is secretly symbolic, distills what is not, verifies the result against a behavioral contract, and emits a **cognition binary** that is bounded, reproducible, capability-confined, roughly 1000x cheaper and 100x faster. speech, then writing, then computation, then cognition: the fourth compilation event. the interpreter is model-agnostic by design. it is a frontier LLM today, but nothing in the pipeline assumes that, so the same loop compiles the behavior of whatever sits at tier-0 next, world models included. the end state we are building toward: contracts as executable institutions, the IR as the shared language of cognitive work, binaries as the commodity form of expertise, and a tiered runtime where nothing is ever figured out twice.

## the three objects

1. **`auto`, the compiler.** cli + service. `auto record` (attach to a live agent, capture traces) to `auto compile --contract` (lower to IR, run passes, verify) to `task.cbin`.
2. **the runtime.** tiered execution. tier-1: compiled fast path. tier-0: frontier model as interpreter, for novelty. guards on every compiled entry; guard trip to deopt to tier-0 to capture trace to recompile. the ratchet: every novel solve compiles back down. nothing figured out twice.
3. **the artifact + registry.** `.cbin` = code + small models + kernels + **manifest** (eval scores, cost/latency bounds, capability requirements, full provenance: traces, reference model, eval run ids). content-addressed, sigstore-signed. the manifest is the trust layer, it is never aspirational, only measured.

## pipeline

- **frontend:** trace SDK (python/ts shims, agents live there). records prompts, tool calls, args, results, branch decisions, env reads. plus the contract language: examples + properties + eval sets + budgets. the contract IS the type system.
- **IR (crown jewel):** typed task graph. nodes carry: capability effects (net / fs / exec / secrets / payments), memory effects (read / write / append), uncertainty class (`Deterministic | Probabilistic | Generative`), resource bounds. regions group nodes for extraction. progressive lowering, dialect-style, MLIR-inspired in design, custom-built (MLIR's machinery is tensor-shaped; this is effect-shaped). stable ids, versioned schema. whoever owns the IR owns the layer, so the spec is written for the world: `spec/ir.md`.
- **passes:**
  1. *symbolic extraction*, LLM-guided CEGIS on regions traces prove deterministic. most agent behavior is secretly a parser; this is where the 100x lives. candidates verified in sandboxes against traces.
  2. *distillation*, residual fuzzy nodes to small specialists (0.5–3b, or plain gradient boosting when it wins). automated train/eval/accept loop. accept only on measured parity.
  3. *verification*, the contract is the type checker. differential testing vs the reference frontier model, statistical acceptance bounds, property fuzzing. failing contract blocks emit. no exceptions.
  4. *optimization*, semantic caching, speculative execution, quantization, kernel-level work later.
- **backend:** wasm component model + onnx/gguf. wasi capabilities ENFORCE the effect system at runtime, a binary physically cannot exceed its declared capabilities. confinement lives in the artifact.
- **runtime:** guards = embedding-distance OOD + conformal prediction (calibrated abstention). a wrong "stay compiled" decision is a silent correctness failure, guards are a first-class component, never an afterthought.

## stack

- core (compiler, passes, runtime, registry, cli): **rust**. workspace, edition 2024.
- IR serialization: **flatbuffers** (byte-stable round-trip is a hard invariant).
- traces: otel-compatible SDK semantics to local parquet/sqlite first; clickhouse when volume demands.
- synthesis sandbox: **wasmtime** (no network inside sandboxes, ever).
- distillation: pytorch pipeline; serve via onnx-runtime / candle / llama.cpp inside the binary.
- property testing: **proptest**. golden files for every IR construct.
- signing: sigstore. registry content-addressed.
- sdks: plain python + typescript packages first; pyo3/napi bindings later.

## repo layout

```
crates/auto-ir        # typed task graph, effects, uncertainty, serialization
crates/auto-trace     # trace model, capture, replay, determinism analysis
crates/auto-contract  # contract lang: examples, properties, evals, budgets
crates/auto-passes    # extraction, distillation driver, verification, optimization
crates/auto-backend   # lowering to wasm component + model artifacts
crates/auto-runtime   # tiered execution, guards, deopt, recompile triggers
crates/auto-registry  # artifact store, manifests, signing
crates/auto-cli       # auto record|compile|run|inspect
sdk/python  sdk/typescript
spec/                # ir.md, manifest.md, adr/, written as if the world will read it
evals/               # reference tasks + harnesses
```

## build spine (ordered, gated, not time-boxed)

each item is one focused piece of work; the hardening pass comes after it lands. never skip a gate.

- **S0** scaffold + IR core: types, effects, uncertainty, flatbuffers round-trip, golden tests, cli skeleton, `spec/ir.md` v0.
- **S1** trace SDK: record real agent runs to trace store to replay to **determinism report** (the measured fraction of secretly-symbolic behavior; this number is the public proof of thesis).
- **S2** contract v0: examples + properties + evals to generated verification harness.
- **S3** first end-to-end compile: ONE task, hand-assisted passes allowed, artifact emitted, manifest honest. the demo: ~$0.50/40s per run to <$0.001/<100ms at measured parity.
- **S4** symbolic extraction automated (CEGIS loop, sandbox verification).
- **S5** distillation automated (train/eval/accept, parity-gated).
- **S6** runtime tiering: guards, deopt, trace-capture, recompile loop.
- **S7** registry + signing + manifest standard v1. spec opens.

## norms

- **honesty is load-bearing.** manifests report measured numbers or `null`, never fabricated, never rounded up. parity claims carry an eval run id. a failing eval blocks emit.
- verification is the product: no pass merges without differential tests vs the reference interpreter.
- no mocks pretending to be the compiler. a stub is labeled `stub` in code, `--help`, and docs.
- rust: `clippy -D warnings`, no `unsafe` without a justification comment, proptest on every IR transform.
- irreversible decisions get an ADR in `spec/adr/` (numbered, terse, alternatives listed).
- spec open, implementation ours. anything under `spec/` is written for external readers.
- terse everywhere. no marketing language inside the repo.

## guardrails

- frontier API spend: working under limited resources, we cap paid OpenAI usage per session and log every call to a spend ledger. no paid runs beyond the cap.
- no network inside synthesis/verification sandboxes.
- never claim compiled parity without an eval run id attached.
- never push main directly; CI green before merge; granular commits, honest messages.
- scope discipline: work only the item in front of you. note adjacent gaps in `spec/adr/open-questions.md` rather than wandering into them.

## glossary

- **cognition binary (`.cbin`)**, the compiled artifact: code + small models + manifest.
- **manifest**, measured guarantees: evals, bounds, capabilities, provenance.
- **tier-0 / tier-1**, interpreter (frontier model) / compiled fast path.
- **deopt**, guard-triggered fallback to tier-0 with trace capture.
- **the ratchet**, novel solves recompile down; nothing figured out twice.