"""Trainer tests (pytest) — the first ones; before ADR-0031 the trainers
were covered only by their --self-test pins and the rust-side protocol
tests in crates/auto-passes/src/distillation.rs.

Run from this directory (the trainers import each other as top-level
modules, exactly as the driver invokes them):

    python -m pytest crates/auto-passes/trainer -q

sklearn tests skip when sklearn is absent; torch tests skip when torch is
absent (torch is deliberately not a CI dependency — the same policy as
mlp_train itself). Everything here is local: no network, no paid calls.
"""

from __future__ import annotations

import json

import pytest

import mlp_train
import tree_train
from tree_train import TrainerError


def jsonl(rows: list[tuple[str, str] | tuple[str, str, int]]) -> str:
    """Observation JSONL from (text, label) or (text, label, weight) rows."""
    lines = []
    for row in rows:
        obs: dict = {"input": row[0], "output": row[1]}
        if len(row) == 3:
            obs["weight"] = row[2]
        lines.append(json.dumps(obs))
    return "\n".join(lines) + "\n"


# ---- weight parsing: back-compat and strictness (stdlib only) ----


def test_absent_weight_parses_as_one():
    texts, labels, weights = tree_train.parse_weighted_observations(
        jsonl([("aaa", "x"), ("bbb", "y", 3)]), None
    )
    assert texts == ["aaa", "bbb"]
    assert labels == ["x", "y"]
    assert weights == [1, 3]


@pytest.mark.parametrize(
    "weight_json",
    ["0", "-2", "1.5", "2.0", "true", '"3"', "null", "[1]"],
)
def test_bad_weights_are_rejected(weight_json):
    line = '{"input":"aaa","output":"x","weight":%s}\n' % weight_json
    with pytest.raises(TrainerError, match="weight"):
        tree_train.parse_weighted_observations(line, None)


def test_weight_blind_entrypoint_refuses_real_weights():
    # parse_observations (the modal wrappers' entrypoint) must never
    # silently drop witness counts
    with pytest.raises(TrainerError, match="weight-blind"):
        tree_train.parse_observations(jsonl([("aaa", "x", 2)]), None)
    # weight 1 IS the weightless protocol: accepted, identical result
    assert tree_train.parse_observations(jsonl([("aaa", "x", 1)]), None) == (
        ["aaa"],
        ["x"],
    )


def test_weight_count_mismatch_is_refused():
    pytest.importorskip("sklearn")
    with pytest.raises(TrainerError, match="weights for"):
        tree_train.train_and_eval(
            ["aaa", "bbb"],
            ["x", "y"],
            buckets=64,
            holdout=0.0,
            seed=0,
            max_depth=4,
            input_field=None,
            weights=[1],
        )


# ---- tree: sample_weight applied, all-ones collapse, honest metrics ----


def tree_fit(rows, holdout=0.0, seed=0):
    pytest.importorskip("sklearn")
    texts, labels, weights = tree_train.parse_weighted_observations(jsonl(rows), None)
    return tree_train.train_and_eval(
        texts,
        labels,
        buckets=1024,
        holdout=holdout,
        seed=seed,
        max_depth=12,
        input_field=None,
        weights=weights,
    )


def test_tree_sample_weight_flips_the_majority():
    # one input text ("aa" is under 3 chars: zero features, a single leaf).
    # By rows the majority is x (2 of 3); by witness weight it is y (5 of 7).
    rows = [("aa", "x"), ("aa", "x"), ("aa", "y", 5)]
    weightless = [("aa", "x"), ("aa", "x"), ("aa", "y")]
    model_w, metrics_w = tree_fit(rows)
    model_p, _ = tree_fit(weightless)
    assert tree_train.infer(model_p, "aa") == "x", "row majority without weights"
    assert tree_train.infer(model_w, "aa") == "y", "witness mass with sample_weight"
    # honest labels: plain train_accuracy counts rows (1 of 3 rows says y),
    # weighted_train_accuracy counts witness mass (5 of 7)
    assert metrics_w["train_accuracy"] == pytest.approx(1 / 3)
    assert metrics_w["weighted_train_accuracy"] == pytest.approx(5 / 7)
    assert metrics_w["train_weight"] == 7


def test_all_ones_weights_train_and_report_byte_identically():
    rows = [("alpha one", "x"), ("beta two", "y"), ("gamma three", "x"), ("delta four", "y")]
    explicit = [(t, o, 1) for (t, o) in rows]
    model_a, metrics_a = tree_fit(rows, holdout=0.25)
    model_b, metrics_b = tree_fit(explicit, holdout=0.25)
    assert tree_train.dumps_canonical(model_a) == tree_train.dumps_canonical(model_b)
    assert tree_train.dumps_metrics(metrics_a) == tree_train.dumps_metrics(metrics_b)
    assert "weighted_train_accuracy" not in metrics_a, "weightless line is unchanged"


def test_holdout_accuracy_stays_plain_and_unweighted():
    # 8 rows, two of them heavy; whatever lands in the holdout, the reported
    # holdout_accuracy must equal the PLAIN fraction recomputed here
    rows = [
        ("first sample text", "x", 9),
        ("second sample text", "y"),
        ("third sample text", "x", 9),
        ("fourth sample text", "y"),
        ("fifth sample text", "x"),
        ("sixth sample text", "y"),
        ("seventh sample text", "x"),
        ("eighth sample text", "y"),
    ]
    texts, labels, _ = tree_train.parse_weighted_observations(jsonl(rows), None)
    model, metrics = tree_fit(rows, holdout=0.25, seed=1)
    _, holdout_idx = tree_train.split_indices(len(texts), 0.25, 1)
    assert metrics["holdout_n"] == len(holdout_idx) > 0
    plain = sum(
        1 for i in holdout_idx if tree_train.infer(model, texts[i]) == labels[i]
    ) / len(holdout_idx)
    assert metrics["holdout_accuracy"] == pytest.approx(plain)


def test_weighted_vs_most_common_yield_different_trained_behavior():
    # THE ADR-0031 fixture: three distinct inputs whose texts featurize
    # identically (all under 3 chars: zero feature vectors), so the tree
    # cannot separate them — one leaf holds every group's rows.
    #   group A "aa": x witnessed 6, y witnessed 1   (majority x)
    #   group B "bb": y witnessed 2                  (majority y)
    #   group C "cc": y witnessed 2                  (majority y)
    # most-common trains on one row per group: leaf {x:1, y:2} -> "y".
    # weighted trains on every witness:        leaf {x:6, y:5} -> "x".
    most_common = [("aa", "x"), ("bb", "y"), ("cc", "y")]
    weighted = [("aa", "x", 6), ("aa", "y", 1), ("bb", "y", 2), ("cc", "y", 2)]
    model_mc, _ = tree_fit(most_common)
    model_w, _ = tree_fit(weighted)
    for text in ("aa", "bb", "cc"):
        assert tree_train.infer(model_mc, text) == "y", "most-common: group votes"
        assert tree_train.infer(model_w, text) == "x", "weighted: witness mass"


# ---- tree CLI end-to-end: weighted metrics line + exit codes ----


def test_cli_weighted_metrics_line(tmp_path, capsys):
    pytest.importorskip("sklearn")
    obs = tmp_path / "obs.jsonl"
    out = tmp_path / "model.json"
    obs.write_text(jsonl([("aa", "x"), ("aa", "x"), ("aa", "y", 5)]), encoding="utf-8")
    code = tree_train.main(
        [
            "--observations",
            str(obs),
            "--out",
            str(out),
            "--holdout",
            "0",
            "--min-holdout-accuracy",
            "0",
        ]
    )
    assert code == 0
    metrics = json.loads(capsys.readouterr().out.strip().splitlines()[-1])
    assert metrics["train_accuracy"] == pytest.approx(1 / 3)
    assert metrics["weighted_train_accuracy"] == pytest.approx(5 / 7)
    assert metrics["train_weight"] == 7
    assert metrics["train_n"] == 3
    model = json.loads(out.read_text(encoding="utf-8"))
    assert tree_train.infer(model, "aa") == "y"


# ---- mlp: per-example loss weighting applied (torch; skipped in CI) ----


def mlp_fit(rows, seed=0):
    pytest.importorskip("torch")
    texts, labels, weights = tree_train.parse_weighted_observations(jsonl(rows), None)
    return mlp_train.train_and_eval(
        texts,
        labels,
        buckets=32,
        holdout=0.0,
        seed=seed,
        hidden=8,
        epochs=200,
        lr=0.01,
        weight_decay=0.0,
        input_field=None,
        weights=weights,
    )


def test_mlp_loss_weighting_flips_prediction():
    # one zero-feature text with conflicting labels: only the loss weights
    # can arbitrate. 1-vs-9 toward y predicts y; mirrored weights predict x
    # (same seed, same rows, same order — the weights are the only change).
    model_y, metrics_y = mlp_fit([("aa", "x", 1), ("aa", "y", 9)])
    model_x, metrics_x = mlp_fit([("aa", "x", 9), ("aa", "y", 1)])
    assert mlp_train.infer(model_y, "aa") == "y"
    assert mlp_train.infer(model_x, "aa") == "x"
    for metrics in (metrics_y, metrics_x):
        assert metrics["train_accuracy"] == pytest.approx(1 / 2)
        assert metrics["weighted_train_accuracy"] == pytest.approx(9 / 10)
        assert metrics["train_weight"] == 10


def test_mlp_all_ones_weights_match_weightless_bytes():
    rows = [("alpha one", "x"), ("beta two", "y"), ("gamma three", "x"), ("delta four", "y")]
    model_a, metrics_a = mlp_fit(rows)
    model_b, metrics_b = mlp_fit([(t, o, 1) for (t, o) in rows])
    assert tree_train.dumps_canonical(model_a) == tree_train.dumps_canonical(model_b)
    assert tree_train.dumps_metrics(metrics_a) == tree_train.dumps_metrics(metrics_b)
    assert "weighted_train_accuracy" not in metrics_a
