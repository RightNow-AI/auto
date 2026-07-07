"""AUTO-BENCH F4 corpus gate — structural validity + declared class skew.

Frozen artifact. Run with no network; it never calls a model. It parses the
FROZEN corpus (id + request only), derives each request's INTENDED label from
the policy below, and asserts the distribution the README declares:

    approve = 24   review = 10   reject = 6   (total 40)

The intended label is used ONLY to check that the authored corpus has the
skew we designed. The RECORDED labels come from the model at record time and
are what the contract/gate actually judge — this script does not assume them.

The policy (rejects take precedence; a blocked vendor or a >= $20000 spend is
never auto-approved):

    reject  if vendor tier == blocked OR amount >= 20000
    approve if amount < 500 OR (amount < 5000 AND tier == preferred)
    review  otherwise

Design invariant (asserted): NO request falls in the only genuine overlap of
the literal policy text (amount < 500 AND tier == blocked). Because the corpus
avoids that region, the intended label is unambiguous under either reading of
the policy's clause ordering.

    python evals/bench/f4-routing/validate_corpus.py
    # exit 0 + printed skew on success; nonzero + reason on any violation.
"""

from __future__ import annotations

import json
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
CORPUS = os.path.join(HERE, "corpus.jsonl")

TIERS = ("preferred", "standard", "new", "blocked")
EXPECTED = {"approve": 24, "review": 10, "reject": 6}
AMOUNT_RE = re.compile(r"\$\s*([0-9][0-9,]*)")


def parse_amount(request: str) -> int:
    matches = AMOUNT_RE.findall(request)
    if len(matches) != 1:
        raise ValueError(f"expected exactly one $amount, found {matches!r}")
    return int(matches[0].replace(",", ""))


def parse_tier(request: str) -> str:
    present = [t for t in TIERS if re.search(rf"\b{t}\b", request)]
    if len(present) != 1:
        raise ValueError(f"expected exactly one vendor tier, found {present!r}")
    return present[0]


def intended(amount: int, tier: str) -> str:
    if tier == "blocked" or amount >= 20000:
        return "reject"
    if amount < 500 or (amount < 5000 and tier == "preferred"):
        return "approve"
    return "review"


def main() -> int:
    with open(CORPUS, encoding="utf-8") as f:
        raw = [ln.rstrip("\n") for ln in f if ln.strip()]

    counts = {"approve": 0, "review": 0, "reject": 0}
    seen_ids: set[str] = set()
    rows = []
    for i, line in enumerate(raw, start=1):
        try:
            obj = json.loads(line)
        except json.JSONDecodeError as e:
            print(f"FAIL line {i}: invalid JSON: {e}", file=sys.stderr)
            return 1
        if set(obj) != {"id", "request"}:
            print(f"FAIL line {i}: keys must be exactly id+request, got {sorted(obj)}", file=sys.stderr)
            return 1
        rid, request = obj["id"], obj["request"]
        expected_id = f"f4-{i:03d}"
        if rid != expected_id:
            print(f"FAIL line {i}: id {rid!r} != expected {expected_id!r}", file=sys.stderr)
            return 1
        if rid in seen_ids:
            print(f"FAIL: duplicate id {rid!r}", file=sys.stderr)
            return 1
        seen_ids.add(rid)
        if not isinstance(request, str) or not request.strip():
            print(f"FAIL {rid}: request must be a non-empty string", file=sys.stderr)
            return 1
        try:
            amount = parse_amount(request)
            tier = parse_tier(request)
        except ValueError as e:
            print(f"FAIL {rid}: {e} :: {request!r}", file=sys.stderr)
            return 1
        # design invariant: the only literal-policy overlap must be empty
        if amount < 500 and tier == "blocked":
            print(f"FAIL {rid}: forbidden overlap (amount<500 AND blocked)", file=sys.stderr)
            return 1
        label = intended(amount, tier)
        counts[label] += 1
        rows.append((rid, amount, tier, label))

    if len(raw) != 40:
        print(f"FAIL: expected 40 requests, found {len(raw)}", file=sys.stderr)
        return 1

    print("F4 corpus:", len(raw), "requests; intended class skew (rejects-precedence policy):")
    for label in ("approve", "review", "reject"):
        flag = "ok" if counts[label] == EXPECTED[label] else "MISMATCH"
        print(f"  {label:<7} {counts[label]:>3}  (expected {EXPECTED[label]}) [{flag}]")

    if counts != EXPECTED:
        print(f"FAIL: skew {counts} != declared {EXPECTED}", file=sys.stderr)
        return 1
    print("skew OK: 24 approve / 10 review / 6 reject - matches README + DESIGN.md F4 (imbalanced).")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
