# ADR-0010: the spend-capped frontier client — fail-closed cap, append-only ledger, blocking HTTP

status: accepted · scope: `crates/auto-frontier` (new), `crates/auto-passes` (`extraction_llm`), `crates/auto-runtime` (`tier0`), `spec/synthesis.md` §7, `spec/runtime.md` §3

## context

The constitution names two components that need a paid frontier model —
LLM-guided CEGIS (ADR-0005's recorded upgrade) and the tier-0 deopt target
(ADR-0007) — and imposes a guardrail: hard spend cap per session, logged,
no paid runs beyond the cap. Under limited resources we set a **$25 per session** cap on paid OpenAI usage, which flipped the first provider from the Anthropic-first plan mid-wave (the
`Frontier` trait seam was frozen before providers, so the flip cost one
module, not a redesign).

## decision

1. **One crate is the only paid path.** `auto-frontier` owns the trait
   (`Frontier`), the cap, the ledger, the price table, and the providers.
   CEGIS and tier-0 take `&mut dyn Frontier` and cannot spend on their own.
2. **Fail-closed cap.** `CappedFrontier` refuses — before anything is sent —
   any request whose conservative worst case would cross the session cap:
   input estimated at one token per two prompt bytes (~2x margin over the
   ~4-chars/token English rule of thumb), plus full `max_output_tokens` at
   output price. A cap of 0 (the default everywhere a cap is read) refuses
   everything; authorization is an explicit nonzero flag.
3. **Append-only ledger, append-before-return.** `~/.auto/spend.jsonl`
   (env-overridable), one line per paid call: timestamp, session, model,
   purpose, usage, cost, request digest. A ledger append failure withholds
   the response; a corrupt ledger is fatal on read. An unrecorded or
   uncounted paid call is the state the guardrail forbids, so both fail loud.
4. **Pinned integer price table, ceil rounding, refuse-unknown.** µ$/MTok as
   exact integers; cost = ceiling division (never rounds down); a model not
   in the table is refused, never priced by guess. Costs are computed from
   the usage the provider reports, never estimated from text length.
5. **Blocking HTTP (ureq 3.3.0, rustls), no tokio.** The CLI is synchronous;
   one blocking POST per call needs no async runtime. Non-2xx handled via
   `http_status_as_error(false)` so error bodies are read and relayed.
6. **OpenAI chat-completions first** (`gpt-5.4-mini` default; nano/full/5.5
   pinned). Reasoning tokens bill inside `completion_tokens` and are bounded
   by `max_completion_tokens`, so usage-based cost and the worst-case
   pre-check both stay sound. The Anthropic provider is recorded, not built:
   its price entries are pinned (from the wave-2 agent's retrieval,
   re-verify before the first Anthropic call) but no key exists
   to live-fire it, and an untested client would be a stub wearing a trench
   coat.

## alternatives considered

**tokio + reqwest.** The ecosystem default; buys concurrent calls and
connection pooling. Rejected for v0: it drags an async runtime into a
synchronous CLI for a client that makes a handful of sequential calls per
compile. The trait seam hides the transport; swap later if concurrency is
measured to matter.

**Provider SDK crates.** Official/community SDKs exist but pin their own
transport stacks and change fast; the two calls used here (one POST, one
JSON body) do not justify the dependency surface. The wire shapes are
verified against the provider docs instead and unit-tested against fixture
bodies.

**Post-hoc cap accounting (record after, refuse when over).** Simpler, but
a burst of parallel or scripted calls could overshoot the cap arbitrarily
before the ledger catches up. The conservative pre-check bounds overshoot
at zero: a request that could cross is never sent.

**In-memory spend counters.** Fast, but a crash (or three parallel CLIs)
zeroes or forks the count. The ledger file is the single source of truth
and is re-read per call; per-call file I/O is noise next to a network call.

## consequences

- Every paid call in the workspace is capped, ledgered, and attributable to
  a session and purpose; `paper/` experiments can cite the ledger.
- The cap check refuses early and conservatively; legitimate calls near the
  cap boundary are sometimes refused (over-estimate) — accepted, that is
  the fail-closed direction.
- Two providers in the price table, one implemented; the trait seam and the
  scripted fake keep CEGIS/tier-0 tests provider-independent.
- ureq/rustls join the dependency tree (host-side only; sandboxes and
  artifacts remain network-free — nothing here is reachable from wasm).

## sources

- OpenAI pricing (gpt-5.4-nano $0.20/$1.25, gpt-5.4-mini $0.75/$4.50,
  gpt-5.4 $2.50/$15, gpt-5.5 $5/$30 per MTok):
  <https://developers.openai.com/api/docs/pricing>
- OpenAI chat completions request/response shape (`max_completion_tokens`;
  `choices[0].message.content`; `usage.prompt_tokens`/`completion_tokens`;
  `Authorization: Bearer`): <https://developers.openai.com/api/docs/api-reference/chat>
- ureq 3.3.0: `send_json`/`read_json`, rustls default,
  `http_status_as_error` config: <https://docs.rs/ureq/latest/ureq/>
- Anthropic pricing (haiku-4-5 $1/$5, sonnet $3/$15, opus-4-8 $5/$25;
  entries pinned, provider not built):
  <https://platform.claude.com/docs/en/about-claude/pricing>
