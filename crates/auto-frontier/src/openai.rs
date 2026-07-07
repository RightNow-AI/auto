//! OpenAI chat-completions provider.
//!
//! Blocking HTTP via ureq 3 (rustls). Request/response shapes verified
//! 2026-07-04 against <https://developers.openai.com/api/docs/api-reference/chat>:
//! POST /v1/chat/completions with `model`, `messages` (`system` + `user`
//! roles), `max_completion_tokens`; answer at `choices[0].message.content`;
//! usage at `usage.prompt_tokens` / `usage.completion_tokens`; auth
//! `Authorization: Bearer <key>`.
//!
//! Cost is computed HERE from the pinned price table (`prices.rs`) using the
//! usage the provider reported — never estimated from text length. The model
//! id must be pinned; construction refuses otherwise. Note for reasoning
//! models (gpt-5.x): reasoning tokens are billed inside `completion_tokens`,
//! so usage-based costing counts them automatically, and
//! `max_completion_tokens` bounds them, which keeps the capped client's
//! worst-case pre-check sound.
//!
//! This provider does NOT enforce the spend cap — wrap it in
//! [`crate::CappedFrontier`] (the only construction the CLI exposes;
//! ADR-0010).

use std::time::Duration;

use serde_json::{Value, json};

use crate::prices::price_of;
use crate::{Frontier, FrontierError, FrontierRequest, FrontierResponse};

/// Environment variable holding the API key.
pub const OPENAI_KEY_ENV: &str = "OPENAI_API_KEY";

const ENDPOINT: &str = "https://api.openai.com/v1/chat/completions";
const TIMEOUT: Duration = Duration::from_secs(120);

/// A blocking OpenAI chat-completions client for one pinned model.
pub struct OpenAiFrontier {
    model: String,
    key: String,
}

/// Manual impl: the API key must never appear in debug output or error text.
impl std::fmt::Debug for OpenAiFrontier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiFrontier")
            .field("model", &self.model)
            .field("key", &"<redacted>")
            .finish()
    }
}

impl OpenAiFrontier {
    /// A client for `model` (must be in the pinned price table). `key` falls
    /// back to `$OPENAI_API_KEY`; absent-or-empty is a [`FrontierError::MissingKey`]
    /// refusal — nothing is ever sent unauthenticated.
    pub fn new(model: &str, key: Option<String>) -> Result<Self, FrontierError> {
        price_of(model)?;
        let key = match key.filter(|k| !k.trim().is_empty()).or_else(|| {
            std::env::var(OPENAI_KEY_ENV)
                .ok()
                .filter(|k| !k.trim().is_empty())
        }) {
            Some(k) => k.trim().to_owned(),
            None => {
                return Err(FrontierError::MissingKey {
                    env_var: OPENAI_KEY_ENV.to_owned(),
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
            "messages": [
                { "role": "system", "content": request.system },
                { "role": "user", "content": request.user },
            ],
            "max_completion_tokens": request.max_output_tokens,
        })
    }

    /// Parse a chat-completions response body into a [`FrontierResponse`],
    /// computing cost from the pinned table — a pure function, unit-tested
    /// against a fixture copied from the API reference.
    fn parse(&self, body: &Value) -> Result<FrontierResponse, FrontierError> {
        let text = body
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .ok_or_else(|| FrontierError::Api {
                detail: format!(
                    "response has no text at choices[0].message.content; body began {:?}",
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
        let input_tokens = usage("prompt_tokens")?;
        let output_tokens = usage("completion_tokens")?;
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

impl Frontier for OpenAiFrontier {
    fn complete(&mut self, request: &FrontierRequest) -> Result<FrontierResponse, FrontierError> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(TIMEOUT))
            .build()
            .new_agent();

        let mut response = agent
            .post(ENDPOINT)
            .header("authorization", &format!("Bearer {}", self.key))
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
    fn client() -> OpenAiFrontier {
        OpenAiFrontier::new("gpt-5.4-mini", Some("test-key-never-sent".into()))
            .expect("pinned model + explicit key")
    }

    #[test]
    fn unpinned_model_refused_at_construction() {
        let err = OpenAiFrontier::new("gpt-mystery", Some("k".into()))
            .expect_err("unpinned model has no price");
        assert!(matches!(err, FrontierError::UnknownModel { .. }));
    }

    #[test]
    fn explicit_empty_key_without_env_is_missing_key() {
        // an explicitly empty key must not silently authenticate; the env
        // fallback may legitimately supply one on operator machines, so only
        // assert the refusal shape when the env is ALSO empty
        if std::env::var(OPENAI_KEY_ENV)
            .map(|v| v.trim().is_empty())
            .unwrap_or(true)
        {
            let err = OpenAiFrontier::new("gpt-5.4-mini", Some("   ".into()))
                .expect_err("blank key refused");
            assert!(
                matches!(err, FrontierError::MissingKey { env_var } if env_var == OPENAI_KEY_ENV)
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
        assert_eq!(body["model"], "gpt-5.4-mini");
        assert_eq!(body["max_completion_tokens"], 128);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "be the reference");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "{\"prompt\":\"x\"}");
        // exactly the documented fields — nothing undocumented rides along
        assert_eq!(body.as_object().unwrap().len(), 3);
    }

    #[test]
    fn response_parsing_reads_text_usage_and_computes_pinned_cost() {
        // shape per the chat API reference (developers.openai.com, 2026-07-04)
        let body = serde_json::json!({
            "id": "chatcmpl-123",
            "object": "chat.completion",
            "model": "gpt-5.4-mini-2026-05",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "\"urgent\"" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 1000, "completion_tokens": 200, "total_tokens": 1200 }
        });
        let parsed = client().parse(&body).expect("fixture parses");
        assert_eq!(parsed.text, "\"urgent\"");
        assert_eq!(parsed.input_tokens, 1000);
        assert_eq!(parsed.output_tokens, 200);
        // 1000 in @ $0.75/MTok = 750µ$·10^-3 → ceil(0.75·1000)=750µ$/1000... :
        // 1000·750000/1e6 = 750µ$; 200·4500000/1e6 = 900µ$; total 1650µ$
        assert_eq!(parsed.cost_usd_micros, 750 + 900);
        assert_eq!(parsed.model, "gpt-5.4-mini-2026-05", "snapshot id recorded");
    }

    #[test]
    fn missing_content_or_usage_is_an_api_error() {
        let no_choices =
            serde_json::json!({ "usage": { "prompt_tokens": 1, "completion_tokens": 1 } });
        assert!(matches!(
            client().parse(&no_choices),
            Err(FrontierError::Api { .. })
        ));
        let no_usage = serde_json::json!({
            "choices": [{ "message": { "content": "hi" } }]
        });
        assert!(matches!(
            client().parse(&no_usage),
            Err(FrontierError::Api { .. })
        ));
    }
}
