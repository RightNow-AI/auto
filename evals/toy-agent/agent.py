"""Toy reference task for S1: a small agent with mixed determinism.

Four of its five effectful calls are deterministic (including the "model"
call, which is secretly a keyword extractor — the thesis in miniature). One
call reads the wall clock and is nondeterministic across runs on purpose:
the determinism report must show a divergent signature, proving the number
is measured, not rigged.

No network. Run it twice under `auto record`, then `auto report`:

    auto record --store store.db -- python evals/toy-agent/agent.py
    auto record --store store.db -- python evals/toy-agent/agent.py
    auto report --task toy-agent --store store.db
"""

import os
import sys
import time

sys.path.insert(
    0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "sdk", "python")
)

from auto_sdk import Tracer  # noqa: E402


def fake_model(prompt: str) -> str:
    """Deterministic 'model': keyword extraction. Secretly symbolic."""
    words = [w.strip(".,") for w in prompt.lower().split()]
    keywords = sorted({w for w in words if len(w) > 4})[:3]
    return " ".join(keywords)


def main() -> None:
    # an alternative document may be passed as argv[1]: recording runs over
    # DISTINCT inputs is what gives synthesis (S4) evidence to generalize from
    doc = sys.argv[1] if len(sys.argv) > 1 else (
        "The quick brown fox jumps over the lazy dog near the riverbank."
    )
    # task-level I/O (ADR-0025): the whole-run input is the document; the
    # whole-run output is the summary dict this run already prints
    with Tracer(task="toy-agent", task_input={"doc": doc}) as t:
        with t.span("run"):
            n = t.tool_call("wordcount", {"text": doc}, lambda: len(doc.split()))
            summary = t.model_call("fake-frontier", {"prompt": doc}, lambda: fake_model(doc))
            route = t.branch("length-router", {"n": n}, "long" if n > 5 else "short")
            t.memory_op("append", "summaries", lambda: None, value=summary)
            # deliberately nondeterministic: the report must call this out
            t.tool_call("clock.now_ms", {}, lambda: time.time_ns() // 1_000_000)
        t.set_task_output({"words": n, "summary": summary, "route": route})
    print(f"words={n} summary={summary!r} route={route}")


if __name__ == "__main__":
    main()
