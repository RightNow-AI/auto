# @auto/sdk (typescript)

Records agent runs as v0 JSONL traces and replays them with recorded outputs
substituted for live calls — the same contract and wire format as the python
SDK. Spec: `spec/trace.md`.

```ts
import { Tracer } from "@auto/sdk"; // runs directly under node >=22.6 (type stripping)

const t = new Tracer({ task: "triage" }); // path or AUTO_TRACE_FILE env
await t.span("agent-loop", async () => {
  const body = await t.toolCall("http.get", { url }, () => fetch(url).then(r => r.text()));
  const draft = await t.modelCall("frontier", { prompt }, () => llm(prompt));
});
t.branch("router", { len: 3 }, "long");
t.envRead("API_BASE"); // digest + length recorded, never the value
t.close();

// replay: same code path, world substituted from the recording
const r = new Tracer({ task: "triage", replay: "run.jsonl" });
```

Honesty properties: exceptions are recorded then re-thrown; `envRead` stores
a sha-256 digest + length, never the value; NaN/Infinity/undefined throw
instead of corrupting the trace; replay never silently tolerates a different
call, input, environment, or decision.

Concurrency: record mode supports overlapping concurrent spans — parenting
rides AsyncLocalStorage, so `Promise.all` over traced `span()` calls parents
each leaf to its own wrapper; sync callers are unchanged. Replay is defined
for sequential runs only: the matcher consumes one recorded order, so
concurrent effectful calls during replay throw `ReplayDivergence`.

v0 limitations (documented, not hidden): no concurrent replay; tests are
runtime tests (no separate type-check step yet).

Tests: `npm test` (node:test, no dependencies, no network).
