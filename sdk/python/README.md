# auto-sdk (python)

Records agent runs as v0 JSONL traces and replays them with recorded outputs
substituted for live calls. Wire format and semantics: `spec/trace.md`.

```python
from auto_sdk import Tracer

with Tracer(task="triage") as t:          # path= or AUTO_TRACE_FILE env
    with t.span("agent-loop"):
        body = t.tool_call("http.get", {"url": url}, lambda: fetch(url))
        draft = t.model_call("frontier", {"prompt": p}, lambda: llm(p))
    t.branch("router", {"len": len(body)}, "long")
    key = t.env_read("API_BASE")          # digest + length recorded, never the value

# later: replay with the world substituted from the recording
with Tracer(task="triage", replay="run.jsonl") as t:
    ...  # same code path; tool/model calls return recorded outputs;
         # any divergence raises ReplayDivergence
```

Honesty properties: exceptions are recorded then re-raised (never swallowed);
`env_read` stores a sha-256 digest + length, never the value; NaN/Infinity
raise instead of corrupting the trace; replay never silently tolerates a
different call, input, environment, or decision.

Concurrency: record mode is thread-safe — span parenting is thread-local
(one thread's nesting never sees another's; new threads start unparented),
seq/span_id allocation and file writes share one lock. Replay is defined for
sequential runs only: the matcher consumes one recorded order, so concurrent
effectful calls during replay raise `ReplayDivergence`.

Not here yet: automatic instrumentation of model/tool frameworks, remote
export, concurrent replay.

Tests: `python -m pytest tests` (pytest only; no network).
