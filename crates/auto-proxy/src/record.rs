//! Pure request→trace transformation — the recordable core of the proxy.
//!
//! An [`Exchange`] is one completed request/response pair the socket loop
//! captured. [`exchange_to_trace`] turns it into a synthetic single-span
//! `model_call` trace, mirroring the CLI's deopt-observation precedent
//! (`ingest_deopt_observation`): rng-free trace id, one span, the reserved
//! cost/token attrs (spec/trace.md §3) computed from the response usage and
//! the pinned price table.
//!
//! I/O choice (ADR-0012): the span **input** is the request body verbatim
//! (the recorded prompt payload — messages, params, everything); the span
//! **output** is the assistant text at `choices[0].message.content`. The raw
//! response envelope is provider-specific and is NOT stored — the compiler
//! consumes the semantic I/O, not the wire body.
//!
//! Pure and socket-free: every unit test drives this without HTTP.

use std::collections::BTreeMap;

use serde_json::Value;

use auto_trace::model::{
    Span, SpanId, SpanKind, Trace, TraceHeader, TraceId, canonical_json, digest_hex,
};

/// One completed request/response pair captured by the socket loop.
#[derive(Debug, Clone)]
pub struct Exchange {
    /// the JSON body the caller POSTed (the recorded prompt payload)
    pub request_body: Value,
    /// the JSON body the upstream returned (2xx exchanges only — the caller
    /// records nothing else in v0)
    pub response_body: Value,
    /// wall-clock duration of the upstream call
    pub duration_ms: u64,
    /// unix epoch ms when the upstream call began
    pub started_at_ms: u64,
}

/// Turn a captured [`Exchange`] into a single-span `model_call` trace.
///
/// The span `name` is the request's `model` field; a request that declares no
/// `model` is the one `Err` case — nothing was recordable, so the caller
/// records nothing. `error` is always `None` (the caller only hands us 2xx
/// exchanges).
///
/// Attrs (spec/trace.md §3): `tokens` = `usage.prompt_tokens +
/// usage.completion_tokens`; `cost_usd_micros` from the pinned price table
/// keyed on the request model. A model absent from the table records `tokens`
/// only (never a guessed price); a response with no usage records neither attr
/// (budget checks then honestly read Inconclusive).
pub fn exchange_to_trace(task: &str, exchange: &Exchange, pid: u32) -> Result<Trace, String> {
    let model = exchange
        .request_body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            "request body has no string \"model\" field; nothing to record".to_owned()
        })?;

    // output = the assistant text; a null/absent content (e.g. a tool-call-only
    // reply) records a null output — the wire conflates None and JSON null.
    let output = match exchange.response_body.pointer("/choices/0/message/content") {
        Some(content) if !content.is_null() => Some(content.clone()),
        _ => None,
    };

    let attrs = usage_attrs(model, &exchange.response_body);
    let trace_id = derive_trace_id(task, exchange, pid, model)?;

    let header = TraceHeader {
        trace_id: TraceId(trace_id),
        task: task.to_owned(),
        started_at_ms: exchange.started_at_ms,
        sdk: format!("auto-proxy/{}", env!("CARGO_PKG_VERSION")),
        attrs: BTreeMap::new(),
        // proxied exchanges carry span-level I/O only (ADR-0025)
        task_input: None,
        task_output: None,
    };
    let span = Span {
        span_id: SpanId(1),
        parent_span_id: None,
        seq: 1,
        kind: SpanKind::ModelCall,
        name: model.to_owned(),
        input: exchange.request_body.clone(),
        output,
        error: None,
        started_at_ms: exchange.started_at_ms,
        duration_ms: exchange.duration_ms,
        attrs,
    };
    Ok(Trace {
        header,
        spans: vec![span],
    })
}

/// The reserved cost/token attrs, computed from the response usage. Empty when
/// usage is absent; `tokens` only (no cost) when the model is not pinned.
fn usage_attrs(model: &str, response_body: &Value) -> BTreeMap<String, String> {
    let mut attrs = BTreeMap::new();
    let prompt = response_body
        .pointer("/usage/prompt_tokens")
        .and_then(Value::as_u64);
    let completion = response_body
        .pointer("/usage/completion_tokens")
        .and_then(Value::as_u64);
    let (Some(prompt), Some(completion)) = (prompt, completion) else {
        return attrs; // missing usage → neither attr
    };
    attrs.insert(
        "tokens".to_owned(),
        prompt.saturating_add(completion).to_string(),
    );
    // cost only for a pinned model — a guessed price is worse than no price
    if let Ok(price) = auto_frontier::prices::price_of(model) {
        let cost = price
            .input_cost(prompt)
            .saturating_add(price.output_cost(completion));
        attrs.insert("cost_usd_micros".to_owned(), cost.to_string());
    }
    attrs
}

/// A per-exchange trace id from pid + time + task + request digest, mirroring
/// `ingest_deopt_observation`'s rng-free derivation (digest of a seed → first
/// 32 hex chars → u128). Distinct prompts get distinct ids; an accidental
/// collision fails ingest loudly (a duplicate trace is an error, never an
/// upsert) and the exchange is still relayed.
fn derive_trace_id(task: &str, exchange: &Exchange, pid: u32, model: &str) -> Result<u128, String> {
    let seed = format!(
        "auto-proxy-{pid}-{task}-{model}-{}-{}-{}",
        exchange.started_at_ms,
        exchange.duration_ms,
        digest_hex(&canonical_json(&exchange.request_body)),
    );
    let hex = digest_hex(&seed);
    u128::from_str_radix(&hex[..32], 16).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use auto_trace::Store;

    // Fixtures copied from the OpenAI chat completions API reference
    // (developers.openai.com/api/docs/api-reference/chat, retrieved 2026-07-04);
    // the response usage matches auto-frontier openai.rs's fixture so the ceil
    // cost math is cross-checkable against that crate.
    fn request(model: &str) -> Value {
        serde_json::json!({
            "model": model,
            "messages": [
                { "role": "system", "content": "You are a helpful assistant." },
                { "role": "user", "content": "Hello!" }
            ]
        })
    }

    fn response_with_usage() -> Value {
        serde_json::json!({
            "id": "chatcmpl-abc123",
            "object": "chat.completion",
            "created": 1_700_000_000,
            "model": "gpt-5.4-mini-2026-05",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "Hello there!" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 1000, "completion_tokens": 200, "total_tokens": 1200 }
        })
    }

    fn exchange(request_body: Value, response_body: Value) -> Exchange {
        Exchange {
            request_body,
            response_body,
            duration_ms: 40,
            started_at_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn pinned_model_records_both_reserved_attrs_with_ceil_math() {
        let ex = exchange(request("gpt-5.4-mini"), response_with_usage());
        let trace = exchange_to_trace("chat", &ex, 4242).expect("model present");

        assert_eq!(trace.spans.len(), 1, "one synthetic span per exchange");
        let span = &trace.spans[0];
        assert_eq!(span.kind, SpanKind::ModelCall);
        // the span name is the REQUEST model, not the response snapshot id
        assert_eq!(span.name, "gpt-5.4-mini");
        // input is the request body verbatim
        assert_eq!(span.input, request("gpt-5.4-mini"));
        // output is the assistant text
        assert_eq!(span.output, Some(Value::String("Hello there!".into())));
        assert_eq!(span.error, None);
        // tokens = 1000 + 200
        assert_eq!(span.attrs.get("tokens").map(String::as_str), Some("1200"));
        // cost = ceil(1000 * 750_000 / 1e6)=750  +  ceil(200 * 4_500_000 / 1e6)=900
        assert_eq!(
            span.attrs.get("cost_usd_micros").map(String::as_str),
            Some("1650")
        );
        assert_eq!(trace.header.task, "chat");
        assert!(trace.header.sdk.starts_with("auto-proxy/"));
    }

    #[test]
    fn unknown_model_records_tokens_only_never_a_guessed_price() {
        // gpt-4o is not in the pinned table (spec/adr/0010) → no cost attr
        let ex = exchange(request("gpt-4o"), response_with_usage());
        let trace = exchange_to_trace("chat", &ex, 1).expect("model present");
        let attrs = &trace.spans[0].attrs;
        assert_eq!(attrs.get("tokens").map(String::as_str), Some("1200"));
        assert!(
            !attrs.contains_key("cost_usd_micros"),
            "an unpinned model must never be priced by guess"
        );
    }

    #[test]
    fn missing_usage_records_neither_attr_but_keeps_the_span() {
        let response = serde_json::json!({
            "id": "chatcmpl-x",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "hi" },
                "finish_reason": "stop"
            }]
        });
        let ex = exchange(request("gpt-5.4-mini"), response);
        let trace = exchange_to_trace("chat", &ex, 1).expect("model present");
        let span = &trace.spans[0];
        assert!(span.attrs.is_empty(), "no usage → no tokens, no cost");
        assert_eq!(span.output, Some(Value::String("hi".into())));
    }

    #[test]
    fn missing_model_records_nothing() {
        let ex = exchange(serde_json::json!({ "messages": [] }), response_with_usage());
        assert!(
            exchange_to_trace("chat", &ex, 1).is_err(),
            "no model field → Err, nothing recorded"
        );
    }

    #[test]
    fn tool_call_reply_with_null_content_records_a_null_output() {
        // a 2xx whose assistant message is tool-calls only carries content: null
        let response = serde_json::json!({
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": null, "tool_calls": [] },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 5, "completion_tokens": 5, "total_tokens": 10 }
        });
        let ex = exchange(request("gpt-5.4-mini"), response);
        let trace = exchange_to_trace("chat", &ex, 1).expect("model present");
        assert_eq!(
            trace.spans[0].output, None,
            "null content records a null output (a documented v0 gap: tool calls are lost)"
        );
        // usage attrs are still recorded
        assert_eq!(
            trace.spans[0].attrs.get("tokens").map(String::as_str),
            Some("10")
        );
    }

    #[test]
    fn trace_round_trips_through_the_store_with_its_attrs() {
        let ex = exchange(request("gpt-5.4-mini"), response_with_usage());
        let trace = exchange_to_trace("chat", &ex, 7).expect("model present");
        let id = trace.header.trace_id;

        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = Store::open(&dir.path().join("proxy.db")).expect("open store");
        store.ingest(&trace).expect("ingest");

        let loaded = store.load_trace(id).expect("load back");
        assert_eq!(
            loaded, trace,
            "the store round-trips the recorded exchange exactly"
        );
        let span = &loaded.spans[0];
        assert_eq!(span.attrs.get("tokens").map(String::as_str), Some("1200"));
        assert_eq!(
            span.attrs.get("cost_usd_micros").map(String::as_str),
            Some("1650")
        );
    }

    #[test]
    fn distinct_prompts_get_distinct_trace_ids() {
        let a = exchange_to_trace(
            "chat",
            &exchange(request("gpt-5.4-mini"), response_with_usage()),
            1,
        )
        .unwrap();
        let b_req = serde_json::json!({ "model": "gpt-5.4-mini", "messages": [{ "role": "user", "content": "different" }] });
        let b = exchange_to_trace("chat", &exchange(b_req, response_with_usage()), 1).unwrap();
        assert_ne!(a.header.trace_id, b.header.trace_id);
    }
}
