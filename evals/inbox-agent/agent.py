"""The multi-step economics-demo agent (E1 at agent scale): THREE real
frontier calls plus one local tool per run, glued by ordinary python.

    classify:  ticket -> billing | bug | feature        (LLM, paid)
    priority:  ticket -> low | normal | urgent          (LLM, paid)
    lookup:    category -> owning team                  (local tool, free)
    summarize: ticket -> one line, <= 12 words          (LLM, paid)

Every model_call span records the call's own measured usage via the
reserved attrs and its real wall-clock duration (the paid call runs inside
the traced closure; wave-3 lesson). Modes, selected by AGENT_MODE:

  frontier  (default) — real chat-completions calls; OPENAI_BASE overrides
             the API base so the SAME code records through `auto proxy`.
  compiled  — each model call POSTs to a local `auto serve` instead:
             AUTO_SERVE_BASE + /run/ + $ID_CLASSIFY / $ID_PRIORITY /
             $ID_SUMMARIZE. Zero glue changes: the agent is the constant,
             the interpreter under it is what compiles away.

    auto record --store db -- python evals/inbox-agent/agent.py "<ticket>"
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
# mirror of crates/auto-frontier/src/prices.rs (µ$/MTok) — keep in sync
INPUT_UMICROS_PER_MTOK = 750_000
OUTPUT_UMICROS_PER_MTOK = 4_500_000

TEAMS = {"billing": "payments-team", "bug": "platform-oncall", "feature": "product-intake"}

PROMPTS = {
    "classify": "You are a support ticket router. Reply with exactly one word: billing, bug, or feature.",
    "priority": "Rate the ticket's urgency. Reply with exactly one word: low, normal, or urgent.",
    "summarize": "Summarize the ticket in at most 12 words. Reply with the summary only.",
}


def ceil_div(numerator: int, denominator: int) -> int:
    return -(-numerator // denominator)


def api_key() -> str:
    key = os.environ.get("OPENAI_API_KEY", "").strip()
    if key:
        return key
    env_path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", ".env")
    try:
        with open(env_path, encoding="utf-8") as f:
            for line in f:
                if line.strip().startswith("OPENAI_API_KEY="):
                    key = line.strip().split("=", 1)[1].strip()
                    if key:
                        return key
    except OSError:
        pass
    print("error: no OPENAI_API_KEY in the environment or repo .env", file=sys.stderr)
    raise SystemExit(2)


def frontier_call(step: str, ticket: str, key: str, base: str) -> tuple[str, dict]:
    """One real chat call for `step`; returns (normalized text, reserved attrs)."""
    body = json.dumps(
        {
            "model": MODEL,
            "messages": [
                {"role": "system", "content": PROMPTS[step]},
                {"role": "user", "content": ticket},
            ],
            "max_completion_tokens": 400,
        }
    ).encode("utf-8")
    request = urllib.request.Request(
        f"{base}/v1/chat/completions",
        data=body,
        headers={"content-type": "application/json", "authorization": f"Bearer {key}"},
        method="POST",
    )
    with urllib.request.urlopen(request, timeout=120) as response:
        payload = json.load(response)
    text = payload["choices"][0]["message"]["content"].strip().strip(".")
    if step != "summarize":
        text = text.lower()
    usage = payload.get("usage", {})
    prompt_tokens = int(usage.get("prompt_tokens", 0))
    completion_tokens = int(usage.get("completion_tokens", 0))
    cost = ceil_div(prompt_tokens * INPUT_UMICROS_PER_MTOK, 1_000_000) + ceil_div(
        completion_tokens * OUTPUT_UMICROS_PER_MTOK, 1_000_000
    )
    return text, {
        "cost_usd_micros": str(cost),
        "tokens": str(prompt_tokens + completion_tokens),
    }


def compiled_call(step: str, ticket: str) -> str:
    """One artifact call: POST the span's input to `auto serve`."""
    base = os.environ["AUTO_SERVE_BASE"]
    artifact_id = os.environ[f"ID_{step.upper()}"]
    body = json.dumps({"ticket": ticket}).encode("utf-8")
    request = urllib.request.Request(
        f"{base}/run/{artifact_id}", data=body, method="POST"
    )
    with urllib.request.urlopen(request, timeout=30) as response:
        payload = json.load(response)
    return payload["output"]


def main() -> None:
    if len(sys.argv) < 2:
        print("usage: agent.py <ticket text>", file=sys.stderr)
        raise SystemExit(2)
    ticket = sys.argv[1]
    mode = os.environ.get("AGENT_MODE", "frontier")
    base = os.environ.get("OPENAI_BASE", "https://api.openai.com")
    key = api_key() if mode == "frontier" else ""

    def model_step(t: Tracer, step: str) -> str:
        if mode == "compiled":
            return t.model_call(step, {"ticket": ticket}, lambda: compiled_call(step, ticket))
        attrs: dict = {}

        def call() -> str:
            try:
                text, usage_attrs = frontier_call(step, ticket, key, base)
            except urllib.error.HTTPError as e:
                detail = e.read().decode("utf-8", errors="replace")[:300]
                raise RuntimeError(f"api {e.code}: {detail}") from None
            attrs.update(usage_attrs)
            return text

        return t.model_call(step, {"ticket": ticket}, call, attrs=attrs)

    with Tracer(task="inbox-agent") as t:
        with t.span("run"):
            category = model_step(t, "classify")
            priority = model_step(t, "priority")
            team = t.tool_call(
                "lookup", category, lambda: TEAMS.get(category, "triage-inbox")
            )
            summary = model_step(t, "summarize")
    report = {"category": category, "priority": priority, "team": team, "summary": summary}
    print(json.dumps(report, sort_keys=True))


if __name__ == "__main__":
    main()
