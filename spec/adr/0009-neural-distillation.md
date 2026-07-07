# ADR-0009: neural distillation — a tiny MLP, plain-weights JSON, a third interpreter, torch on Modal

status: accepted · scope: `crates/auto-model` (`mlp`), `crates/auto-passes` (mlp-interpreter, `trainer/mlp_train.py`, `trainer/modal_mlp_train.py`, `tests/mlp_parity.rs`), `spec/distillation.md` §8

## context

ADR-0006 shipped the smallest honest specialist — one decision tree — and
recorded neural specialists as the upgrade, reserving a Modal GPU slot for
them. The next residue class is real: routing that is separable but not
axis-aligned, where a single tree's `count <= threshold` splits measurably
fall short and a learned linear mix of trigram counts does not. The
requirements do not move: featurization bit-identical between trainer and
artifact; deterministic, reproducible training; the same pass-or-nothing
emit gate; measured numbers only; and **no torch in CI** — the dependency
is gigabytes, GPU-shaped, and CI must stay fast and network-free at test
time.

## decision

Five coupled choices, each the smallest honest step:

1. **The smallest honest neural specialist**: a single-hidden-layer relu
   MLP over the SAME frozen char-trigram fnv1a features (spec/distillation
   §2 — pinned vectors and all). No embeddings, no tokenizer, no depth: one
   `[hidden][buckets]` matmul, relu, one `[classes][hidden]` matmul,
   argmax. **Ties break toward the lowest class index** — a documented,
   pinned convention (tested natively, in wasm, and in the trainer's
   verifier), never an accident of float ordering.
2. **Plain-weights JSON, `mlp_version` 0**: `hidden_weights`,
   `hidden_bias`, `out_weights`, `out_bias` as plain float lists plus
   sorted `classes` — read strictly (exact version, unknown fields
   rejected, shapes validated, non-finite weights refused). The same
   loud-format-change policy as every other versioned wire.
3. **A third interpreter, not a runtime**: `mlp-interpreter` compiles
   auto-model's own inference to wasm under the frozen ABI + `init`
   extension (mlp json as the payload) — one implementation, two
   compilations, zero imports, every failure a trap. Distilled-MLP
   artifacts face the identical differential gate as trees and synthesized
   programs.
4. **torch trains over the unchanged subprocess protocol**: `mlp_train.py`
   is a drop-in `tree_train.py` sibling (same flags, metrics line, exit
   codes; adds `--hidden/--epochs/--lr/--weight-decay` — full-batch AdamW,
   where decay 0 is exactly plain Adam), imports the frozen featurizer
   from tree_train, seeds with `torch.manual_seed` under strict
   `use_deterministic_algorithms(True)` (+ `CUBLAS_WORKSPACE_CONFIG` for
   cuda matmuls), and verifies the **export** with a pure-python forward
   pass before reporting metrics. torch imports lazily; absent torch is an
   exit-2 refusal naming Modal as the intended executor.
5. **Modal A10G as the training executor, parity in CI without torch**:
   `modal_mlp_train.py` turns the reserved GPU slot on for real
   (`gpu="A10G"`) — optional at this scale, wired because it is the exact
   profile larger specialists inherit. CI proves inference parity over
   hand-built weights (`mlp_parity.rs`); training is verified by recorded
   Modal evidence runs, never by a CI job.

## alternatives considered

**ONNX runtime inside artifacts.** Rejected by ADR-0006 for trees; the
rationale compounds here — an ONNX runtime in every artifact dwarfs a
~40-line forward pass, adds a parser surface to the confinement boundary,
and buys generality no 7-field wire format needs. Revisit when model
architectures outgrow hand-rolled inference.

**candle / burn (rust ML) for in-artifact inference.** Real rust inference
stacks that compile to wasm and would keep training rust-native too.
Younger and heavier than what this format needs: the forward pass is two
matmuls, and the existing auto-model crate already compiles to both
targets. The subprocess trainer seam (ADR-0006) is exactly where a rust
training backend could swap in later without a format change.

**Quantization (int8/f16 weights).** Premature: a default-shape model
(1024×64) is under ~1.5 MB of json and infers in microseconds; nothing has
measured a size or latency problem. The constitution files quantization
under the optimization pass, where it stays until a measurement forces it.

**Distilling LLM specialists (0.5–3b) now.** The constitution's endpoint,
still not honest to claim: teacher sampling is paid spend under a cap
whose plumbing does not exist, no fixture has the data volume, and
sub-word featurization (a tokenizer) is a `features.kind` format change of
its own. Recorded in spec/adr/open-questions.md; the MLP is the rung that
proves the torch/GPU/export/gate joints those models will reuse.

**Statistical acceptance for divergent targets.** Unchanged from ADR-0006:
acceptance bounds belong in the contract under `contract_version`, never
as a distillation-local gate bypass. A neural model does not soften the
gate.

## consequences

- The distillation family now has two wire formats (`model_version` 0,
  `mlp_version` 0) and three embedded interpreters; `auto-passes` build
  time and the wasm32 prerequisite grow again.
- torch joins the toolchain **only where training runs** (an operator's
  machine or Modal); CI never installs it, so the trainer's torch path is
  exercised by Modal evidence runs, not by CI — stated in the spec rather
  than hidden.
- Reproducibility is scoped honestly: same observations + flags + torch
  build + device ⇒ byte-identical model json; nothing is claimed across
  torch releases or between cpu and gpu, because torch itself claims
  nothing there.
- GPU spend becomes possible in the training path. Runs are operator-
  initiated (`modal run`), never CI- or test-initiated; the frontier-spend
  cap plumbing (open question) should eventually cover GPU-hours too.
- The `classes` head is sized to **all** witnessed labels (sorted), so an
  exported MLP can name any recorded label — unlike a tree, which can only
  answer labels its training split contained.

## sources

- pytorch reproducibility notes — `torch.manual_seed` seeds all devices;
  `torch.use_deterministic_algorithms(True)` selects deterministic kernels
  and errors on ops without them; "Completely reproducible results are not
  guaranteed across PyTorch releases, individual commits, or different
  platforms. Furthermore, results may not be reproducible between CPU and
  GPU executions, even when using identical seeds":
  <https://docs.pytorch.org/docs/stable/notes/randomness.html>. The
  nondeterminism alert for CUDA NLLLoss applies to the spatial (2d)
  variant (`aten/src/ATen/native/cuda/NLLLoss2d.cu`); plain classification
  cross-entropy runs under strict deterministic mode.
- NVIDIA cuBLAS reproducibility — set `CUBLAS_WORKSPACE_CONFIG=:16:8` or
  `:4096:8` for bitwise-reproducible results across streams:
  <https://docs.nvidia.com/cuda/cublas/> (§results reproducibility).
- `torch.nn.Linear.weight` is shaped `(out_features, in_features)` — the
  exported rows are already the wire's row-major `[hidden][buckets]` /
  `[classes][hidden]`:
  <https://docs.pytorch.org/docs/stable/generated/torch.nn.Linear.html>.
- Modal GPU acceleration — `gpu=` on `@app.function`; the current guide
  spells the 24 GB Ampere tier `"A10"` while modal 1.3.0's client (the
  pinned version, checked in source) still ships and accepts the `"A10G"`
  spelling used here, passing it through uppercased:
  <https://modal.com/docs/guide/gpu>,
  <https://modal.com/docs/reference/modal.gpu>.
