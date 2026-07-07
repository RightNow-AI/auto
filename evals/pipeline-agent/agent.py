"""Region-compilation reference agent (ADR-0015): one run = a CHAIN of three
model calls whose glue is identity — the smallest honest multi-span region.

    extract: {"doc": s} -> normalized text (lowercase, single-spaced)
    route:   text       -> its first word
    format:  label      -> the label uppercased

Each stage is deliberately secretly-symbolic (expressible in the v0 DSL), so
`auto compile` with a region contract can synthesize every stage, witness the
glue as identity, and emit ONE pipeline artifact whose differential gate
replays every recorded end-to-end chain.

    auto record --store db -- python evals/pipeline-agent/agent.py "<doc>"
"""

from __future__ import annotations

import os
import sys

sys.path.insert(
    0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "sdk", "python")
)

from auto_sdk import Tracer  # noqa: E402

DEFAULT_DOC = "Beta Alpha Gamma"


def extract(doc: str) -> str:
    return " ".join(doc.lower().split())


def route(text: str) -> str:
    return text.split()[0]


def format_label(label: str) -> str:
    return label.upper()


def main() -> None:
    doc = sys.argv[1] if len(sys.argv) > 1 else DEFAULT_DOC
    with Tracer(task="pipeline-agent") as t:
        with t.span("run"):
            text = t.model_call("extract", {"doc": doc}, lambda: extract(doc))
            label = t.model_call("route", text, lambda: route(text))
            final = t.model_call("format", label, lambda: format_label(label))
    print(f"final={final}")


if __name__ == "__main__":
    main()
