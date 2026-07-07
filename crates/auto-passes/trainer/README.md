# trainer — the distillation train/eval loop (S5)

`tree_train.py` turns observation JSONL into the frozen model json that
`crates/auto-model` loads and the artifact model-interpreter runs. stdlib +
scikit-learn only.

## protocol

```
python tree_train.py --observations obs.jsonl --out model.json \
    [--input-field TEXT] [--buckets 1024] [--holdout 0.25] [--seed 0] \
    [--min-holdout-accuracy 1.0] [--max-depth 12] [--self-test]
```

- observations: one `{"input": <json>, "output": <json>}` per line (blank
  lines skipped). `output` must be a json string — the label. text =
  `input[--input-field]` when given (must be a string), else the input
  itself (must be a string). optional `"weight"`: the row's witness count,
  a json integer >= 1, absent = 1 (ADR-0031). a file whose weights are all
  1 trains and reports byte-identically to a weightless file.
- metrics: exactly one json line on stdout:
  `{"train_accuracy":..,"holdout_accuracy":..,"train_n":..,"holdout_n":..,"classes":[..],"trainer":"tree_train.py sklearn-1.9.0 seed=0"}`
  — measured by re-running the EXPORTED model (a python tree walk over the
  wire dict), not sklearn's predict. what ships is what was measured.
- weights (ADR-0031): any weight > 1 engages sklearn `sample_weight` — the
  split criterion and every leaf argmax count witness mass instead of rows
  — and the metrics line gains `weighted_train_accuracy` (witness mass
  reproduced / total train mass, the objective the fit saw) and
  `train_weight` (that total). `train_accuracy` stays the PLAIN fraction of
  training rows and `holdout_accuracy` stays PLAIN UNWEIGHTED — measured
  reality, not the training trick. conflicting labels for one input (a
  divergent group's witnessed outputs) are the point: 100% plain accuracy
  is then impossible by construction and the shortfall is the recorded
  disagreement, reported, never smoothed. `train_n`/`holdout_n` keep
  counting rows; a divergent group contributes one row per distinct
  witnessed output and those rows split independently, so a held-out
  minority witness that the model (correctly) answers with the majority
  counts as a plain miss.
- exit codes: `0` trained and the holdout gate passed; `2` invalid
  observations or params (no model written); `3` holdout accuracy below
  `--min-holdout-accuracy` — metrics still printed, model.json still
  written. an honest below-threshold result is data; the gate decides.
- `--holdout 0` disables the gate: an empty holdout counts as vacuously
  perfect (1.0).
- `--self-test` asserts the pinned fnv1a vectors and a featurize sanity
  case, prints `ok`, exits 0. needs no files and no sklearn.

## determinism

same observations + same flags => byte-identical model.json (on the same
sklearn/numpy build): the split shuffle is `random.Random(seed)`, the tree
is `DecisionTreeClassifier(random_state=seed, max_depth=...)`, and the json
is dumped with sorted keys and compact separators — the same key order as
auto-model's `to_json`.

## feature spec (frozen)

the featurizer must match `crates/auto-model/src/lib.rs` bit-for-bit:
lowercase (str.lower), char trigrams including spaces, FNV-1a 32-bit
(offset 2166136261, prime 16777619) over the trigram's utf-8 bytes,
`hash % buckets`, occurrence counts as floats; `count <= threshold` goes
LEFT (sklearn semantics). pinned vectors, asserted by `--self-test` here
and by auto-model's tests: `fnv1a("") = 2166136261`,
`fnv1a("a") = 0xE40C292C`, `fnv1a("abc") = 0x1A47E90B`.

## tests

```
python -m pytest crates/auto-passes/trainer -q
```

first-class trainer tests (before ADR-0031 only `--self-test` and the
rust-side protocol tests existed): weight parsing back-compat and
strictness, a fixture where `sample_weight` flips the trained majority, the
measured weighted-vs-most-common divergence fixture (ADR-0031), mlp loss
weighting flipping a prediction, and all-ones == weightless byte identity
for both trainers. sklearn/torch tests skip where those are absent — torch
stays a non-CI dependency. `--self-test` additionally pins the weight-parse
rules stdlib-only.

## modal

the modal wrappers are weight-blind today: they call
`parse_observations`, which REFUSES a file carrying any weight != 1
instead of silently dropping witness counts. weightless files behave
exactly as before; threading weights through the wrappers is a
three-line change when remote weighted training is needed.

```
modal run crates/auto-passes/trainer/modal_tree_train.py \
    --observations obs.jsonl --out model.json
```

(modal profile `rightnow-ai`; same flags as the cli, `--input-field ""`
means unset.) `modal_tree_train.py` imports tree_train's functions — one
implementation, two frontends. honest note: a decision tree does not need
a GPU and gets none; the modal path exists to prove the remote
train/eval/accept loop and to host future torch trainers (the commented
`gpu="A10G"` line in `modal_tree_train.py` is where they turn it on).
written against modal 1.3.0: `modal.Mount` is gone, the module ships via
`image.add_local_python_source("tree_train")`.

## mlp_train.py — the neural rung

same protocol, torch instead of sklearn: observation JSONL in, the frozen
**mlp json** out (`crates/auto-model/src/mlp.rs`, `mlp_version` 0 — a
single-hidden-layer relu MLP as plain float lists; `classes` sorted;
argmax ties break toward the lowest class index).

```
python mlp_train.py --observations obs.jsonl --out model.json \
    [--input-field TEXT] [--buckets 1024] [--holdout 0.25] [--seed 0] \
    [--min-holdout-accuracy 1.0] [--hidden 64] [--epochs 200] [--lr 0.01] \
    [--weight-decay 0.0] [--self-test]
```

- identical observation format (including the optional `"weight"` witness
  count, ADR-0031 — here applied as per-example loss weighting:
  `(ce_i * w_i).sum() / w.sum()`, the same scalar as the mean loss over a
  witness-expanded batch without materializing repeats; all-ones weights
  keep the exact `CrossEntropyLoss()` mean path and the unchanged metrics
  line, and any weight > 1 adds the same honestly-labeled
  `weighted_train_accuracy`/`train_weight` fields — holdout stays plain
  unweighted), metrics-line shape (trainer field:
  `mlp_train.py torch-<version> seed=0`; `classes` are all witnessed
  labels, sorted — the exported head must represent every recorded label),
  exit codes, and model-written-pass-or-fail behavior as tree_train.py.
  accuracy is measured by re-running the EXPORTED weights with a
  pure-python forward pass (relu, argmax, ties → lowest class index) —
  never torch's own predictions. what ships is what was measured.
- featurization is IMPORTED from tree_train (`featurize`, the pinned
  fnv1a) — one implementation, never duplicated. `--self-test` runs
  tree_train's pinned assertions plus mlp forward-pass pins (including the
  ties-low case), prints `ok`, exits 0; needs no files and no torch.
- torch imports lazily inside training. no torch → exit 2 with a message
  naming the intended executor: Modal (`modal_mlp_train.py`). torch is
  deliberately NOT a CI dependency — CI proves the inference side
  (`crates/auto-passes/tests/mlp_parity.rs`); training runs are Modal
  evidence runs (spec/distillation.md §8).
- training: full-batch AdamW over CrossEntropyLoss for `--epochs` rounds —
  at distillation's distinct-observation scale full batch is exact and
  cheap. `--weight-decay` is AdamW's decoupled decay (0 = plain Adam,
  exactly); it shrinks weights on feature buckets no training text touched,
  which otherwise keep init noise that unseen inputs inherit. fewer than 2
  distinct labels refuses (exit 2): the wire format rejects single-class
  heads. non-finite trained weights refuse (exit 2) instead of writing
  json auto-model would reject.
- with n this small, hyperparameters and `--seed` (init + split) decide
  whether the trained model reproduces every recorded observation; sweep
  them freely — seeds are random restarts, and the emit gate cannot be
  gamed by the choice because `auto distill` differentially replays every
  distinct recorded input whatever the holdout said.
- determinism: `torch.manual_seed(seed)`,
  `torch.use_deterministic_algorithms(True)` (strict), cudnn benchmark
  off, `CUBLAS_WORKSPACE_CONFIG=:4096:8` set before the torch import.
  same observations + same flags + same torch build + same device =>
  byte-identical model.json. cross-device (cpu vs gpu) or cross-release
  byte-identity is NOT claimed — torch does not promise it.

```
modal run crates/auto-passes/trainer/modal_mlp_train.py \
    --observations obs.jsonl --out model.json
```

runs the SAME functions remotely with a REAL gpu (`gpu="A10G"` — the slot
the tree wrapper reserved): optional at this scale, a default-shape mlp
trains in seconds on cpu; wired live because this is the exact
remote-training profile the constitution's 0.5–3b specialists inherit
(spec/adr/0009-neural-distillation.md). the image ships both
`mlp_train` and `tree_train` via `add_local_python_source` — mlp_train
imports the frozen featurizer from tree_train.

to train on modal INSIDE the emit gate, pass `-q` so modal's progress
output does not displace the metrics line (the protocol reads the LAST
non-empty stdout line):

```
auto distill --contract ... --store ... --model-kind mlp --input-field text \
    --trainer "modal run -q crates/auto-passes/trainer/modal_mlp_train.py"
```
