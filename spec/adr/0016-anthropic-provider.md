# ADR-0016: the Anthropic provider — built as a mirror, still live-unfired

status: accepted · scope: `crates/auto-frontier` (`anthropic`)

## context

ADR-0010 built OpenAI first because the owner's key is OpenAI. The Anthropic
price entries were pinned in the `v1` table (haiku-4-5 $1/$5, sonnet $3/$15,
opus-4-8 $5/$25 per MTok) but the provider was recorded as honestly unbuilt:
no key existed to live-fire it, and an untested client would be a stub
wearing a trench coat. The `Frontier` trait seam was frozen before providers,
so adding the second provider is one module, not a redesign.

## decision

1. **`AnthropicFrontier` mirrors `openai.rs` structurally.** Pure `body()` /
   `parse()` halves unit-tested without sockets against fixtures from the API
   reference; blocking ureq 3 with `http_status_as_error(false)` and a 120s
   timeout; non-2xx relayed as `Api` with status + ≤400-char body tail; cost
   computed from the pinned table (`prices::price_of`, ceiling division) using
   provider-reported usage; `Debug` redacts the key; a missing/blank key is a
   `MissingKey` refusal naming `ANTHROPIC_API_KEY`; an unpinned model is
   refused at construction. It does not enforce the cap — `CappedFrontier`
   wraps it (ADR-0010).
2. **Wire shape** (verified against the Messages API reference, source
   below): POST `/v1/messages`; headers `x-api-key`, `anthropic-version:
   2023-06-01`, `content-type: application/json`; body `{model, max_tokens,
   system, messages: [{role: "user", content}]}` with
   `request.max_output_tokens` mapped to `max_tokens`; answer read from the
   first `content` block of type `"text"`; usage from `usage.input_tokens` /
   `usage.output_tokens`. Thinking tokens bill inside `output_tokens` and are
   bounded by `max_tokens`, so the cap pre-check stays sound.
3. **Still not live-fired.** No Anthropic key exists in this environment;
   the tests exercise only the pure halves. Before the first live call,
   re-verify the pinned prices (ADR-0010 §6's note stands), and record that
   first call in `paper/log.md` when it happens.

## sources

- Anthropic Messages API request/response shape and required headers:
  <https://platform.claude.com/docs/en/api/messages> (retrieved)
- Anthropic pricing (entries pinned in ADR-0010 / `prices.rs` v1):
  <https://platform.claude.com/docs/en/about-claude/pricing> (retrieved
 )
