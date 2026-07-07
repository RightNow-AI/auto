# ADR-0012: the recording proxy — zero-code-change trace capture via an OpenAI-compatible endpoint

status: accepted · scope: `crates/auto-proxy` (new modules `record`, `server`), `spec/trace.md` (Recording proxy section)

## context

Recording an agent today means instrumenting it: the trace SDK (S1,
`sdk/python`) wraps model/tool calls to emit spans. That is the right long-run
frontend, but it requires touching the agent's code, and most agents worth
compiling are OpenAI-SDK callers we do not own. The recording proxy is the
zero-code-change lever: point the agent's `base_url` at the proxy, and every
`/v1/chat/completions` call is recorded on its way to the real upstream. It
records the same trace shape the SDK does, so determinism analysis, contracts,
and compilation consume proxy-captured traces with no special case.

## decision

1. **Pure core, thin socket shell.** `record::exchange_to_trace` (request +
   response JSON → one `model_call` trace) and `server::preflight` (routing
   decision) are pure functions, unit-tested without a socket. `server::proxy`
   is a blocking `tiny_http` loop that only does I/O: read, forward via `ureq`,
   relay, ingest. No `cargo test` opens a socket; loopback proof is an e2e
   script (the workspace rule).

2. **I/O choice: request body in, assistant text out.** The span `input` is the
   request body verbatim — messages, params, everything — because that IS the
   recorded prompt payload the compiler must see. The span `output` is the
   assistant text at `choices[0].message.content`. The raw response envelope
   (ids, `system_fingerprint`, `object`, timestamps) is provider-specific noise
   the IR does not consume, so it is **not** stored. A tool-call-only reply
   (`content: null`) records a null output — a documented v0 gap (tool calls
   are not yet captured).

3. **Reserved attrs from the pinned table.** `tokens` =
   `usage.prompt_tokens + usage.completion_tokens`; `cost_usd_micros` from
   `auto_frontier::prices::price_of` keyed on the **request** model, computed
   with the same ceil integer math as the frontier client (ADR-0010). A model
   absent from the pinned table records `tokens` only — never a guessed price.
   A response with no usage records neither attr, so budget checks honestly
   read Inconclusive rather than seeing a fabricated zero.

4. **Relay-first, ingest-second.** The upstream bytes (status + body) are
   relayed to the caller verbatim *before* the store is touched. An ingest
   failure is `eprintln`-loud but never propagates: the agent's call must not
   fail or slow down because recording hiccuped. Recording is best-effort; the
   proxy is otherwise transparent. `ureq` is built with
   `http_status_as_error(false)` so upstream 4xx/5xx bodies are relayed, not
   collapsed into a transport error.

5. **No credential storage.** The proxy holds no key. It forwards the caller's
   `Authorization` header verbatim and refuses (401) a request that carries
   none — nothing is ever sent unauthenticated, and no secret is written to the
   store. Only the body and `Authorization` are forwarded; other request
   headers are dropped in v0.

6. **Streaming refused in v0.** A `"stream": true` body is rejected with 400
   `{"error":"streaming is not recordable in v0"}` *before* forwarding: an SSE
   stream cannot be parsed into one recordable exchange, and forwarding it
   would relay a response we cannot record — a silent gap. Refusing loudly is
   the honest choice. Preflight order is route (404) → streaming (400) → auth
   (401) → forward.

7. **rng-free trace id.** Derived like `ingest_deopt_observation`: digest of a
   seed (pid + task + model + start + duration + request-body digest) → first
   32 hex chars → u128. Distinct prompts get distinct ids; an accidental
   collision fails ingest loudly (a trace id is immutable, never upserted) and
   the exchange is still relayed.

## alternatives considered

**SDK-side automatic attrs.** Have the trace SDK compute cost/tokens itself.
That is the right home for instrumented agents, but it requires the code change
the proxy exists to avoid. The two are complementary, not competing.

**mitmproxy-style TLS interception.** Record HTTPS to arbitrary hosts with no
`base_url` change by terminating the caller's TLS. Rejected: it MITMs the
caller's transport, needs a trusted CA in the agent, and adds a large security
surface for a v0 whose users can set one env var.

**Store the raw request and response bodies.** Faithful to the wire, but the
bodies are provider-specific envelopes; the IR consumes semantic I/O, and
storing full envelopes bloats the trace store with fields no pass reads. The
request body is kept (it is the prompt); the response envelope is reduced to
its assistant text.

**Thread-per-request with a `Mutex<Store>`.** Concurrency for agents that fan
out parallel calls. Deferred: the store ingest is `&mut`, sequential recording
is correct, and a single recv loop is the simpler honest v0. Concurrency is an
open question (spec/adr/open-questions.md) if measured to matter.

## consequences

- Any OpenAI-SDK agent is recordable by setting `base_url` — no code change,
  no key held by us, the same trace shape the SDK emits.
- Proxy-captured `model_call` spans carry honest cost/token attrs for pinned
  models and honest gaps (tokens-only, or neither) otherwise, so the economics
  demo and budget checks read real numbers or Inconclusive, never fiction.
- v0 gaps are explicit: no streaming, single-span traces (no tool-call/branch
  structure), response envelope reduced to assistant text, sequential serving.
- `tiny_http` joins the workspace as the blocking server (shared with
  `auto-serve`); no async runtime, matching the synchronous-workspace rationale
  in ADR-0010/0011.

## sources

- OpenAI chat completions request/response shape (`model`, `messages`;
  `choices[0].message.content`; `usage.prompt_tokens`/`completion_tokens`;
  `Authorization: Bearer`):
  <https://developers.openai.com/api/docs/api-reference/chat>
- OpenAI pricing (pinned table, ADR-0010):
  <https://developers.openai.com/api/docs/pricing>
- tiny_http 0.12 API (`Server::http`, `Request::{method,url,headers,as_reader,respond}`,
  `Response::{from_string,from_data,with_status_code,with_header}`,
  `Header::from_bytes`, `StatusCode(u16)`):
  <https://docs.rs/tiny_http/0.12.0/tiny_http/>
- ureq 3.3 (`Agent::post`, `RequestBuilder::{header,send}` with `AsSendBody`
  for `&[u8]`, `Body::read_to_vec`, `Response::status().as_u16()`,
  `http_status_as_error`): <https://docs.rs/ureq/3.3.0/ureq/>
