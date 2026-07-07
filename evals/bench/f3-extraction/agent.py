"""AUTO-BENCH F3 reference agent: typed-field extraction from one customer
support email — the "most agents are secretly parsers" family, head-on
(evals/bench/DESIGN.md, family F3).

ONE real gpt-5.4-mini chat call per run, INSIDE the traced closure, so the
span's duration_ms is the real API latency and the reserved attrs
(spec/trace.md §3) carry the call's OWN measured usage:

    tokens          = usage.prompt_tokens + usage.completion_tokens
    cost_usd_micros = ceil-integer cost from the pinned per-MTok prices

Prices are mirrored from crates/auto-frontier/src/prices.rs (gpt-5.4-mini
$0.75 / $4.50 per MTok); the mirror must track that table. The key comes
from $OPENAI_API_KEY or the repo .env (gitignored). No key -> exit 2 (a
paid reference agent without a key refuses loudly; it never fakes an
answer).

The system prompt pins the EXACT output shape: a minified JSON object
{"order_id":"ORD-#####","category":...,"urgency":...} and nothing else.
The agent then parses and validates that the reply IS such JSON:
json.loads on the raw content (no code-fence stripping, no substring
rescue, no silent repair), keys exactly {order_id, category, urgency},
order_id matching ORD-[0-9]{5}, category/urgency in their closed sets. An
invalid reply raises inside the traced closure, so the span records the
error honestly (and the run exits nonzero) — the usage attrs are updated
BEFORE validation, so an errored span still carries the money it burned.

Output recorded on the span = the parsed object (the typed fields, not the
reply string). Span: kind model_call, name "extract", input {"email": ...}.
Task label: bench-f3.

    auto record --store <store> -- python evals/bench/f3-extraction/agent.py "<email>"
"""

from __future__ import annotations

import json
import os
import re
import sys
import urllib.error
import urllib.request

sys.path.insert(
    0,
    os.path.join(
        os.path.dirname(os.path.abspath(__file__)), "..", "..", "..", "sdk", "python"
    ),
)

from auto_sdk import Tracer  # noqa: E402

MODEL = "gpt-5.4-mini"
ENDPOINT = "https://api.openai.com/v1/chat/completions"
# mirror of crates/auto-frontier/src/prices.rs (µ$/MTok) — keep in sync
INPUT_UMICROS_PER_MTOK = 750_000
OUTPUT_UMICROS_PER_MTOK = 4_500_000

CATEGORIES = ("refund", "replacement", "status", "cancel", "other")
URGENCIES = ("low", "normal", "high")
ORDER_ID_RE = re.compile(r"ORD-[0-9]{5}")

SYSTEM = (
    "You extract order fields from one customer support email. "
    "Reply with ONLY a minified JSON object - no code fences, no prose, no "
    'trailing text - exactly of the form {"order_id":"ORD-#####",'
    '"category":"...","urgency":"..."}. '
    "order_id: the order id copied verbatim from the email (ORD- followed by "
    "five digits). "
    "category: exactly one of refund (asks for money back), replacement (asks "
    "for a new item, part, or exchange), status (asks where the order is or "
    "when it arrives), cancel (asks to cancel the order), other (any other "
    "request). "
    "urgency: high only when the email uses explicit rush wording such as "
    "urgent, immediately, asap, or right away; low only when it explicitly "
    "defers such as no rush, no hurry, or whenever you get a chance; "
    "otherwise normal."
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


def complete(email: str, key: str) -> tuple[str, dict]:
    """One real chat-completions call. Returns (raw reply text, reserved attrs)."""
    body = json.dumps(
        {
            "model": MODEL,
            "messages": [
                {"role": "system", "content": SYSTEM},
                {"role": "user", "content": email},
            ],
            # reasoning models bill thinking inside completion tokens; leave
            # headroom so the small JSON answer is never truncated away
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
    if not isinstance(content, str):
        raise ValueError("model reply has no text content")
    return content, attrs


def parse_extraction(reply: str) -> dict:
    """The model reply must BE the pinned JSON object — parsed, never
    repaired. Anything else raises; the tracer records the error and the
    run exits nonzero (an invalid reply is data, not something to fix)."""
    try:
        value = json.loads(reply)
    except ValueError as e:
        raise ValueError(f"model reply is not JSON ({e}): {reply[:160]!r}") from None
    if not isinstance(value, dict):
        raise ValueError(f"model reply is JSON but not an object: {reply[:160]!r}")
    keys = set(value)
    if keys != {"order_id", "category", "urgency"}:
        raise ValueError(f"model reply has wrong keys: {sorted(keys)!r}")
    order_id = value["order_id"]
    if not isinstance(order_id, str) or not ORDER_ID_RE.fullmatch(order_id):
        raise ValueError(f"order_id {order_id!r} does not match ORD-#####")
    if value["category"] not in CATEGORIES:
        raise ValueError(f"category {value['category']!r} not one of {CATEGORIES}")
    if value["urgency"] not in URGENCIES:
        raise ValueError(f"urgency {value['urgency']!r} not one of {URGENCIES}")
    return value


def main() -> None:
    if len(sys.argv) < 2:
        print("usage: agent.py <email text>", file=sys.stderr)
        raise SystemExit(2)
    email = sys.argv[1]
    key = api_key()

    # the paid call runs INSIDE the traced closure so the span's duration_ms
    # is the real API latency; attrs is filled by the closure BEFORE the
    # reply is validated, so even a span that errors on an invalid reply
    # still records what the call billed
    attrs: dict = {}

    def call() -> dict:
        try:
            reply, usage_attrs = complete(email, key)
        except urllib.error.HTTPError as e:
            detail = e.read().decode("utf-8", errors="replace")[:300]
            raise RuntimeError(f"api {e.code}: {detail}") from None
        attrs.update(usage_attrs)
        return parse_extraction(reply)

    with Tracer(task="bench-f3") as t:
        with t.span("run"):
            fields = t.model_call("extract", {"email": email}, call, attrs=attrs)
    print(json.dumps(fields, sort_keys=True, separators=(",", ":")))


if __name__ == "__main__":
    main()
