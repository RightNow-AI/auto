"""Modal wrapper around mlp_train — the SAME training, run remotely on GPU.

One implementation, two frontends: this file imports mlp_train's functions
and only moves bytes + applies the same holdout gate. Written against
modal 1.3.0 (modal.Mount is gone; modules ship to the container via
image.add_local_python_source, which puts them on the container PYTHONPATH
— both mlp_train and tree_train, because mlp_train imports the frozen
featurizer from tree_train).

usage (profile rightnow-ai):
    modal run crates/auto-passes/trainer/modal_mlp_train.py \
        --observations obs.jsonl --out model.json

The GPU is REAL here — `gpu="A10G"`, the slot modal_tree_train.py reserved.
Honest note: at this scale it is optional, not necessary — a default-shape
mlp (1024 buckets x 64 hidden, full batch) trains in seconds on cpu — but
it is wired live because this function is the exact remote-training profile
the constitution's 0.5-3b specialists inherit, and proving the GPU wiring
(deterministic cuda kernels included) is the point of this rung
(spec/adr/0009-neural-distillation.md).
"""

from __future__ import annotations

import json
from pathlib import Path

import modal

import mlp_train
import tree_train

app = modal.App("auto-distill-mlp-trainer")

image = (
    modal.Image.debian_slim()
    .pip_install("torch")
    .add_local_python_source("mlp_train", "tree_train")
)


@app.function(image=image, gpu="A10G")
def train_remote(
    observations_text: str,
    input_field: str = "",
    buckets: int = 1024,
    holdout: float = 0.25,
    seed: int = 0,
    hidden: int = 64,
    epochs: int = 200,
    lr: float = 0.01,
    weight_decay: float = 0.0,
) -> tuple[str, str]:
    """Train on observation JSONL text; return (model_json, metrics_json).

    input_field "" means unset (the input IS the text) — modal's CLI has no
    None, so empty string is the sentinel.
    """
    field = input_field or None
    texts, labels = tree_train.parse_observations(observations_text, field)
    model, metrics = mlp_train.train_and_eval(
        texts,
        labels,
        buckets=buckets,
        holdout=holdout,
        seed=seed,
        hidden=hidden,
        epochs=epochs,
        lr=lr,
        weight_decay=weight_decay,
        input_field=field,
    )
    return tree_train.dumps_canonical(model), tree_train.dumps_metrics(metrics)


@app.local_entrypoint()
def main(
    observations: str,
    out: str,
    input_field: str = "",
    buckets: int = 1024,
    holdout: float = 0.25,
    seed: int = 0,
    min_holdout_accuracy: float = 1.0,
    hidden: int = 64,
    epochs: int = 200,
    lr: float = 0.01,
    weight_decay: float = 0.0,
) -> None:
    """Read the local observations file, train remotely, write model.json,
    print the metrics line. Exits 3 when the holdout gate fails — the same
    protocol as mlp_train.py (model.json is still written)."""
    text = Path(observations).read_text(encoding="utf-8-sig")
    model_json, metrics_json = train_remote.remote(
        text,
        input_field=input_field,
        buckets=buckets,
        holdout=holdout,
        seed=seed,
        hidden=hidden,
        epochs=epochs,
        lr=lr,
        weight_decay=weight_decay,
    )
    Path(out).write_text(model_json + "\n", encoding="utf-8", newline="\n")
    print(metrics_json)
    metrics = json.loads(metrics_json)
    if not (metrics["train_n"] > 0 and metrics["holdout_accuracy"] >= min_holdout_accuracy):
        raise SystemExit(3)
