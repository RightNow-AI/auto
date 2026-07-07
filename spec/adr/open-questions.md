# open questions

Decisions deliberately **not** made yet. Anything here was below the
confidence bar when touched (per repo norms: record, don't guess silently).
Each entry names the forcing spine item where known.

## IR semantics

- **Loops / iteration.** Real agent behavior loops (retry, tool-until-done,
  map-over-list); v0 graphs are strictly acyclic. Candidates: structured loop
  regions (a region with back-edge semantics) vs. tail-recursive graph
  references. Forced by S1 traces at the latest — recorded traces of loops
  must lower to *something*.
- **Branch semantics.** v0 `branch` is structural (arity ≥1 in / ≥2 out) with
  no predicate representation and no arm-selection rule. Forced by S2
  (contracts need to talk about paths) and S3 (backend must execute one).
- **Uncertainty × kind coupling.** Should `model_call` be forbidden from
  `deterministic`, or `transform` from `generative`? v0 leaves them
  independent; S1's determinism report will show whether declared classes
  survive contact with traces.
- **Type system growth.** Records/structs, unions/optionals, schema'd
  structures beyond the `json` escape hatch. Forced by S2 (the contract IS
  the type system; `json` everywhere would make contracts mushy).
- **Region nesting & overlap rules for extraction.** v0: flat, overlap
  allowed. Extraction passes (S4) may need exclusive or nested regions;
  unclear which.
- **Graph-level metadata / provenance pointers.** Trace ids, parent-graph
  links, producer info. Likely belongs to the manifest (S7), not the IR —
  keeping the IR pure data. Revisit at S1 when traces exist.

## serialization

- **Scalar-presence canonicality.** A foreign writer using
  `force_defaults`-style encoding produces buffers we accept and re-serialize
  canonically (bytes differ from input). Detecting present-but-default scalars
  would need raw vtable inspection (`unsafe`); not worth it at v0. Revisit if
  third-party writers appear before the registry's content addressing (S7)
  makes it matter.
- **Verifier limits vs. pathological graphs.** Default flatbuffers verifier
  limits (depth 64, 1e6 tables) bound what `from_bytes` can read — e.g.
  `list<>` nesting beyond ~60 levels, or graphs with more than ~1e6
  tables total. Raise deliberately if a real workload hits them.
- **Multi-error validation.** `validate()` reports the first violation in
  deterministic order. Collecting all violations would improve tooling UX;
  cheap to add when `auto inspect --check`-style workflows exist.
- **`.air` ↔ `.cbin` relationship.** `.air` (serialized graph) is a working
  convention as of S0; whether IR graphs travel inside `.cbin` unchanged or
  in a lowered form is a backend (S3) decision.

## traces (S1)

- **Concurrent tracing.** Record mode is concurrent as of post-spine wave 1:
  python span parenting is thread-local (seq/span_id allocation and file
  writes under one lock), typescript parenting rides AsyncLocalStorage
  (overlapping `Promise.all` spans each keep their own chain). Replay of
  concurrent runs CLOSED by ADR-0029 (wave 12): both SDK matchers consume
  the first unconsumed recorded span with the same (kind, name, canonical
  input) — sequential runs replay byte-identically to the old cursor
  (pinned), concurrent calls match order-independently. Residue, recorded:
  (1) divergent duplicates — the same (kind, name, input) recorded twice
  with DIFFERENT outputs is assigned in recorded order by ARRIVAL; under
  concurrency arrival order is a race by construction (spec/trace.md §8);
  (2) SDK replay now verifies the multiset of effectful calls and their
  I/O, not arrival order — order claims across traces live in rust compare
  and determinism analysis; (3) rust `auto-trace::replay::compare` still
  zips effectful spans positionally in seq order, so comparing two traces
  of a concurrent agent whose interleavings differ reports a false
  SignatureMismatch — a concurrency-aware trace comparison is unowned work
  in `auto-trace`.
- **Typescript type-checking.** The ts SDK runs via node's type stripping;
  tests are runtime tests and no `tsc --noEmit` gate exists yet (would add a
  devDependency). Add when the SDK grows consumers.
- **Crash-truncated final line — CLOSED, wave 12 (ADR-0030).** Explicit
  recovery (`auto record --recover-partial`): parse until a torn FINAL line
  (torn middle stays fatal in both modes), ingest marked PARTIAL (store
  schema v3), determinism analysis excludes partial traces and says so with
  a count line; strict remains the default everywhere. Open residue:
  partial traces are invisible to verification observations by exclusion —
  a contract-side accounting of excluded partials (an Unchecked note when a
  scope would have matched them) exists in the harness; whether partial
  traces should ever feed synthesis (they cannot today) is deliberately
  unasked until a real corpus needs it.
- **Payload scrubbing.** `env_read` never records values, but model/tool
  inputs are recorded verbatim and may contain secrets the agent interpolated
  into prompts. A scrubbing/redaction layer (and its effect on digests) needs
  design before traces leave the local machine.
- **Store growth.** No GC, no compaction, no parquet export yet; fine for
  local volumes. Revisit when a store exceeds what sqlite handles gracefully.
- **Sub-millisecond durations.** All toy-agent calls record 0ms, so the
  time-weighted fraction honestly reports "no data". Higher-resolution
  timing (µs) would make time weighting useful for fast tool calls.
- **`record`'s task label.** The task comes from the SDK constructor (the
  recorded header), not from the CLI. An `AUTO_TRACE_TASK` env override could
  let operators relabel runs without code changes — but two sources of truth
  for the same field needs a precedence rule first.
- **OTLP bridge.** Semantics mirror otel's trace/span model on purpose; an
  exporter bridge (store → OTLP) is cheap if ecosystem integration is wanted.
  Not before the local-first loop is proven.

## contracts (S2)

- **Task-scope verification — RESOLVED, wave 10 (ADR-0025).** SDKs record
  whole-run I/O (`task_input=` / `set_task_output`), the wire and store carry
  it (optional header field + `task_output` line; store schema v2, v1
  migrates in place), the determinism report gains a task-level section
  (only when recorded; old stores render byte-identical), and trace-mode
  `auto verify` of task-scope contracts returns real verdicts (toy e2e PASS
  from recorded reality). Still open at task scope: SYNTHESIS/compilation
  (emit stays span/region-scoped; compile/distill keep their refusals);
  task-level cost/token budgets (no honest declaration channel — span attrs
  do not aggregate without an invented rule; Unchecked until one exists);
  daemon watching (the ratchet cannot recompile what emit refuses — the
  daemon refuses upfront, guard in watch.rs).
- **Judged / statistical match modes.** Partially closed (wave 8, ADR-0019):
  `match = "judged"` examples compare by LLM-judged semantic equivalence
  through the `Judge` seam (`--judge-model`, capped + ledgered per ADR-0010;
  exact-equal short-circuits free; no judge = Inconclusive, never Pass).
  Statistical acceptance closed in wave 6 (ADR-0018). Judged DIFFERENTIAL
  closed in wave 9 (ADR-0021): `differential_match = "judged"` arbitrates
  byte-divergent replay groups through the same judge, the declared
  agreement threshold still deciding.
- **Judged differential residue (ADR-0021).** Three recorded gaps.
  (1) Minority witnesses are never judged: the differential compares one
  reference per group (the ADR-0018 canonical pick), so recorded minority
  outputs of a divergent group are invisible to the verdict — the
  per-class / weighted-witness question already open for training now has a
  verification-side twin. (2) Judge spend is bounded only by the input
  population (at most one call per byte-divergent distinct input): a large
  store can hit the ADR-0010 session cap mid-gate, which surfaces honestly
  as a Failed agreement check but buys nothing — a declared per-gate
  judge-call budget in the contract may be wanted. (3) Judge
  reproducibility (extends the ADR-0019 entry): a judged-differential eval
  run is reproducible only up to the judge; versioning / judge-eval
  evidence beyond naming the model remains open.
- **Property set growth.** The v0 set is closed on principle (no code
  execution at verify time, ADR-0003). Growth path: sandboxed wasm
  predicates once the S4 sandbox exists — never unsandboxed snippets.
- **Cost / token measurement.** Partially closed: the reserved span attrs
  `cost_usd_micros` / `tokens` (spec/trace.md §3 — decimal u64 strings set
  by the recording agent) make `max_cost_usd_micros` / `max_tokens`
  measurable: p95 over observations, all-or-unchecked (partial attr coverage
  never passes), malformed values fail loudly. Remaining: the attrs are the
  agent's own declaration, verified against nothing — billing verification
  against provider invoices and per-provider adapters that set the attrs
  from real API responses do not exist; subject-mode (live) verification has
  no billing source at all, so those budgets stay Inconclusive there.
- **Reference-side budgets vs subject-mode emit.** Cost/token budgets are
  claims about the RECORDED reference; a wasm subject has no billing, so a
  budget-carrying contract honestly forces every compile/distill emit
  Inconclusive (measured in wave 3 — the ticket-triage demo split into an
  emit contract and a recorded-reference contract as the workaround).
  Candidate fix: the emit gate checks reference-side budgets against the
  store observations (trace mode) while subject-side checks run the
  candidate — one contract, two measurement sources, both honest. Needs a
  harness change with its own tests; decide before contracts get consumers.
- **Eval-run retention.** Closed (wave 8, ADR-0020): `evalrun::gc` prunes a
  runs directory to the newest `keep_newest` records plus every
  manifest-pinned id, deletes only files it could have written
  (`<64-hex>.json`), and fails loud on any delete/read error. The protected
  set comes from `auto-registry`'s `pinned_eval_runs`, which re-verifies every
  artifact and refuses (never a partial set) on any corruption, so GC cannot
  collect a run a corrupt-but-real manifest still pins. Still open: age- and
  size-based policies (mtime horizons, byte ceilings), and the two-store
  retention move that ties run lifetime to the registry — tracked under the S7
  registry "GC / retention" entry, which stays open.

- **Eval-run retention — age policy closed (wave 10, ADR-0020 amendment).**
  `evalrun::gc_with_age` adds an optional caller-supplied cutoff: a record
  past the keep-newest floor is removed only if also STRICTLY older than
  the cutoff (pins always kept; ties keep), so age can only ever restrict
  deletion, never extend it; `gc` = `gc_with_age(.., None)`, byte-identical.
  Still open: size-based policies (byte ceilings) and tying run lifetime to
  the registry (S7 GC entry stays open).

## artifacts & execution (S3)

- **Component model migration.** The constitution names the wasm component
  model as the backend; v0 ships a core module with the hand-frozen
  `alloc`/`run` ABI (ADR-0004). Migration means wit worlds + canonical ABI
  replacing packed-pointer returns — a format break with a manifest bump.
- **WASI capability plumbing for impure artifacts.** v0 artifacts are pure
  (zero imports, empty capability set). Impure artifacts need declared IR
  effects (`net`/`fs`/...) mapped to WASI grants at instantiation — the
  runtime-enforced effect system the constitution promises. Forced by the
  first compiled task whose traces carry capability effects.
- **Float/NaN determinism.** wasm NaN bit patterns are nondeterministic
  across engines/platforms; the toy task is string-only so nothing forces it
  yet. If numeric tasks arrive, decide on NaN canonicalization config and
  what "same output" means for float-carrying canonical JSON.
- **Artifact size & multi-module layout.** One `module.wasm` per container
  today. S5 adds model files (onnx/gguf) and maybe multiple modules — entry
  naming conventions, size budgets, and chunking/streaming are undecided.
- **Cost measurement feeding manifests.** `measured` carries latencies only;
  no cost-per-call exists anywhere (ties to the contracts entry: cost/token
  capture). The headline economics claim needs measured frontier cost vs
  compiled cost — plumbing and units undecided.

- **In-process python embedding: tool callbacks, wheels/CI, napi twin
  (ADR-0024).** `auto-py` embeds a PURE `.cbin` in the host Python process
  (pyo3 0.29 / abi3-py310). Tool callbacks CLOSED in wave 11 (ADR-0027):
  `Runner(path, tools={...})` maps declared capabilities to host Python
  callables (exactly-declared rule, GIL reattached inside callbacks) and
  `AutoAbstained` carries structured reason/distance/threshold. The napi
  twin landed pure-only (ADR-0026). Still open: abi3 wheels + the optional
  CI job (Rust gates run without CPython — proposed YAML in ADR-0024
  appendix); per-request accounting for embedded tools (see ADR-0028
  entry); async surfaces.

- **Per-request tool policy — budget landed, authorization open (ADR-0028).**
  `auto serve --max-tool-calls-per-request N` caps executed tool calls per
  request through the `HostTools::Callback` seam (ADR-0027): the n+1-th call
  in one request gets an err envelope surfaced as a 500, with a stderr audit
  line per executed call. Correct for the sequential server (ADR-0011) —
  the recorded thread-per-request upgrade must move the shared counter into
  per-request state first. Still open: authorization / accounting (which
  caller may invoke which tool, charged to whom) and the embedded
  (auto-py / napi) twin of the same budget over the identical seam.
- **auto-node twin parity + wasm-in-node (ADR-0026).** The napi twin is
  pure-only v0: `tools=` host callbacks (auto-py reached parity in
  ADR-0027) are a recorded follow-up. wasm-in-node — running the artifact's
  module in V8 directly, zero native install — is the strongest recorded
  alternative; prerequisite is a loader/guard conformance suite so a JS
  reimplementation of the refusal rules and guard math cannot drift
  silently. Also recorded: an async `answerAsync` surface, npm packaging,
  and a shared embed-core crate when a third embedding lands. First bench:
  p50 18.2 µs/call vs the ~290 µs stdio floor (echo fixture,).
- **Embedded packaging (wave 13, ADR-0024/0026 appendices).** Both
  embeddings now build by strangers — auto-py wheel via maturin, auto-node
  addon via the checked-in npm script — CI-proven on ubuntu only
  (.github/workflows/embedded.yml, optional by convention). Open:
  windows/macOS CI legs + per-platform prebuilds/wheel matrix (v0 platform
  = build host); registry publish of wheel/addon alongside .cbins;
  @napi-rs/cli pinned to major only (exact pin needs a lockfile, which
  private v0 avoids); the hand-written index.d.ts is aligned with the
  macro-derived typedef only by review — a CI diff would make drift
  mechanical.
- **Embedded per-answer tool budget (wave 13, ADR-0032).** auto-py
  `max_tool_calls=` mirrors ADR-0028 at the answer boundary (n+1-th call =
  err envelope = trap; audit per executed call; budget without tools
  refuses at load). Open: the napi twin's budget (blocked on its tools=
  parity), and per-TOOL budgets vs the single per-answer count.

## synthesis (S4)

- **LLM proposal generation.** The constitution names LLM-guided CEGIS; v0
  is enumerative (ADR-0005). The checker half (observations, sandboxed
  evaluator, emit gate) is built; proposal generation waits on the
  frontier-spend cap plumbing — no paid in-loop calls without it.
- **DSL growth.** Records, branching, regex ops. Regex synthesis overfits
  easily (a mined pattern can memorize its examples), so regex ops need
  held-out discipline before entering the op set. Every op-set change bumps
  `dsl_version` with an ADR.
- **Ranking beyond shortest-first.** Shortest-first is Occam by op count;
  among equal-length fits, enumeration order decides. An observed-cost
  ranking (measured per-op latency) might pick better programs — but the
  cost model must be measured, not asserted.
- **Synthesis provenance fields in manifest v1.** Program digest, states
  explored, distinct inputs are carried in `notes` as prose today. Manifest
  v1 (S7) should carry them as queryable fields.
- **Per-program codegen.** The generic interpreter re-parses `program.json`
  per instance; hot artifacts may justify rust→wasm codegen per program,
  which ADR-0005 rejected for compile cost. Revisit on measured interpreter
  overhead.
- **Incremental resynthesis.** New traces arrive after an artifact exists;
  today synthesis is from-scratch over all distinct observations. Cheaper:
  re-verify the existing program against the grown set first, resynthesize
  only on failure. Undecided, forced by the S6 recompile loop.

## distillation (S5)

- **Gradient boosting / forest export.** The v0 wire format carries exactly
  one tree; multiclass GBMs are per-class ensembles (trees × classes ×
  stages) with score accumulation. `model_version` bump + interpreter growth
  when a target measurably wins with them (ADR-0006).
- **LLM specialists (0.5–3b) beyond the MLP.** The torch rung exists
  (ADR-0009): `mlp_train.py` trains a single-hidden-layer MLP on Modal's
  A10G profile, exported as plain-weights json (`mlp_version` 0),
  parity-gated in CI without torch. Still open toward the constitution's
  endpoint: true LLM specialists — which need tokenizer-featurizers (the
  frozen trigram counts stop at lexical routing; a sub-word featurizer is
  a `features.kind` format change), real data volume, and spend-cap
  plumbing for teacher sampling and GPU-hours before any of it is honest.
- **Statistical acceptance bounds.** Closed in two halves. Gate side (wave
  6, ADR-0018): a contract-declared `differential_min_agreement_milli`
  prices divergence at the differential — id-bearing, never a
  distillation-local bypass. Training side (wave 7, ADR-0018 amendment):
  divergent references become trainable only behind an explicit operator
  flag (`--divergent-pick most-common`) — the most-witnessed recorded
  output per input, ties lexicographic on canonical string, errored groups
  never trainable; the default stays refusal, and the declared threshold
  stays the acceptance authority at the gate. Weighted witness training
  landed behind `--divergent-pick weighted` (ADR-0031, wave 12): one row
  per witnessed output of every non-errored group, weight = witness count
  (sklearn sample_weight / mlp loss weighting); distill-only (synthesis
  rejects conflicting observations by construction); weighted and
  most-common provably agree per separable group, differ where features
  collide (measured fixture in the ADR) — the unchanged gate prices
  whichever is emitted. Open residue: per-CLASS balancing (oversampling
  minority classes across groups) is a different knob, still unowned.
- **Feature spec growth.** Word-level features and embeddings beyond char
  trigrams — each a new `features.kind` under a version bump. Embeddings
  also unlock S6 guards (OOD distance needs an embedding space).
- **Trainer hermeticity.** `tree_train.py` drags python + scikit-learn into
  the toolchain. Rust-native training (linfa/smartcore) would make distill
  hermetic; ecosystem maturity said not yet (ADR-0006).
- **Corpus / dataset management.** The distill-agent corpus is a built
  fixture. Real tasks need dataset versioning, class-balance reporting, and
  retraining policy once the S6 loop feeds novel traces back.

## tiering (S6)

- **Semantic embedding guards (ADR-0023).** Guard wire v2 ships the lexical
  rung: trigram-hash cosine, dependency-free, the same split-conformal
  calibration. The constitution's full reading — semantic embedding distance
  (an in-process onnx encoder, e.g. MiniLM) — needs an inference stack in the
  runtime plus a distribution story for encoder weights (in-artifact: tens of
  MB per .cbin and the encoder version becomes load-bearing provenance;
  alongside: a registry story that does not exist). Gates run with no
  network, so a download-on-first-use encoder can never be the tested path.
  When decided, it lands as a new `embedding.method` on the v2 wire — unknown
  methods already refuse loudly. Also open: a measured Jaccard-vs-cosine
  trip-rate comparison on real traffic (first data point, wave 9: on 20
  short billing tickets at alpha 0.001, v1 Jaccard tripped a fully
  out-of-domain probe at distance 0.9421 > 0.8974 while v2 cosine admitted
  it at 0.7525 <= 0.7590 — dense trigram hashing has a higher noise floor on
  short English; alpha 200 milli trips it); v2 claims geometry, not an
  improvement number.
- **Real conformal calibration.** Leave-one-out max over a handful of
  witnesses is the degenerate case and claims no coverage. Proper conformal
  prediction needs calibration sets, a designed nonconformity score, and a
  contract-declared coverage level. The disjoint-witness threshold-1.0
  behavior (spec/runtime.md §2) is the crudeness made visible.
- **Latency / cost guards.** Guards fire on input distance only; nothing
  trips on a slow tier-1 call or an expensive tier-0. Needs measured
  per-call budgets in the manifest and runtime enforcement — ties to the
  cost-capture entries above.
- **Auto-recompile daemon.** The ratchet is manual: deopt ingests, an
  operator recompiles. A watcher (N new observations → recompile → atomic
  artifact swap) is the runtime the constitution describes; incremental
  resynthesis (synthesis entry above) is its prerequisite.
- **Frontier tier-0 binding.** Tier-0 is a pluggable command; binding the
  constitution's frontier-model interpreter requires API access under the
  hard spend cap (whose plumbing still does not exist — toolchain entry
  below) plus full trace capture of tier-0 runs, not the single I/O
  observation v0 records.

## registry (S7)

- **Remote registries — loopback transport landed (ADR-0022).** `registry
  serve|push|pull` (spec/registry.md §6) move artifacts and their detached
  signatures between roots over HTTP, content verified at both ends. Still
  open: **authentication and TLS** (v0 is loopback / trusted-LAN only, no
  auth, plaintext), a **multi-writer server** (the accept loop is
  sequential), deletion/GC over the wire, and foreign-key trust policy. The
  OCI distribution spec is the recorded target for an open, public
  registry.
- **sigstore.** Keyless signing (OIDC identity, Fulcio, Rekor transparency
  log) is the constitution's named design and needs network + identity
  infrastructure. The detached-signature layout was chosen to map onto
  sigstore bundles without re-identifying artifacts (ADR-0008).
- **Key rotation / multi-key trust.** One local keypair, no rotation, no
  revocation, no policy for which foreign keys to trust. All three are
  required before any artifact crosses a machine boundary.
- **GC / retention.** Nothing is ever deleted from a registry, and the
  eval-run records manifests pin (contracts entry above) must outlive every
  artifact citing them. A retention policy needs both stores to move
  together.
- **Signing eval-run records.** Artifacts are signed; the eval runs their
  manifests cite are bare content-addressed files, so the manifest→eval-run
  link is digest-only. Whether runs get their own signatures or travel in a
  signed envelope with the artifact is undecided.

## toolchain & repo

- **flatc distribution for contributors.** Currently: manual install of the
  pinned release binary (README). Options later: vendored per-OS binaries,
  nix/devcontainer, or the pure-rust `planus` codegen. Decide when a second
  regular contributor exists.
- **Licensing.** "Spec open, implementation ours" needs a concrete license
  pair (spec: CC-BY? implementation: proprietary/BUSL/Apache?). Owner
  decision; no LICENSE file until made.
- **Proptest regression files.** Policy: commit `proptest-regressions/` files
  when a failure is ever found (none yet, so none exist).
- **Frontier API spend cap plumbing.** The constitution mandates a hard cap
  per session with logging; no paid calls exist in S0 so nothing enforces it
  yet. Must land with the first paid integration (S1 record / S3 compile).
