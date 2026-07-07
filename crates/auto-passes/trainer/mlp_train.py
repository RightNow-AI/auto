#!/usr/bin/env python3
"""Distillation trainer, neural rung: observation JSONL -> torch MLP (one
relu hidden layer) -> the frozen mlp json that crates/auto-model/src/mlp.rs
loads and the mlp-interpreter artifact runs.

Same trainer protocol as tree_train.py (flags, one metrics line on stdout,
exit codes, model written pass-or-fail, seed determinism, the optional
per-row "weight" witness count of ADR-0031 — here as per-example loss
weighting); adds --hidden/--epochs/--lr/--weight-decay. The FROZEN feature
spec is imported from tree_train
(featurize, the pinned fnv1a) — one implementation, never duplicated. The
export matches auto-model's mlp wire struct field-for-field: mlp_version 0,
features, hidden_weights [hidden][buckets], hidden_bias, out_weights
[classes][hidden], out_bias, classes (sorted) — plain float lists, no
tensor container. Inference argmax ties break toward the LOWEST class
index, same as the rust reader.

torch is imported lazily inside training: --self-test and observation
parsing need no torch. Without torch the trainer exits 2 naming the
intended executor (Modal, modal_mlp_train.py); torch is deliberately NOT a
CI dependency — CI proves the inference side via
crates/auto-passes/tests/mlp_parity.rs.

Exit codes: 0 trained and the holdout gate passed; 2 invalid observations
or params, or torch missing (no model written); 3 gate failed (metrics
printed, model.json written — an honest below-threshold result is data,
not a crash).
"""

from __future__ import annotations

import argparse
import math
import os
import sys

import tree_train
from tree_train import TrainerError, featurize

MLP_VERSION = 0


def export_mlp(
    hidden_weights: list[list[float]],
    hidden_bias: list[float],
    out_weights: list[list[float]],
    out_bias: list[float],
    classes: list[str],
    *,
    buckets: int,
    input_field: str | None,
) -> dict:
    """The frozen mlp wire dict, field-for-field against auto-model's Wire
    struct (crates/auto-model/src/mlp.rs). Plain float lists only."""
    features: dict = {"kind": tree_train.FEATURE_KIND, "buckets": buckets}
    if input_field is not None:
        features["input_field"] = input_field
    return {
        "mlp_version": MLP_VERSION,
        "features": features,
        "hidden_weights": hidden_weights,
        "hidden_bias": hidden_bias,
        "out_weights": out_weights,
        "out_bias": out_bias,
        "classes": classes,
    }


def infer(model: dict, text: str) -> str:
    """Forward pass over the EXPORTED wire dict, pure python f64 — the same
    arithmetic auto-model runs: featurize, relu hidden layer, argmax over
    logits, ties -> the lowest class index (strict > comparison). The
    verification path re-derives everything from the export, never from the
    torch module."""
    x = featurize(text, model["features"]["buckets"])
    hidden = [
        max(sum(w * v for w, v in zip(row, x, strict=True)) + b, 0.0)
        for row, b in zip(model["hidden_weights"], model["hidden_bias"], strict=True)
    ]
    best, best_logit = 0, float("-inf")
    for i, (row, b) in enumerate(zip(model["out_weights"], model["out_bias"], strict=True)):
        logit = sum(w * h for w, h in zip(row, hidden, strict=True)) + b
        if logit > best_logit:
            best, best_logit = i, logit
    return model["classes"][best]


def train_and_eval(
    texts: list[str],
    labels: list[str],
    *,
    buckets: int,
    holdout: float,
    seed: int,
    hidden: int,
    epochs: int,
    lr: float,
    weight_decay: float,
    input_field: str | None,
    weights: list[int] | None = None,
) -> tuple[dict, dict]:
    """Split (tree_train.split_indices — the identical deterministic split),
    fit a 1-hidden-layer relu MLP (CrossEntropyLoss, AdamW, full-batch — n is
    the distinct-observation count, small by construction), export plain
    float lists, then verify the EXPORT on both splits with the pure-python
    forward pass. Returns (model, metrics); applies no gate — the caller
    decides.

    weight_decay is AdamW's decoupled decay (0.0 = plain Adam, exactly). At
    distillation scale it is the generalization knob that matters: feature
    buckets absent from every training text get no data gradient, so without
    decay their weights keep random init noise and unseen inputs inherit it —
    measured as short holdout docs misrouting on never-trained trigrams.
    Decay shrinks exactly those weights toward zero.

    Determinism: torch.manual_seed(seed) fixes the init;
    use_deterministic_algorithms(True) (strict — nondeterministic kernels
    error instead of running), cudnn.benchmark off, and
    CUBLAS_WORKSPACE_CONFIG (set before torch import) fix the kernels. Same
    observations + flags + torch build + device => byte-identical
    model.json. torch does not promise identity across releases or between
    cpu and gpu, and neither does this trainer.

    weights (ADR-0031): per-row witness counts. None, or all-ones, IS the
    weightless protocol — the exact CrossEntropyLoss() mean path, untouched.
    Any weight > 1 switches the loss to per-example weighting,
    (ce_i * w_i).sum() / w.sum() — the same scalar as the mean loss over the
    witness-expanded batch, without materializing repeats — and the metrics
    line gains weighted_train_accuracy (witness mass reproduced / total
    train mass) and train_weight (that total). train_accuracy stays the
    plain fraction of training rows; holdout_accuracy stays PLAIN
    UNWEIGHTED. Conflicting labels for one input are the point: the loss
    sees the witnessed distribution and the weights arbitrate it.
    """
    # deterministic cublas needs this set before the first cuda context
    os.environ.setdefault("CUBLAS_WORKSPACE_CONFIG", ":4096:8")
    try:
        import torch  # lazy: --self-test and parse errors must not need torch
    except ImportError:
        raise TrainerError(
            "torch is not installed; mlp_train.py trains where torch exists — "
            "the intended executor is Modal "
            "(modal run crates/auto-passes/trainer/modal_mlp_train.py). "
            "torch is deliberately not a CI dependency."
        ) from None

    if weights is not None and len(weights) != len(texts):
        raise TrainerError(f"{len(weights)} weights for {len(texts)} observations")
    weighted = weights is not None and any(w != 1 for w in weights)
    classes = sorted(set(labels))
    if len(classes) < 2:
        raise TrainerError(
            f"need at least 2 distinct labels, got {classes!r}: the mlp wire "
            "format rejects single-class heads (constant behavior is a "
            "synthesis target, not a training one)"
        )
    train_idx, holdout_idx = tree_train.split_indices(len(texts), holdout, seed)
    if not train_idx:
        raise TrainerError(
            f"no training examples left ({len(texts)} observations, holdout {holdout}); "
            "reduce --holdout or add data"
        )

    torch.manual_seed(seed)
    torch.use_deterministic_algorithms(True)
    torch.backends.cudnn.benchmark = False
    device = torch.device("cuda" if torch.cuda.is_available() else "cpu")
    class_index = {c: i for i, c in enumerate(classes)}
    x_train = torch.tensor(
        [featurize(texts[i], buckets) for i in train_idx],
        dtype=torch.float32,
        device=device,
    )
    y_train = torch.tensor([class_index[labels[i]] for i in train_idx], device=device)
    net = torch.nn.Sequential(
        torch.nn.Linear(buckets, hidden),
        torch.nn.ReLU(),
        torch.nn.Linear(hidden, len(classes)),
    ).to(device)
    optimizer = torch.optim.AdamW(net.parameters(), lr=lr, weight_decay=weight_decay)
    net.train()
    if weighted:
        assert weights is not None
        loss_fn = torch.nn.CrossEntropyLoss(reduction="none")
        w_train = torch.tensor(
            [float(weights[i]) for i in train_idx], dtype=torch.float32, device=device
        )
        w_total = w_train.sum()
        for _ in range(epochs):
            optimizer.zero_grad()
            ((loss_fn(net(x_train), y_train) * w_train).sum() / w_total).backward()
            optimizer.step()
    else:
        # the weightless path, byte-for-byte: CrossEntropyLoss() mean
        loss_fn = torch.nn.CrossEntropyLoss()
        for _ in range(epochs):
            optimizer.zero_grad()
            loss_fn(net(x_train), y_train).backward()
            optimizer.step()

    # nn.Linear stores weight as [out_features, in_features] — already the
    # wire's row-major [hidden][buckets] / [classes][hidden]; .tolist() turns
    # float32 tensors into plain python floats (exact f32 -> f64 widening)
    model = export_mlp(
        net[0].weight.detach().cpu().tolist(),
        net[0].bias.detach().cpu().tolist(),
        net[2].weight.detach().cpu().tolist(),
        net[2].bias.detach().cpu().tolist(),
        classes,
        buckets=buckets,
        input_field=input_field,
    )
    flat = (
        [w for row in model["hidden_weights"] for w in row]
        + model["hidden_bias"]
        + [w for row in model["out_weights"] for w in row]
        + model["out_bias"]
    )
    if not all(math.isfinite(w) for w in flat):
        raise TrainerError(
            "training diverged: exported weights are not finite (auto-model "
            "rejects non-finite weights); lower --lr or --epochs"
        )

    def accuracy(idx: list[int]) -> float:
        if not idx:
            return 1.0  # vacuous: an empty holdout has nothing to get wrong
        good = sum(1 for i in idx if infer(model, texts[i]) == labels[i])
        return good / len(idx)

    metrics = {
        "train_accuracy": accuracy(train_idx),
        "holdout_accuracy": accuracy(holdout_idx),
        "train_n": len(train_idx),
        "holdout_n": len(holdout_idx),
        "classes": classes,
        "trainer": f"mlp_train.py torch-{torch.__version__} seed={seed}",
    }
    if weighted:
        assert weights is not None
        total = sum(weights[i] for i in train_idx)
        good = sum(weights[i] for i in train_idx if infer(model, texts[i]) == labels[i])
        metrics["weighted_train_accuracy"] = good / total
        metrics["train_weight"] = total
    return model, metrics


def self_test() -> None:
    """tree_train's own pinned assertions (imported, never duplicated: the
    fnv1a vectors and featurize behavior ARE tree_train's) plus the mlp
    forward pass on hand-built wire dicts — relu, argmax, ties -> lowest
    class index, mirroring auto-model's mlp tests. Needs no files and no
    torch."""
    tree_train.self_test()  # pinned fnv1a vectors + featurize sanity
    # mirrors crates/auto-model/src/mlp.rs `tiny`: hidden = relu(x0 - x1),
    # logits = [h, -h + 0.1] -> "a" iff h > 0.1 else "b"
    tiny = export_mlp(
        [[1.0, -1.0]],
        [0.0],
        [[1.0], [-1.0]],
        [0.0, 0.1],
        ["a", "b"],
        buckets=2,
        input_field=None,
    )
    x = featurize("aaa", 2)
    h = max(x[0] - x[1], 0.0)
    assert infer(tiny, "aaa") == ("a" if h > 0.1 else "b")
    tie = export_mlp(
        [[0.0, 0.0]],
        [0.0],
        [[0.0], [0.0]],
        [0.5, 0.5],
        ["a", "b"],
        buckets=2,
        input_field=None,
    )
    assert infer(tie, "anything") == "a"  # equal logits: lowest index wins


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="train a 1-hidden-layer relu MLP (torch) over frozen "
        "char-trigram fnv1a features; emit mlp json readable by "
        "crates/auto-model"
    )
    parser.add_argument(
        "--observations",
        help='observation jsonl: one {"input":..,"output":..} per line; '
        'optional "weight" = witness count >= 1, absent = 1 (ADR-0031)',
    )
    parser.add_argument("--out", help="where to write model.json")
    parser.add_argument(
        "--input-field",
        default=None,
        help="object field holding the text; omitted = the input IS the text",
    )
    parser.add_argument("--buckets", type=int, default=1024, help="feature-vector width")
    parser.add_argument("--holdout", type=float, default=0.25, help="held-out fraction in [0,1)")
    parser.add_argument(
        "--seed", type=int, default=0, help="split shuffle + torch.manual_seed"
    )
    parser.add_argument(
        "--min-holdout-accuracy",
        type=float,
        default=1.0,
        help="gate: exit 3 when holdout accuracy is below this",
    )
    parser.add_argument("--hidden", type=int, default=64, help="hidden-layer width")
    parser.add_argument("--epochs", type=int, default=200, help="full-batch epochs")
    parser.add_argument("--lr", type=float, default=0.01, help="AdamW learning rate")
    parser.add_argument(
        "--weight-decay",
        type=float,
        default=0.0,
        help="AdamW decoupled weight decay (0 = plain Adam); shrinks weights "
        "on feature buckets no training text touched, so unseen inputs stop "
        "inheriting init noise",
    )
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="assert pinned fnv1a vectors (via tree_train) + mlp forward-pass "
        "pins, print ok, exit 0; needs no torch",
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    if args.self_test:
        self_test()
        print("ok")
        return 0
    if args.observations is None or args.out is None:
        parser.error("--observations and --out are required (unless --self-test)")
    if args.buckets < 1:
        parser.error("--buckets must be >= 1")
    if not 0.0 <= args.holdout < 1.0:
        parser.error("--holdout must be in [0, 1)")
    if args.hidden < 1:
        parser.error("--hidden must be >= 1")
    if args.epochs < 1:
        parser.error("--epochs must be >= 1")
    if args.lr <= 0.0:
        parser.error("--lr must be > 0")
    if args.weight_decay < 0.0:
        parser.error("--weight-decay must be >= 0")
    try:
        with open(args.observations, encoding="utf-8-sig") as f:
            raw = f.read()
    except OSError as e:
        print(f"error: cannot read {args.observations}: {e}", file=sys.stderr)
        return 2
    try:
        texts, labels, weights = tree_train.parse_weighted_observations(raw, args.input_field)
        model, metrics = train_and_eval(
            texts,
            labels,
            buckets=args.buckets,
            holdout=args.holdout,
            seed=args.seed,
            hidden=args.hidden,
            epochs=args.epochs,
            lr=args.lr,
            weight_decay=args.weight_decay,
            input_field=args.input_field,
            weights=weights,
        )
    except TrainerError as e:
        print(f"error: {e}", file=sys.stderr)
        return 2
    # model.json is written pass or fail — the gate decides acceptance
    with open(args.out, "w", encoding="utf-8", newline="\n") as f:
        f.write(tree_train.dumps_canonical(model) + "\n")
    print(tree_train.dumps_metrics(metrics))
    passed = metrics["train_n"] > 0 and metrics["holdout_accuracy"] >= args.min_holdout_accuracy
    return 0 if passed else 3


if __name__ == "__main__":
    sys.exit(main())
