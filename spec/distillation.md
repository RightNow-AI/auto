# Auto distillation — small specialists, v0

Status: v0, matches `crates/auto-model`, `crates/auto-passes`, and the
trainer (`crates/auto-passes/trainer/tree_train.py`) as merged. Model
wire-format version: **0** (the `model_version` field). Where prose and code
disagree, the code wins; this document is written for external readers.

**Distillation** is the pass for behavior that is real but not symbolic.
Symbolic extraction (spec/synthesis.md) compiles spans whose recorded
behavior a closed program space reproduces exactly; distillation takes the
residue — fuzzy routing, soft classification, threshold-shaped judgment that
no straight-line pipeline spells — and fits a **small specialist** to the
recorded observations instead. The constitution names 0.5–3b models "or
plain gradient boosting when it wins" as this pass; v0 ships the smallest
honest member of that family and says so: **one decision tree over hashed
character-trigram features**, trained by an external scikit-learn trainer.
Gradient boosting and neural specialists are the recorded upgrades
(ADR-0006; spec/adr/open-questions.md "distillation (S5)") — nothing here
pretends to be them.

## 1. shape of the pass

`auto distill` gathers the distinct recorded observations of one span
signature (exactly as synthesis does), hands them to a **trainer
subprocess** (§4), and packages the returned model **program-as-data**: the
artifact carries the model json as its `program.json` init payload plus the
**generic model interpreter** — `auto-model`'s inference compiled to wasm
(`crates/auto-passes/model-interpreter`, nested-built from source by
`auto-passes`' build.rs, embedded via `model_interpreter_wasm()`). The
module is loaded through the same additive `init` ABI extension synthesis
uses (spec/artifact.md §4): one `init(ptr, len)` per instance, model json as
the payload, zero imports, every failure a trap. The candidate then faces
the same emit gate as every artifact (§5). Distillation changed what
proposes candidates — a trainer instead of a search — not what admits them.

## 2. the feature spec (frozen)

Featurization is fixed, bit-for-bit, in trainer and interpreter alike:

1. lowercase the text (unicode lowercasing);
2. slide a 3-char window over its chars — unicode scalar values, **spaces
   and punctuation included**; a text under 3 chars has zero features;
3. hash each trigram's utf-8 bytes with **FNV-1a 32-bit**: offset basis
   **2166136261**, prime **16777619** — xor the byte first, then multiply;
4. the feature index is `hash % buckets` (`buckets` is a model field, > 0);
5. the feature value is the trigram's **occurrence count**, as f64.

Pinned vectors, asserted in both implementations:

```
fnv1a("")    = 2166136261
fnv1a("a")   = 0xE40C292C
fnv1a("abc") = 0x1A47E90B
```

Identical featurization is **load-bearing**: the trainer computes features
to fit the tree; the interpreter recomputes them at inference. Drift in any
constant — offset, prime, window, lowercasing, counting — silently reshuffles
every feature index, and the tree splits on noise. That failure mode
produces no error, only wrong answers, which is why the constants are pinned
by test vectors on both sides and why the differential gate (§5) replays
every witnessed input through the wasm interpreter before anything is
emitted. The `features.kind` string (`"char_trigram_fnv1a"`) exists so a
future featurizer is a loud format change, never a silent reinterpretation.

## 3. the model wire format

One JSON document, canonical form (sorted keys, compact separators), read
strictly by `auto_model::Model::from_json`:

| field | meaning |
|---|---|
| `model_version` | exactly **0**; any other value is rejected loudly |
| `features.kind` | must be `"char_trigram_fnv1a"` (§2) |
| `features.buckets` | feature-vector width; must be > 0 |
| `features.input_field` | optional: object field holding the text; absent = the input **is** the text |
| `nodes` | the tree, node **0 is the root**; must be non-empty |

Each node is externally tagged as `split` or `leaf`:

- `{"split":{"feature":F,"threshold":T,"left":L,"right":R}}` — internal
  node. Inference walks **left iff `count <= threshold`** — sklearn's split
  convention, adopted verbatim so exported trees run unmodified;
- `{"leaf":{"label":"..."}}` — a leaf carries a string label, returned as a
  JSON string.

**Strict parse.** Readers reject: `model_version` ≠ 0; an unknown
`features.kind`; zero buckets; an empty node table; unknown fields anywhere
(`deny_unknown_fields`); a child index past the node table; a split feature
past `buckets`. No best-effort reads. Inference is total: a missing
`input_field`, a non-string text, and a walk longer than the node count (a
cycle) are typed errors — inside the artifact each becomes a trap, an honest
execution failure.

## 4. the trainer protocol

Training runs in an **external subprocess**, not in the compiler. The
`--trainer` argument is one command string, split on whitespace (e.g.
`"python crates/auto-passes/trainer/tree_train.py"`); the driver appends its
arguments: the observations file (JSONL, one `{"input":…,"output":…}` object
per distinct observation), the model output path, the holdout fraction
(default **0.25**), the seed (default **0**), the holdout-accuracy floor
(default **1.0**), and the input field when the contract's inputs are
objects. Exact flag spellings live in the driver and trainer — code wins.

- **The metrics line is the only metrics source.** The trainer prints
  exactly one stdout line beginning `metrics ` followed by a JSON object —
  `holdout_accuracy` at minimum, plus whatever the trainer measured (split
  sizes, tree shape). The driver reports these numbers and never recomputes,
  infers, or rounds them. What the trainer did not measure does not exist.
- **Exit codes.** `0` — model written, metrics emitted. `2` — **honest
  refusal**: the trainer measured itself below the floor (or cannot split
  usefully) and declined to hand over a model; refusal is an outcome, not a
  crash. `3` — protocol error: bad invocation, unreadable observations.
  Anything else is a driver error.
- **Determinism.** The seed fixes both the holdout split and the tree
  fitting: same observations, same parameters, same seed → same split, same
  tree, same model bytes. Two distill runs over one store are reproducible.

### 4.1 training data under divergence — picks and witness weights

By default an input recorded with different outputs refuses to distill: a
divergent reference is not evidence to fit against. Two **explicit operator
choices** (the CLI's `--divergent-pick`, never a default) select training
data anyway. Neither touches the gate — the declared
`differential_min_agreement_milli` (ADR-0018) remains the sole acceptance
authority — and a group with any recorded error is never trainable under
any mode.

- **`most-common`** (ADR-0018 amendment) — one training row per input: its
  most-witnessed recorded output, ties toward the lexicographically
  smallest canonical string. Minority witnesses are discarded entirely.
- **`weighted`** (ADR-0031) — one training row per **distinct witnessed
  output** per input, carrying an optional `"weight"` field in the
  observation JSONL: that output's witness count, a JSON integer ≥ 1,
  absent = 1. A file whose weights are all 1 **is** the weightless protocol,
  byte-for-byte. Conflicting labels for one input are the point: the
  trainer sees the true witnessed distribution and resolves it by weight —
  sklearn `sample_weight` for the tree, per-example loss weighting
  (`(ce_i·w_i).Σ / w.Σ`) for the mlp.

Metrics under weights: the line gains `weighted_train_accuracy` (witness
mass reproduced over total training mass — the objective the fit saw) and
`train_weight` (that total); `train_accuracy` stays the plain fraction of
training **rows**, and `holdout_accuracy` stays **plain and unweighted** —
measured reality, not the training trick. With a divergent group's rows in
play, 100% plain accuracy is impossible by construction; the shortfall is
the recorded disagreement, reported, never smoothed.

For a single group, a weighted fit resolves to the same label as the
most-common pick — the same argmax over the same witness counts. The two
differ where the frozen features cannot separate groups (colliding
trigrams, texts under 3 chars, depth limits): one leaf or decision region
then holds several groups' rows, and most-common counts **group votes**
where weighted counts **witness mass**. Measured fixture:
`trainer/test_trainers.py` and ADR-0031.

## 5. acceptance — the same gate, holdout as provenance

A distilled artifact passes **the same pass-or-nothing gate as synthesis and
hand-assisted compiles** (spec/artifact.md §7): contract verification plus
**differential exact replay of every distinct recorded input** through the
candidate wasm, with Fail *and* Inconclusive blocking emit. There is no
force flag and no separate "statistical" gate:

- **Holdout metrics are provenance, never acceptance.** `holdout_accuracy`
  is evidence the tree generalizes beyond its training split; it travels in
  the manifest notes next to the eval run id. It is **not** a substitute for
  the gate: a model at 100% holdout that misses one witnessed input is
  emit-blocked, and a model below 100% on the witnessed inputs never emits.
  Honest: v0 distills fuzzy *rules*, not noisy ones.
- **Divergent signatures refuse by default.** An input recorded with
  different outputs is not evidence to fit against unless the operator
  explicitly says how to read it (§4.1) — and even then the choice selects
  training data only. The gate is unchanged: an undeclared-exact contract
  hard-fails divergent references at the differential, and a contract
  declaring `differential_min_agreement_milli` (ADR-0018) passes or fails
  on the measured agreement rate — a majority- or weighted-trained subject
  reproduces whichever output it learned, and divergent groups still count
  per the ADR-0018/0021 rules.
- The sklearn tree and the artifact's tree walk are two implementations of
  one split semantics (§3); the gate executes the **wasm** side on every
  witnessed input, so an export or semantics drift surfaces as a gate
  failure, not a silent divergence.

## 6. remote training (Modal)

The subprocess protocol (§4) makes the trainer's executor swappable: the
driver runs a command and reads a metrics line; nothing requires that
command to train locally. The Modal profile runs the **same
`tree_train.py`** on remote CPU — same arguments, same metrics line, same
exit codes, same seed determinism — and reserves the GPU slot for the torch
specialists recorded as upgrades. Remote training is an operator
convenience, **never a CI dependency**: CI trains locally, and no rust test
touches the network (repo norm; there is no exception here).

## 7. versioning

`model_version` is read exact-match: this build accepts exactly **0**. Any
change to the feature spec, the node semantics, or the wire fields bumps the
version with an ADR — the same policy as `manifest_version`,
`contract_version`, and `dsl_version`. New featurizers arrive as new
`features.kind` values under a version bump, so a reader never guesses what
a number meant. Rationale and alternatives:
spec/adr/0006-distillation.md.

## 8. neural specialists (mlp v0)

The first neural member of the family (ADR-0009): a **single-hidden-layer
relu MLP** over the same frozen trigram features (§2), for residue where a
single tree's axis-aligned splits measurably fall short. Everything around
it is deliberately unchanged; this section is the complete delta.

- **Wire format.** One JSON document, canonical form, read strictly by
  `auto_model::Mlp::from_json` (`crates/auto-model/src/mlp.rs` — code
  wins): `mlp_version` (exactly **0**, versioned independently of
  `model_version` under §7's policy), `features` (§2 unchanged — same
  kind, same pinned vectors), `hidden_weights` (row-major
  `[hidden][buckets]`), `hidden_bias`, `out_weights` (`[classes][hidden]`),
  `out_bias`, `classes` (the output labels, argmax-indexed; the trainer
  writes them sorted). Weights are **plain float lists** — no tensor
  container format. Readers reject unknown fields, shape mismatches,
  non-finite weights, zero hidden units, and fewer than two classes.
  Inference: featurize (§2), relu hidden layer, argmax over the output
  logits; **ties break toward the lowest class index** — a pinned
  convention, tested on both sides of the language boundary, not an
  accident of float order.
- **Trainer protocol identity.** `mlp_train.py` speaks §4 verbatim: the
  same driver-appended flags, one metrics line of the same shape (its
  `trainer` field is `"mlp_train.py torch-<version> seed=<seed>"`), the
  same exit codes as `tree_train.py`, the same
  model-written-pass-or-fail behavior, the same seed determinism
  (`torch.manual_seed` plus strict deterministic-algorithm flags;
  byte-identical model json on one torch build and device — torch promises
  nothing across releases or between cpu and gpu, so neither does the
  trainer). It adds `--hidden`, `--epochs`, `--lr`, `--weight-decay`
  (AdamW's decoupled decay; 0 = plain Adam) and nothing else.
  Featurization is **imported from `tree_train.py`**, never reimplemented;
  metrics are measured by re-running the **exported** weights with a
  pure-python forward pass, never by the torch module's own predictions.
  torch missing at train time is an exit-2 protocol error naming the
  intended executor (Modal).
- **The honesty rules are unchanged.** The same emit gate (§5): contract
  verification plus differential exact replay of every witnessed input
  through the mlp-interpreter wasm (same ABI + `init` extension, mlp json
  as the payload, zero imports, failures trap). Holdout metrics remain
  provenance, never acceptance; divergent signatures still refuse.
- **torch is not a CI dependency.** CI proves the inference side —
  `crates/auto-passes/tests/mlp_parity.rs` drives hand-built MLPs through
  the embedded wasm interpreter against native inference, byte-equal,
  plus every trap path — and `mlp_train.py --self-test` runs torch-free.
  Training itself is verified by **Modal evidence runs**
  (`modal_mlp_train.py`), recorded like every other measured number. No
  rust test touches the network; nothing here is an exception.
- **GPU.** The Modal training function requests a real GPU (`gpu="A10G"`)
  — honestly optional at this scale (a default-shape MLP, 1024 buckets ×
  64 hidden, trains in seconds on cpu) and wired live anyway, because this
  is the exact remote-training profile the constitution's 0.5–3b
  specialists inherit. What those still need is recorded in
  spec/adr/open-questions.md, not claimed here.
