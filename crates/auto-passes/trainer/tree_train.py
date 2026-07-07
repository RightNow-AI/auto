#!/usr/bin/env python3
"""Distillation trainer: observation JSONL -> sklearn decision tree -> the
frozen model json that crates/auto-model loads and artifacts interpret.

stdlib + scikit-learn only. The feature spec is FROZEN and must match
crates/auto-model/src/lib.rs bit-for-bit: lowercase the text (str.lower),
slide a 3-char window over its chars (spaces included), hash each trigram's
utf-8 bytes with FNV-1a 32-bit (offset 2166136261, prime 16777619), take
hash % buckets as the feature index, count occurrences as floats. Split
rule: count <= threshold goes LEFT (sklearn semantics, same as the rust
walker — orientation is preserved, never remapped).

Observation rows may carry an optional "weight" — a witness count >= 1,
absent = 1 (ADR-0031). Weights select training data only: sklearn's
sample_weight during the fit, a weighted_train_accuracy/train_weight pair
added to the metrics line. holdout_accuracy stays plain and unweighted. A
file whose weights are all 1 trains and reports byte-identically to a
weightless file.

Exit codes: 0 trained and the holdout gate passed; 2 invalid observations
or params (no model written); 3 gate failed (metrics printed, model.json
written — an honest below-threshold result is data, not a crash).
"""

from __future__ import annotations

import argparse
import json
import random
import sys

FNV_OFFSET = 2166136261
FNV_PRIME = 16777619
MODEL_VERSION = 0
FEATURE_KIND = "char_trigram_fnv1a"


class TrainerError(Exception):
    """Invalid observations or parameters (CLI exit 2)."""


def fnv1a_32(data: bytes) -> int:
    """FNV-1a 32-bit over bytes. Frozen constants — a drift here silently
    reshuffles every feature. Pinned vectors asserted by self_test()."""
    h = FNV_OFFSET
    for b in data:
        h ^= b
        h = (h * FNV_PRIME) & 0xFFFFFFFF
    return h


def featurize(text: str, buckets: int) -> list[float]:
    """Frozen spec: lowercase, char trigrams (spaces included),
    fnv1a % buckets, occurrence counts as floats."""
    counts = [0.0] * buckets
    chars = list(text.lower())
    for i in range(len(chars) - 2):
        trigram = "".join(chars[i : i + 3])
        counts[fnv1a_32(trigram.encode("utf-8")) % buckets] += 1.0
    return counts


def _type_name(value: object) -> str:
    """JSON type name for error messages."""
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "bool"
    if isinstance(value, (int, float)):
        return "number"
    if isinstance(value, str):
        return "string"
    if isinstance(value, list):
        return "array"
    return "object"


def parse_weighted_observations(
    text: str, input_field: str | None
) -> tuple[list[str], list[str], list[int]]:
    """Parse observation JSONL: one {"input": <json>, "output": <json>} per
    line (blank lines skipped), plus an OPTIONAL "weight" — the row's witness
    count (ADR-0031). Absent = 1: a weightless file parses exactly as before.
    A present weight must be a JSON integer >= 1 (a count, never a bool,
    float, or string). output must be a JSON string — the label. Text =
    input[input_field] when a field is given (input must be an object, the
    value a string), else the input itself (must be a string)."""
    texts: list[str] = []
    labels: list[str] = []
    weights: list[int] = []
    for lineno, line in enumerate(text.splitlines(), start=1):
        if not line.strip():
            continue
        try:
            obs = json.loads(line)
        except json.JSONDecodeError as e:
            raise TrainerError(f"line {lineno}: not valid json: {e}") from None
        if not isinstance(obs, dict) or "input" not in obs or "output" not in obs:
            raise TrainerError(f'line {lineno}: expected an object with "input" and "output"')
        output = obs["output"]
        if not isinstance(output, str):
            raise TrainerError(
                f"line {lineno}: output must be a JSON string (the label), got {_type_name(output)}"
            )
        weight = obs.get("weight", 1)
        # bool is an int subtype in python: reject it explicitly
        if isinstance(weight, bool) or not isinstance(weight, int) or weight < 1:
            raise TrainerError(
                f"line {lineno}: weight must be a JSON integer >= 1 (a witness "
                f"count), got {json.dumps(weight)}"
            )
        inp = obs["input"]
        if input_field is not None:
            if not isinstance(inp, dict):
                raise TrainerError(
                    f"line {lineno}: input must be an object with field {input_field!r}, "
                    f"got {_type_name(inp)}"
                )
            if input_field not in inp:
                raise TrainerError(f"line {lineno}: input has no field {input_field!r}")
            value = inp[input_field]
            if not isinstance(value, str):
                raise TrainerError(
                    f"line {lineno}: input[{input_field!r}] must be a string, "
                    f"got {_type_name(value)}"
                )
            texts.append(value)
        else:
            if not isinstance(inp, str):
                raise TrainerError(
                    f"line {lineno}: input must be a string (or pass --input-field), "
                    f"got {_type_name(inp)}"
                )
            texts.append(inp)
        labels.append(output)
        weights.append(weight)
    if not texts:
        raise TrainerError("no observations")
    return texts, labels, weights


def parse_observations(text: str, input_field: str | None) -> tuple[list[str], list[str]]:
    """The weight-blind entrypoint (kept for callers that thread no weights,
    e.g. the modal wrappers): parses exactly like parse_weighted_observations
    but REFUSES a file carrying any weight != 1 — dropping witness counts
    silently would train a different model than the file asks for."""
    texts, labels, weights = parse_weighted_observations(text, input_field)
    if any(w != 1 for w in weights):
        raise TrainerError(
            "observations carry witness weights but this entrypoint is "
            "weight-blind; use a weight-aware caller (tree_train.py / "
            "mlp_train.py CLIs thread weights; ADR-0031)"
        )
    return texts, labels


def split_indices(n: int, holdout: float, seed: int) -> tuple[list[int], list[int]]:
    """Deterministic split: shuffle 0..n with random.Random(seed); the first
    round(n*holdout) shuffled indices are held out (at least 1 when
    holdout > 0 and n >= 4). Returns (train, holdout) index lists."""
    indices = list(range(n))
    random.Random(seed).shuffle(indices)
    h = round(n * holdout)
    if holdout > 0 and n >= 4:
        h = max(h, 1)
    return indices[h:], indices[:h]


def export_tree(clf, *, buckets: int, input_field: str | None) -> dict:
    """sklearn tree_ -> the frozen wire dict. sklearn routes x <= threshold
    LEFT, exactly the frozen rule. Node 0 is the root (sklearn's own order);
    leaves take the argmax class; thresholds become plain floats."""
    tree = clf.tree_
    nodes: list[dict] = []
    for i in range(tree.node_count):
        left = int(tree.children_left[i])
        right = int(tree.children_right[i])
        if left == -1:  # sklearn TREE_LEAF; right is -1 iff left is
            class_index = int(tree.value[i][0].argmax())
            nodes.append({"leaf": {"label": str(clf.classes_[class_index])}})
        else:
            nodes.append(
                {
                    "split": {
                        "feature": int(tree.feature[i]),
                        "threshold": float(tree.threshold[i]),
                        "left": left,
                        "right": right,
                    }
                }
            )
    features: dict = {"kind": FEATURE_KIND, "buckets": buckets}
    if input_field is not None:
        features["input_field"] = input_field
    return {"model_version": MODEL_VERSION, "features": features, "nodes": nodes}


def infer(model: dict, text: str) -> str:
    """Walk the EXPORTED model over the frozen features — the verification
    path re-derives everything from the wire dict, never from sklearn."""
    counts = featurize(text, model["features"]["buckets"])
    nodes = model["nodes"]
    i = 0
    for _ in range(len(nodes)):  # cycle guard, mirrors the rust walker
        node = nodes[i]
        if "leaf" in node:
            return node["leaf"]["label"]
        split = node["split"]
        i = split["left"] if counts[split["feature"]] <= split["threshold"] else split["right"]
    raise AssertionError("tree walk exceeded the node count (cycle in exported tree)")


def train_and_eval(
    texts: list[str],
    labels: list[str],
    *,
    buckets: int,
    holdout: float,
    seed: int,
    max_depth: int,
    input_field: str | None,
    weights: list[int] | None = None,
) -> tuple[dict, dict]:
    """Split, fit DecisionTreeClassifier(random_state=seed, max_depth=...),
    export to the frozen shape, then verify the EXPORT on both splits.
    Returns (model, metrics); applies no gate — the caller decides.

    weights (ADR-0031): per-row witness counts. None, or all-ones, IS the
    weightless protocol — same fit call, same metrics line, byte-for-byte.
    Any weight > 1 engages sklearn's sample_weight (the split criterion and
    every leaf argmax count witness mass instead of rows) and the metrics
    line gains two honestly-labeled fields: weighted_train_accuracy (witness
    mass reproduced / total train mass — the objective the fit saw) and
    train_weight (that total). train_accuracy stays the PLAIN fraction of
    training rows reproduced, and holdout_accuracy stays PLAIN UNWEIGHTED —
    measured reality, not the training trick. With conflicting labels for
    one input (a divergent group's rows) 100% plain accuracy is impossible
    by construction: the shortfall is the recorded disagreement, reported,
    never smoothed."""
    import sklearn  # lazy: --self-test must run stdlib-only
    from sklearn.tree import DecisionTreeClassifier

    if weights is not None and len(weights) != len(texts):
        raise TrainerError(f"{len(weights)} weights for {len(texts)} observations")
    weighted = weights is not None and any(w != 1 for w in weights)
    train_idx, holdout_idx = split_indices(len(texts), holdout, seed)
    if not train_idx:
        raise TrainerError(
            f"no training examples left ({len(texts)} observations, holdout {holdout}); "
            "reduce --holdout or add data"
        )
    x_train = [featurize(texts[i], buckets) for i in train_idx]
    y_train = [labels[i] for i in train_idx]
    clf = DecisionTreeClassifier(random_state=seed, max_depth=max_depth)
    if weighted:
        assert weights is not None
        clf.fit(x_train, y_train, sample_weight=[float(weights[i]) for i in train_idx])
    else:
        clf.fit(x_train, y_train)
    model = export_tree(clf, buckets=buckets, input_field=input_field)

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
        "classes": [str(c) for c in clf.classes_],
        "trainer": f"tree_train.py sklearn-{sklearn.__version__} seed={seed}",
    }
    if weighted:
        assert weights is not None
        total = sum(weights[i] for i in train_idx)
        good = sum(weights[i] for i in train_idx if infer(model, texts[i]) == labels[i])
        metrics["weighted_train_accuracy"] = good / total
        metrics["train_weight"] = total
    return model, metrics


def dumps_canonical(model: dict) -> str:
    """Canonical model json: sorted keys, compact separators — byte-stable
    across runs and the same key order as auto-model's to_json."""
    return json.dumps(model, sort_keys=True, separators=(",", ":"))


def dumps_metrics(metrics: dict) -> str:
    """One metrics line, keys in protocol order."""
    return json.dumps(metrics, separators=(",", ":"))


def self_test() -> None:
    """Pinned fnv1a vectors (identical to auto-model's tests) + featurize
    sanity + weight-parse pins (ADR-0031). Needs no files and no sklearn."""
    assert fnv1a_32(b"") == 2166136261
    assert fnv1a_32(b"a") == 0xE40C292C
    assert fnv1a_32(b"abc") == 0x1A47E90B
    counts = featurize("AbC", 1024)  # lowercases to "abc": exactly one trigram
    assert sum(counts) == 1.0
    assert counts[0x1A47E90B % 1024] == 1.0
    assert sum(featurize("ab", 8)) == 0.0  # too short for a trigram
    assert sum(featurize("AbAb", 8)) == 2.0  # "abab" -> "aba", "bab"
    # weight parsing: absent = 1, present = the count, both on one file
    _, _, w = parse_weighted_observations(
        '{"input":"aa","output":"x"}\n{"input":"aa","output":"y","weight":3}\n', None
    )
    assert w == [1, 3]
    for bad in ['"weight":0', '"weight":-1', '"weight":1.5', '"weight":true', '"weight":"2"']:
        try:
            parse_weighted_observations('{"input":"aa","output":"x",%s}\n' % bad, None)
        except TrainerError:
            pass
        else:
            raise AssertionError(f"{bad} must be rejected")
    # the weight-blind entrypoint refuses weighted files instead of
    # silently dropping witness counts
    try:
        parse_observations('{"input":"aa","output":"x","weight":2}\n', None)
    except TrainerError:
        pass
    else:
        raise AssertionError("parse_observations must refuse weights != 1")
    assert parse_observations('{"input":"aa","output":"x","weight":1}\n', None) == (
        ["aa"],
        ["x"],
    )


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="train a decision tree over frozen char-trigram fnv1a features; "
        "emit model json readable by crates/auto-model"
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
        "--seed", type=int, default=0, help="split shuffle + sklearn random_state"
    )
    parser.add_argument(
        "--min-holdout-accuracy",
        type=float,
        default=1.0,
        help="gate: exit 3 when holdout accuracy is below this",
    )
    parser.add_argument("--max-depth", type=int, default=12, help="tree depth cap")
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="assert pinned fnv1a vectors + featurize sanity, print ok, exit 0",
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
    if args.max_depth < 1:
        parser.error("--max-depth must be >= 1")
    try:
        with open(args.observations, encoding="utf-8-sig") as f:
            raw = f.read()
    except OSError as e:
        print(f"error: cannot read {args.observations}: {e}", file=sys.stderr)
        return 2
    try:
        texts, labels, weights = parse_weighted_observations(raw, args.input_field)
        model, metrics = train_and_eval(
            texts,
            labels,
            buckets=args.buckets,
            holdout=args.holdout,
            seed=args.seed,
            max_depth=args.max_depth,
            input_field=args.input_field,
            weights=weights,
        )
    except TrainerError as e:
        print(f"error: {e}", file=sys.stderr)
        return 2
    # model.json is written pass or fail — the gate decides acceptance
    with open(args.out, "w", encoding="utf-8", newline="\n") as f:
        f.write(dumps_canonical(model) + "\n")
    print(dumps_metrics(metrics))
    passed = metrics["train_n"] > 0 and metrics["holdout_accuracy"] >= args.min_holdout_accuracy
    return 0 if passed else 3


if __name__ == "__main__":
    sys.exit(main())
