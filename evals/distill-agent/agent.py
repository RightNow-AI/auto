"""Reference task for S5: an agent whose one model call is genuinely fuzzy.

The "fuzzy-router" labels a document by a rule that is deliberately NOT
expressible in the extraction DSL (no contains, no branching — spec/synthesis
§2), so plain `auto compile` must refuse honestly and distillation is the
compile path that works:

    "urgent"  if any of outage | breach | deadline is a substring of the
              lowercased doc, else
    "long"    if the doc has more than 60 characters, else
    "short"

The doc comes from argv[1] (required): one recorded run per corpus line gives
the trainer one observation per document.

    while IFS= read -r doc; do
      auto record --store store.db -- python evals/distill-agent/agent.py "$doc"
    done < evals/distill-agent/corpus.txt
"""

import os
import sys

sys.path.insert(
    0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "sdk", "python")
)

from auto_sdk import Tracer  # noqa: E402

MARKERS = ("outage", "breach", "deadline")


def fuzzy_route(doc: str) -> str:
    """Substring tests plus a length threshold: trivial in python, honestly
    out of reach for the closed straight-line DSL."""
    lowered = doc.lower()
    if any(marker in lowered for marker in MARKERS):
        return "urgent"
    if len(doc) > 60:
        return "long"
    return "short"


def main() -> None:
    if len(sys.argv) < 2:
        print("usage: agent.py <document>", file=sys.stderr)
        raise SystemExit(2)
    doc = sys.argv[1]
    with Tracer(task="distill-agent") as t:
        with t.span("run"):
            label = t.model_call("fuzzy-router", {"text": doc}, lambda: fuzzy_route(doc))
    print(f"label={label}")


if __name__ == "__main__":
    main()
