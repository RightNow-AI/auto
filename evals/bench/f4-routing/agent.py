"""AUTO-BENCH F4 — policy-routing reference agent: a REAL frontier call per run.

Routes one purchase-approval request into approve | review | reject by asking
OpenAI (gpt-5.4-mini) — the behavior Auto records, distills, and compiles.
This is the F4 family of evals/bench (DESIGN.md): a decision over rules +
thresholds. The routing rule is inexpressible in the 17-op extraction DSL (no
arithmetic, no threshold comparison, no branching — spec/synthesis.md §2), so
`auto compile` refuses honestly and DISTILLATION (tree/mlp; spec/distillation.md)
is the compile path that works. Classes are DELIBERATELY imbalanced
(~24 approve / ~10 review / ~6 reject; see corpus.jsonl + README.md), which is
where weighted witnesses (ADR-0031) earn their keep.

Every run makes one paid API call and records one model_call span ("route")
whose reserved attrs (spec/trace.md §3) carry the call's OWN measured usage:

    tokens          = usage.prompt_tokens + usage.completion_tokens
    cost_usd_micros = ceil-integer cost from the pinned per-MTok prices

Prices are mirrored from crates/auto-frontier/src/prices.rs (gpt-5.4-mini
$0.75 / $4.50 per MTok, source and retrieval date cited there); the mirror
must track that table. The key comes from $OPENAI_API_KEY or the repo .env
(gitignored). No key -> exit 2 (a paid reference agent without a key refuses;
it never fakes an answer).

    auto record --store db -- python evals/bench/f4-routing/agent.py "<request>"

The agent normalizes the model's reply (strip whitespace/period, lowercase)
and VALIDATES membership in {approve, review, reject}: an off-policy or
malformed reply raises inside the traced closure, so the span records an
honest error (never a fabricated label). Whatever valid word the model
answers IS the reference; the contract's one_of property and the differential
gate judge it at verify/emit time, honestly.
"""

from __future__ import annotations

import json
import os
import sys
import urllib.error
import urllib.request

sys.path.insert(
    0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "..", "sdk", "python")
)

from auto_sdk import Tracer  # noqa: E402

MODEL = "gpt-5.4-mini"
ENDPOINT = "https://api.openai.com/v1/chat/completions"
# mirror of crates/auto-frontier/src/prices.rs (µ$/MTok) — keep in sync
INPUT_UMICROS_PER_MTOK = 750_000
OUTPUT_UMICROS_PER_MTOK = 4_500_000

LABELS = ("approve", "review", "reject")

# The policy is stated VERBATIM in the system prompt. The model is expected to
# follow it with high-but-not-perfect determinism; whatever it actually answers
# is the recorded reference (the corpus is designed to avoid the one genuine
# overlap — amount < $500 AND a blocked vendor — so every request has an
# unambiguous intended label; see README.md).
SYSTEM = (
    "You are a purchase-approval router. Apply this policy exactly:\n"
    "- APPROVE if the amount is under $500, OR if the amount is under $5000 "
    "and the vendor is a preferred vendor.\n"
    "- REJECT if the amount is $20000 or more, OR if the vendor is a blocked "
    "vendor.\n"
    "- Otherwise REVIEW.\n"
    "Reply with exactly one word: approve, review, or reject."
)


def ceil_div(numerator: int, denominator: int) -> int:
    return -(-numerator // denominator)


def api_key() -> str:
    key = os.environ.get("OPENAI_API_KEY", "").strip()
    if key:
        return key
    env_path = os.path.join(
        os.path.dirname(os.path.abspath(__file__)), "..", "..", "..", ".env"
    )
    try:
        with open(env_path, encoding="utf-8") as f:
            for line in f:
                line = line.strip()
                if line.startswith("OPENAI_API_KEY="):
                    key = line.split("=", 1)[1].strip()
                    if key:
                        return key
    except OSError:
        pass
    print("error: no OPENAI_API_KEY in the environment or repo .env", file=sys.stderr)
    raise SystemExit(2)


def route(request: str, key: str) -> tuple[str, dict]:
    """One real chat-completions call. Returns (normalized label, reserved
    attrs). The reply is lowercased/stripped here; membership validation
    happens in the traced closure AFTER the usage attrs are attached, so even
    a malformed (rejected) reply records the cost of the paid call that made
    it."""
    body = json.dumps(
        {
            "model": MODEL,
            "messages": [
                {"role": "system", "content": SYSTEM},
                {"role": "user", "content": request},
            ],
            # reasoning models bill thinking inside completion tokens; leave
            # headroom so the one-word answer is never truncated away
            "max_completion_tokens": 400,
        }
    ).encode("utf-8")
    http_request = urllib.request.Request(
        ENDPOINT,
        data=body,
        headers={
            "content-type": "application/json",
            "authorization": f"Bearer {key}",
        },
        method="POST",
    )
    with urllib.request.urlopen(http_request, timeout=120) as response:
        payload = json.load(response)
    content = payload["choices"][0]["message"]["content"]
    label = content.strip().strip(".").lower()
    usage = payload.get("usage", {})
    prompt_tokens = int(usage.get("prompt_tokens", 0))
    completion_tokens = int(usage.get("completion_tokens", 0))
    cost = ceil_div(prompt_tokens * INPUT_UMICROS_PER_MTOK, 1_000_000) + ceil_div(
        completion_tokens * OUTPUT_UMICROS_PER_MTOK, 1_000_000
    )
    attrs = {
        "cost_usd_micros": str(cost),
        "tokens": str(prompt_tokens + completion_tokens),
    }
    return label, attrs


def main() -> None:
    if len(sys.argv) < 2:
        print("usage: agent.py <purchase request text>", file=sys.stderr)
        raise SystemExit(2)
    request = sys.argv[1]
    key = api_key()

    # the paid call runs INSIDE the traced closure so the span's duration_ms
    # is the real API latency; the attrs dict is filled by the closure and
    # the tracer serializes attrs after the closure returns, so the measured
    # usage rides the same span as its measured duration
    attrs: dict = {}

    def call() -> str:
        try:
            label, usage_attrs = route(request, key)
        except urllib.error.HTTPError as e:
            detail = e.read().decode("utf-8", errors="replace")[:300]
            raise RuntimeError(f"api {e.code}: {detail}") from None
        # attach the measured usage FIRST so the span carries the paid call's
        # cost/tokens even if the reply then fails validation below
        attrs.update(usage_attrs)
        if label not in LABELS:
            raise RuntimeError(f"model reply {label!r} is not one of {LABELS}")
        return label

    with Tracer(task="bench-f4") as t:
        with t.span("run"):
            label = t.model_call("route", {"request": request}, call, attrs=attrs)
    print(f"label={label}")


if __name__ == "__main__":
    main()
