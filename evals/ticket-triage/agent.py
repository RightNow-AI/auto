"""The economics-demo reference agent (E1): a REAL frontier call per run.

Routes one support ticket into billing | bug | feature by asking OpenAI
(gpt-5.4-mini) — the behavior Auto records and compiles. Every run makes
one paid API call and records one model_call span whose reserved attrs
(spec/trace.md §3) carry the call's OWN measured usage:

    tokens          = usage.prompt_tokens + usage.completion_tokens
    cost_usd_micros = ceil-integer cost from the pinned per-MTok prices

Prices are mirrored from crates/auto-frontier/src/prices.rs (gpt-5.4-mini
$0.75 / $4.50 per MTok, source and retrieval date cited there); the mirror
must track that table. The key comes from $OPENAI_API_KEY or the repo
.env (gitignored). No key -> exit 2 (a paid reference agent without a key
refuses; it never fakes an answer).

    auto record --store db -- python evals/ticket-triage/agent.py "<ticket>"

The agent normalizes the model's reply (strip whitespace/period,
lowercase) — that normalization is part of the recorded behavior. Whatever
the model answers IS the reference; the contract's one_of property judges
it at verify time, honestly.
"""

from __future__ import annotations

import json
import os
import sys
import urllib.error
import urllib.request

sys.path.insert(
    0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "sdk", "python")
)

from auto_sdk import Tracer  # noqa: E402

MODEL = "gpt-5.4-mini"
ENDPOINT = "https://api.openai.com/v1/chat/completions"
# mirror of crates/auto-frontier/src/prices.rs (µ$/MTok) — keep in sync
INPUT_UMICROS_PER_MTOK = 750_000
OUTPUT_UMICROS_PER_MTOK = 4_500_000

SYSTEM = (
    "You are a support ticket router. Classify the user's ticket. "
    "Reply with exactly one word: billing, bug, or feature."
)


def ceil_div(numerator: int, denominator: int) -> int:
    return -(-numerator // denominator)


def api_key() -> str:
    key = os.environ.get("OPENAI_API_KEY", "").strip()
    if key:
        return key
    env_path = os.path.join(
        os.path.dirname(os.path.abspath(__file__)), "..", "..", ".env"
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


def triage(ticket: str, key: str) -> tuple[str, dict]:
    """One real chat-completions call. Returns (label, reserved attrs)."""
    body = json.dumps(
        {
            "model": MODEL,
            "messages": [
                {"role": "system", "content": SYSTEM},
                {"role": "user", "content": ticket},
            ],
            # reasoning models bill thinking inside completion tokens; leave
            # headroom so the one-word answer is never truncated away
            "max_completion_tokens": 400,
        }
    ).encode("utf-8")
    request = urllib.request.Request(
        ENDPOINT,
        data=body,
        headers={
            "content-type": "application/json",
            "authorization": f"Bearer {key}",
        },
        method="POST",
    )
    with urllib.request.urlopen(request, timeout=120) as response:
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
        print("usage: agent.py <ticket text>", file=sys.stderr)
        raise SystemExit(2)
    ticket = sys.argv[1]
    key = api_key()

    # the paid call runs INSIDE the traced closure so the span's duration_ms
    # is the real API latency; the attrs dict is filled by the closure and
    # the tracer serializes attrs after the closure returns, so the measured
    # usage rides the same span as its measured duration
    attrs: dict = {}

    def call() -> str:
        try:
            label, usage_attrs = triage(ticket, key)
        except urllib.error.HTTPError as e:
            detail = e.read().decode("utf-8", errors="replace")[:300]
            raise RuntimeError(f"api {e.code}: {detail}") from None
        attrs.update(usage_attrs)
        return label

    with Tracer(task="ticket-triage") as t:
        with t.span("run"):
            label = t.model_call("triage", {"ticket": ticket}, call, attrs=attrs)
    print(f"label={label}")


if __name__ == "__main__":
    main()
