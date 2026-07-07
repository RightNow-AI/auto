# ADR-0006: distillation — one decision tree over frozen trigram features, trainer as subprocess, same-gate acceptance

status: accepted · scope: `crates/auto-model`, `crates/auto-passes` (trainer, model-interpreter), `crates/auto-cli` (`distill`), `spec/distillation.md`

## context

S5 automates distillation: spans whose behavior is real but not symbolic —
extraction honestly exhausts its budget because the closed DSL cannot spell
contains-tests or branching — still deserve compilation. Requirements:
acceptance must be measured, never assumed (the emit gate of ADR-0004, not
holdout scores); training must be deterministic and reproducible; the
featurization the trainer fits against must be bit-identical to the one the
artifact infers with, or the model silently splits on noise; no rust test
may touch the network; and the manifest reports what was measured or
nothing. The constitution names 0.5–3b specialists "or plain gradient
boosting when it wins" — and the honesty norms forbid shipping the big words
before a task needs them.

## decision

Five coupled choices:

1. **The smallest honest specialist**: one decision tree (`auto-model`) over
   **frozen char-trigram features** — lowercase unicode text, 3-char windows
   including spaces, FNV-1a 32-bit (offset 2166136261, prime 16777619) over
   utf-8 bytes, `% buckets`, occurrence counts as f64. Split rule `count <=
   threshold` goes **left** — sklearn's convention adopted verbatim, so an
   exported tree runs unmodified. Pinned hash vectors are asserted on both
   sides of the language boundary.
2. **Strict model wire format v0**: exact-match `model_version`, named
   `features.kind`, validated node/feature references, node 0 root, string
   leaf labels. A new featurizer is a loud format change.
3. **Trainer-as-subprocess protocol**: scikit-learn `DecisionTreeClassifier`
   in `tree_train.py`, invoked by the driver with observations JSONL, an
   output path, holdout fraction, seed, and an accuracy floor. One `metrics`
   stdout line is the only metrics source; exit codes distinguish trained
   (0) / honest refusal (2) / protocol error (3); the seed pins the split
   and the fit.
4. **Same-gate acceptance**: distilled artifacts are packaged program-as-data
   (model json as the `init` payload, generic model interpreter nested-built
   from source) and pass the identical contract-plus-differential gate as
   every compile. Holdout accuracy is provenance in the manifest, never a
   substitute for exact replay of witnessed inputs.
5. **Modal as an optional remote executor** of the same trainer script —
   profile-based, CPU now, the GPU slot reserved for torch specialists —
   never a CI dependency.

## alternatives considered

**Gradient boosting / random forests.** Strictly stronger fits on real fuzz,
and the constitution names boosting by name. Deferred: export complexity
multiplies — multiclass GBMs are per-class ensembles (trees × classes ×
stages) with score accumulation and a link function, so the wire format and
the wasm interpreter grow an order of magnitude for a capability no eval
task needs yet. The single tree exercises every joint (trainer protocol,
frozen features, gate, init payload) that an ensemble will reuse; the
upgrade is a `model_version` bump when a target measurably wins with it.

**Torch / LLM distillation.** The constitution's endpoint (0.5–3b
specialists). Needs GPU capacity and training-data volume no fixture has,
and it is spend-adjacent — teacher sampling and GPU hours belong under the
frontier-spend cap machinery, which does not exist yet. The Modal GPU slot
is reserved precisely for this; nothing pretends it landed.

**ONNX runtime inside artifacts.** Standard interchange, one exporter for
every sklearn/torch model. Rejected for v0: embedding an ONNX runtime into
each artifact is a heavyweight wasm dependency with its own parser surface,
against a ~60-line tree walk with zero imports that the loader can confine
by construction. Revisit when model families outgrow hand-rolled inference.

**Training in rust (linfa / smartcore).** Would keep the toolchain hermetic
— no python, no sklearn version skew. Rejected for now: the rust training
ecosystem is younger and less battle-tested than sklearn's twenty years of
tree code, and the subprocess boundary is the very seam that later swaps
executors (Modal) and frameworks (torch). Recorded as the trainer-hermeticity
open question; the protocol makes the swap cheap.

**Statistical acceptance for divergent targets.** Distilling a signature
whose recorded outputs genuinely vary requires accepting less than exact
replay — agreement rates, tolerance bounds. Deliberately not loosened here:
acceptance bounds belong in the **contract** (a format change under
`contract_version`, with its own ADR), not as a distillation-local escape
hatch from the gate.

## consequences

- The capability ceiling is explicit: separable lexical/level routing
  distills; anything softer refuses honestly (trainer exit 2, or gate
  block). The refusals are as designed as the passes.
- Python + scikit-learn become a toolchain dependency for `distill` (CI
  installs them; the e2e says so). Trainer hermeticity is an open question.
- The feature spec is frozen in two implementations; pinned vectors guard
  the constants, and the differential gate catches whatever drift the
  vectors do not.
- `auto-passes` nested-builds a second wasm interpreter (model next to DSL);
  build time and the wasm32 target prerequisite grow accordingly.
- Distilled artifacts reuse the `init` extension unchanged — a `.cbin`
  reader cannot tell a model payload from a program payload without parsing
  it, which is the point: one packaging, one gate, one runtime path.
- Holdout metrics enter manifests as provenance prose (notes) until manifest
  v1 defines queryable fields (existing open question, S7).

## sources

- scikit-learn `DecisionTreeClassifier` and the exported tree structure
  (`tree_.feature`, `tree_.threshold`, `children_left`/`children_right`;
  samples with `X[:, feature] <= threshold` go to the left child):
  <https://scikit-learn.org/stable/modules/generated/sklearn.tree.DecisionTreeClassifier.html>;
  <https://scikit-learn.org/stable/auto_examples/tree/plot_unveil_tree_structure.html>.
  Also verified locally against sklearn 1.9.0: a manual `<=`-goes-left walk
  over `tree_` arrays reproduces `predict` exactly.
- FNV-1a reference — 32-bit offset basis 2166136261, prime 16777619, xor
  byte then multiply:
  <https://en.wikipedia.org/wiki/Fowler%E2%80%93Noll%E2%80%93Vo_hash_function>
  (Landon Curt Noll's canonical page, isthe.com/chongo/tech/comp/fnv, was
  unreachable over https at verification time).
- Modal — serverless python functions with per-function CPU/GPU resources:
  <https://modal.com/docs>.
