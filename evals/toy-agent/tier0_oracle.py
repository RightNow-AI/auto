"""Pluggable tier-0 for the toy task — the reference process `auto run` deopts
to when an artifact's guard trips.

Honest note: tier-0 here is a local reference process, not a frontier model.
The constitution binds tier-0 to a frontier model as the interpreter for
novelty; that binding requires API access under the frontier-spend cap,
neither of which exists yet (spec/adr/open-questions.md, "tiering (S6)").
The runtime sees tier-0 only through the command contract in spec/runtime.md,
so this process and a real frontier binding are interchangeable to it.

The command contract: exactly one argument — the canonical input JSON, e.g.
{"prompt":"..."} — the output value as JSON on stdout, exit 0. Anything
malformed: a plain-words usage error on stderr, exit 2.

The rule below is `fake_model` from evals/toy-agent/agent.py, ported exactly:
this oracle answers precisely what the recorded reference answered, so a
deopt observation it produces is legitimate synthesis evidence.
"""

import json
import sys


def fake_model(prompt: str) -> str:
    """Deterministic 'model': keyword extraction. Secretly symbolic."""
    words = [w.strip(".,") for w in prompt.lower().split()]
    keywords = sorted({w for w in words if len(w) > 4})[:3]
    return " ".join(keywords)


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: tier0_oracle.py '<canonical input json>'", file=sys.stderr)
        return 2
    try:
        value = json.loads(sys.argv[1])
    except ValueError as e:
        print(f"tier0_oracle: input is not JSON: {e}", file=sys.stderr)
        return 2
    prompt = value.get("prompt") if isinstance(value, dict) else None
    if not isinstance(prompt, str):
        print(
            'tier0_oracle: input must be an object with a string "prompt" field',
            file=sys.stderr,
        )
        return 2
    print(json.dumps(fake_model(prompt)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
