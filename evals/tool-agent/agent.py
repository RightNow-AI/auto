"""Capability-region reference agent (ADR-0017): a chain whose middle span
is a REAL TOOL CALL — the capability boundary.

    extract: {"doc": s} -> normalized text     (model_call, secretly symbolic)
    lookup:  text -> owning team               (tool_call: first word's team)
    format:  team -> uppercased                (model_call, secretly symbolic)

Region-compiled, the tool span becomes a declared capability: the artifact
imports auto.tool_call, the gate replays the recorded pairs hermetically,
and `auto run --tool lookup=...` provides the live implementation.
"""

from __future__ import annotations

import os
import sys

sys.path.insert(
    0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "sdk", "python")
)

from auto_sdk import Tracer  # noqa: E402


def extract(doc: str) -> str:
    return " ".join(doc.lower().split())


def lookup(text: str) -> str:
    return f"team-{text.split()[0][0]}"


def format_team(team: str) -> str:
    return team.upper()


def main() -> None:
    doc = sys.argv[1] if len(sys.argv) > 1 else "Beta Alpha Gamma"
    with Tracer(task="tool-agent") as t:
        with t.span("run"):
            text = t.model_call("extract", {"doc": doc}, lambda: extract(doc))
            team = t.tool_call("lookup", text, lambda: lookup(text))
            final = t.model_call("format", team, lambda: format_team(team))
    print(f"final={final}")


if __name__ == "__main__":
    main()
