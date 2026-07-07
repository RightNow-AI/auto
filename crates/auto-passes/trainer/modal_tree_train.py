"""Modal wrapper around tree_train — the SAME training, run remotely.

One implementation, two frontends: this file imports tree_train's functions
and only moves bytes + applies the same holdout gate. Written against
modal 1.3.0 (modal.Mount is gone; the module ships to the container via
image.add_local_python_source, which puts it on the container PYTHONPATH).

usage (profile rightnow-ai):
    modal run crates/auto-passes/trainer/modal_tree_train.py \
        --observations obs.jsonl --out model.json

A decision tree does not need a GPU and gets none here; this path exists to
prove the remote train/eval/accept loop and to host future torch trainers.
"""

from __future__ import annotations

import json
from pathlib import Path

import modal

import tree_train

app = modal.App("auto-distill-trainer")

image = (
    modal.Image.debian_slim()
    .pip_install("scikit-learn==1.9.0")
    .add_local_python_source("tree_train")
)


@app.function(
    image=image,
    # gpu="A10G",  # future torch trainers flip this on; a tree gains nothing from it
)
def train_remote(
    observations_text: str,
    input_field: str = "",
    buckets: int = 1024,
    holdout: float = 0.25,
    seed: int = 0,
    max_depth: int = 12,
) -> tuple[str, str]:
    """Train on observation JSONL text; return (model_json, metrics_json).

    input_field "" means unset (the input IS the text) — modal's CLI has no
    None, so empty string is the sentinel.
    """
    field = input_field or None
    texts, labels = tree_train.parse_observations(observations_text, field)
    model, metrics = tree_train.train_and_eval(
        texts,
        labels,
        buckets=buckets,
        holdout=holdout,
        seed=seed,
        max_depth=max_depth,
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
    max_depth: int = 12,
) -> None:
    """Read the local observations file, train remotely, write model.json,
    print the metrics line. Exits 3 when the holdout gate fails — the same
    protocol as tree_train.py (model.json is still written)."""
    text = Path(observations).read_text(encoding="utf-8-sig")
    model_json, metrics_json = train_remote.remote(
        text,
        input_field=input_field,
        buckets=buckets,
        holdout=holdout,
        seed=seed,
        max_depth=max_depth,
    )
    Path(out).write_text(model_json + "\n", encoding="utf-8", newline="\n")
    print(metrics_json)
    metrics = json.loads(metrics_json)
    if not (metrics["train_n"] > 0 and metrics["holdout_accuracy"] >= min_holdout_accuracy):
        raise SystemExit(3)
