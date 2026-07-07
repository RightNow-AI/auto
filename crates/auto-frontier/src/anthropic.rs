//! Anthropic Messages API provider.
//!
//! Blocking HTTP via ureq 3 (rustls). Request/response shapes verified
//! 2026-07-04 against <https://platform.claude.com/docs/en/api/messages>:
//! POST /v1/messages with `model`, `max_tokens`, `system`, `messages`
//! (one `user` role message); answer at the first `content` block of type
//! `"text"`; usage at `usage.input_tokens` / `usage.output_tokens`; auth
//! headers `x-api-key` + `anthropic-version: 2023-06-01` (+ the
//! `content-type: application/json` that `send_json` sets).
//!
//! Cost is computed HERE from the pinned price table (`prices.rs`) using the
//! usage the provider reported — never estimated from text length. The model
//! id must be pinned; construction refuses otherwise. Note for thinking
//! models (sonnet/opus): thinking tokens are billed inside `output_tokens`
//! and bounded by `max_tokens`, so usage-based costing counts them
//! automatically and the capped client's worst-case pre-check stays sound.
//!
//! Built to mirror `openai.rs`; **never live-fired** — no Anthropic key
//! exists in this environment, so only the pure body/parse halves are
//! tested, against fixtures from the API reference. Re-verify the pinned
//! prices before the first live call (ADR-0010 §6, ADR-0016).
//!
//! This provider does NOT enforce the spend cap — wrap it in
//! [`crate::CappedFrontier`] (the only construction the CLI exposes;
//! ADR-0010).

use std::time::Duration;

use serde_json::{Value, json};

use crate::prices::price_of;
use crate::{Frontier, FrontierError, FrontierRequest, FrontierResponse};

/// Environment variable holding the API key.
pub const ANTHROPIC_KEY_ENV: &str = "ANTHROPIC_API_KEY";

const ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
/// Required API version header value (verified 2026-07-04).
const ANTHROPIC_VERSION: &str = "2023-06-01";
const TIMEOUT: Duration = Duration::from_secs(120);

/// A blocking Anthropic Messages API client for one pinned model.
pub struct AnthropicFrontier {
    model: String,
    key: String,
}

/// Manual impl: the API key must never appear in debug output or error text.
impl std::fmt::Debug for AnthropicFrontier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicFrontier")
            .field("model", &self.model)
            .field("key", &"<redacted>")
            .finish()
    }
}

impl AnthropicFrontier {
    /// A client for `model` (must be in the pinned price table). `key` falls
    /// back to `$ANTHROPIC_API_KEY`; absent-or-empty is a [`FrontierError::MissingKey`]
    /// refusal — nothing is ever sent unauthenticated.
    pub fn new(model: &str, key: Option<String>) -> Result<Self, FrontierError> {
        price_of(model)?;
        let key = match key.filter(|k| !k.trim().is_empty()).or_else(|| {
            std::env::var(ANTHROPIC_KEY_ENV)
                .ok()
                .filter(|k| !k.trim().is_empty())
        }) {
            Some(k) => k.trim().to_owned(),
            None => {
                return Err(FrontierError::MissingKey {
                    env_var: ANTHROPIC_KEY_ENV.to_owned(),
                });
            }
        };
        Ok(Self {
            model: model.to_owned(),
            key,
        })
    }

    /// The wire body for one request — a pure function, unit-tested without
    /// a socket.
    fn body(&self, request: &FrontierRequest) -> Value {
        json!({
            "model": self.model,
            "max_tokens": request.max_output_tokens,
            "system": request.system,
            "messages": [
                { "role": "user", "content": request.user },
            ],
        })
    }

    /// Parse a Messages API response body into a [`FrontierResponse`],
    /// computing cost from the pinned table — a pure function, unit-tested
    /// against a fixture copied from the API reference.
    fn parse(&self, body: &Value) -> Result<FrontierResponse, FrontierError> {
        let text = body
            .get("content")
            .and_then(Value::as_array)
            .and_then(|blocks| {
                blocks
                    .iter()
                    .find(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            })
            .and_then(|b| b.get("text"))
            .and_then(Value::as_str)
            .ok_or_else(|| FrontierError::Api {
                detail: format!(
                    "response has no content block of type \"text\"; body began {:?}",
                    snippet(&body.to_string())
                ),
            })?
            .to_owned();
        let usage = |field: &str| {
            body.pointer(&format!("/usage/{field}"))
                .and_then(Value::as_u64)
                .ok_or_else(|| FrontierError::Api {
                    detail: format!("response usage.{field} is missing or not a u64"),
                })
        };
        let input_tokens = usage("input_tokens")?;
        let output_tokens = usage("output_tokens")?;
        let price = price_of(&self.model)?;
        let cost_usd_micros = price
            .input_cost(input_tokens)
            .saturating_add(price.output_cost(output_tokens));
        // record the snapshot id the provider says served the call; fall back
        // to the requested id
        let model = body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(&self.model)
            .to_owned();
        Ok(FrontierResponse {
            text,
            input_tokens,
            output_tokens,
            cost_usd_micros,
            model,
        })
    }
}

impl Frontier for AnthropicFrontier {
    fn complete(&mut self, request: &FrontierRequest) -> Result<FrontierResponse, FrontierError> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(TIMEOUT))
            .build()
            .new_agent();

        let mut response = agent
            .post(ENDPOINT)
            .header("x-api-key", &self.key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .send_json(self.body(request))
            .map_err(|e| FrontierError::Http {
                detail: e.to_string(),
            })?;

        let status = response.status();
        if !status.is_success() {
            let tail = response
                .body_mut()
                .read_to_string()
                .unwrap_or_else(|e| format!("<unreadable body: {e}>"));
            return Err(FrontierError::Api {
                detail: format!("http {status}: {}", snippet(&tail)),
            });
        }
        let body: Value = response
            .body_mut()
            .read_json()
            .map_err(|e| FrontierError::Api {
                detail: format!("response body is not JSON: {e}"),
            })?;
        self.parse(&body)
    }

    fn model_id(&self) -> &str {
        &self.model
    }
}

/// Char-boundary-safe prefix for error details (bodies can be huge).
fn snippet(text: &str) -> String {
    const MAX: usize = 400;
    if text.chars().count() > MAX {
        let head: String = text.chars().take(MAX).collect();
        format!("{head}…")
    } else {
        text.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A client with an explicit key — env-independent, and NEVER used to
    /// open a socket in tests (only the pure body/parse halves are tested).
    fn client() -> AnthropicFrontier {
        AnthropicFrontier::new("claude-haiku-4-5", Some("test-key-never-sent".into()))
            .expect("pinned model + explicit key")
    }

    #[test]
    fn unpinned_model_refused_at_construction() {
        let err = AnthropicFrontier::new("claude-mystery", Some("k".into()))
            .expect_err("unpinned model has no price");
        assert!(matches!(err, FrontierError::UnknownModel { .. }));
    }

    #[test]
    fn explicit_empty_key_without_env_is_missing_key() {
        // an explicitly empty key must not silently authenticate; the env
        // fallback may legitimately supply one on operator machines, so only
        // assert the refusal shape when the env is ALSO empty
        if std::env::var(ANTHROPIC_KEY_ENV)
            .map(|v| v.trim().is_empty())
            .unwrap_or(true)
        {
            let err = AnthropicFrontier::new("claude-haiku-4-5", Some("   ".into()))
                .expect_err("blank key refused");
            assert!(
                matches!(err, FrontierError::MissingKey { env_var } if env_var == ANTHROPIC_KEY_ENV)
            );
        }
    }

    #[test]
    fn request_body_matches_the_documented_wire_shape() {
        let body = client().body(&FrontierRequest {
            system: "be the reference".into(),
            user: "{\"prompt\":\"x\"}".into(),
            max_output_tokens: 128,
        });
        assert_eq!(body["model"], "claude-haiku-4-5");
        assert_eq!(body["max_tokens"], 128);
        assert_eq!(body["system"], "be the reference");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "{\"prompt\":\"x\"}");
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        // exactly the documented fields — nothing undocumented rides along
        assert_eq!(body.as_object().unwrap().len(), 4);
    }

    #[test]
    fn response_parsing_reads_text_usage_and_computes_pinned_cost() {
        // shape per the Messages API reference (platform.claude.com, 2026-07-04)
        let body = serde_json::json!({
            "id": "msg_013Zva2CMHLNnXjNJJKqJ2EF",
            "type": "message",
            "role": "assistant",
            "model": "claude-haiku-4-5-20251001",
            "content": [{ "type": "text", "text": "\"urgent\"" }],
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": { "input_tokens": 1000, "output_tokens": 200 }
        });
        let parsed = client().parse(&body).expect("fixture parses");
        assert_eq!(parsed.text, "\"urgent\"");
        assert_eq!(parsed.input_tokens, 1000);
        assert_eq!(parsed.output_tokens, 200);
        // haiku-4-5 is pinned at $1/$5 per MTok: 1000·1_000_000/1e6 = 1000µ$
        // in; 200·5_000_000/1e6 = 1000µ$ out; total 2000µ$
        assert_eq!(parsed.cost_usd_micros, 1000 + 1000);
        assert_eq!(
            parsed.model, "claude-haiku-4-5-20251001",
            "snapshot id recorded"
        );
    }

    #[test]
    fn missing_content_or_usage_is_an_api_error() {
        let no_content = serde_json::json!({ "usage": { "input_tokens": 1, "output_tokens": 1 } });
        assert!(matches!(
            client().parse(&no_content),
            Err(FrontierError::Api { .. })
        ));
        // content present but no block of type "text" (e.g. tool_use only)
        let no_text_block = serde_json::json!({
            "content": [{ "type": "tool_use", "id": "toolu_1", "name": "t", "input": {} }],
            "usage": { "input_tokens": 1, "output_tokens": 1 }
        });
        assert!(matches!(
            client().parse(&no_text_block),
            Err(FrontierError::Api { .. })
        ));
        let no_usage = serde_json::json!({
            "content": [{ "type": "text", "text": "hi" }]
        });
        assert!(matches!(
            client().parse(&no_usage),
            Err(FrontierError::Api { .. })
        ));
    }
}
