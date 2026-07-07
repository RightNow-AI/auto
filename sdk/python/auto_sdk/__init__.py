"""Auto trace SDK for python agents (S1).

Real functionality: recording agent runs to v0 JSONL traces and replaying
them with recorded outputs substituted for live calls. See spec/trace.md for
the wire format and semantics.

    from auto_sdk import Tracer

    with Tracer(task="triage") as t:
        body = t.tool_call("http.get", {"url": url}, lambda: fetch(url))
        answer = t.model_call("frontier", {"prompt": p}, lambda: llm(p))

Not yet here (later spine items): automatic instrumentation of model/tool
frameworks, typescript parity, remote export. What is here works and is
tested; nothing else is pretended.
"""

from .tracer import (
    FORMAT_VERSION,
    ReplayDivergence,
    ReplayedError,
    SDK_NAME,
    Tracer,
    __version__,
    canonical_json,
    digest_hex,
)

__all__ = [
    "FORMAT_VERSION",
    "ReplayDivergence",
    "ReplayedError",
    "SDK_NAME",
    "Tracer",
    "__version__",
    "canonical_json",
    "digest_hex",
]
