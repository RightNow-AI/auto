//! Spend-capped frontier client — the ONLY path in this workspace to a paid
//! model API (CLAUDE.md guardrail: hard cap per session, logged, no paid
//! runs beyond the cap without owner authorization).
//!
//! Fail-closed by construction: a cap of 0 refuses every paid call, and 0
//! is the default everywhere a cap is read. Costs are computed from a
//! pinned price table and written to an append-only ledger BEFORE the
//! response is returned to the caller; a request whose worst case would
//! cross the cap is refused without being issued.
//!
//! This module holds the frozen seam (trait + wire types + the scripted
//! test fake). The capped wrapper, the ledger, and the Anthropic HTTP
//! client live in sibling modules.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

pub mod anthropic;
pub mod capped;
pub mod ledger;
pub mod openai;
pub mod prices;

pub use anthropic::{ANTHROPIC_KEY_ENV, AnthropicFrontier};
pub use capped::CappedFrontier;
pub use ledger::{LEDGER_PATH_ENV, LedgerEntry, SpendLedger, now_unix_ms};
pub use openai::{OPENAI_KEY_ENV, OpenAiFrontier};
pub use prices::{ModelPrice, PRICE_TABLE_VERSION, PRICES, price_of};

/// One completion request. `system` carries task framing; `user` carries
/// the observations/input; `max_output_tokens` bounds the response AND the
/// worst-case cost estimate the cap check uses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontierRequest {
    pub system: String,
    pub user: String,
    pub max_output_tokens: u32,
}

/// One completion response with the usage the provider reported and the
/// cost computed from the pinned price table — never estimated after the
/// fact, never fabricated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontierResponse {
    pub text: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// micro-USD, computed from the price table entry for `model`
    pub cost_usd_micros: u64,
    pub model: String,
}

/// Every honest way a frontier call can fail. `CapExceeded` and
/// `MissingKey` are refusals (nothing was sent); the rest are failures of
/// an attempted call.
#[derive(Debug, thiserror::Error)]
pub enum FrontierError {
    /// no API key in the environment — refused before any request
    #[error("no API key: set {env_var} (paid calls are fail-closed)")]
    MissingKey { env_var: String },
    /// issuing this request could cross the session cap — refused unsent
    #[error(
        "spend cap would be exceeded: spent {spent_usd_micros}µ$ + worst-case \
         {estimated_usd_micros}µ$ > cap {cap_usd_micros}µ$ (raise --spend-cap-usd only \
         with owner authorization)"
    )]
    CapExceeded {
        spent_usd_micros: u64,
        estimated_usd_micros: u64,
        cap_usd_micros: u64,
    },
    /// model absent from the pinned price table — cost would be a guess,
    /// so the call is refused
    #[error("unknown model {model:?}: not in the pinned price table, cost cannot be computed")]
    UnknownModel { model: String },
    /// transport-level failure (connect, TLS, timeout)
    #[error("http failure: {detail}")]
    Http { detail: String },
    /// the provider answered with an error or an unparseable body
    #[error("api error: {detail}")]
    Api { detail: String },
    /// the spend ledger could not be read or appended — treated as fatal
    /// because an unrecorded paid call would violate the guardrail
    #[error("ledger failure: {detail}")]
    Ledger { detail: String },
}

/// The frozen seam: anything that can answer a completion request. Callers
/// (CEGIS, tier-0) depend on this trait only, so tests script it and the
/// capped client wraps any implementation.
pub trait Frontier {
    fn complete(&mut self, request: &FrontierRequest) -> Result<FrontierResponse, FrontierError>;
    fn model_id(&self) -> &str;
}

/// Test fake: answers from a fixed script, records every request it was
/// asked. NOT a mock pretending to be a frontier model — tests that use it
/// are testing the caller's protocol, and say so.
#[derive(Debug, Default)]
pub struct ScriptedFrontier {
    model: String,
    script: VecDeque<Result<FrontierResponse, FrontierError>>,
    pub requests: Vec<FrontierRequest>,
}

impl ScriptedFrontier {
    pub fn new(model: &str) -> Self {
        Self {
            model: model.to_owned(),
            script: VecDeque::new(),
            requests: Vec::new(),
        }
    }

    /// Queue a full response (explicit usage numbers — tests own their
    /// arithmetic).
    pub fn push_response(&mut self, response: FrontierResponse) {
        self.script.push_back(Ok(response));
    }

    /// Queue a text answer with explicit token/cost accounting.
    pub fn push_text(&mut self, text: &str, input_tokens: u64, output_tokens: u64, cost: u64) {
        let model = self.model.clone();
        self.push_response(FrontierResponse {
            text: text.to_owned(),
            input_tokens,
            output_tokens,
            cost_usd_micros: cost,
            model,
        });
    }

    /// Queue a failure.
    pub fn push_error(&mut self, error: FrontierError) {
        self.script.push_back(Err(error));
    }
}

impl Frontier for ScriptedFrontier {
    fn complete(&mut self, request: &FrontierRequest) -> Result<FrontierResponse, FrontierError> {
        self.requests.push(request.clone());
        self.script.pop_front().unwrap_or_else(|| {
            Err(FrontierError::Api {
                detail: "scripted frontier exhausted: more calls than scripted responses".into(),
            })
        })
    }

    fn model_id(&self) -> &str {
        &self.model
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(user: &str) -> FrontierRequest {
        FrontierRequest {
            system: "sys".into(),
            user: user.to_owned(),
            max_output_tokens: 64,
        }
    }

    #[test]
    fn scripted_frontier_answers_in_order_and_records_requests() {
        let mut f = ScriptedFrontier::new("scripted-model");
        f.push_text("first", 10, 5, 25);
        f.push_error(FrontierError::Http {
            detail: "scripted outage".into(),
        });

        let first = f.complete(&request("a")).expect("first is scripted Ok");
        assert_eq!(first.text, "first");
        assert_eq!(first.cost_usd_micros, 25);
        assert!(matches!(
            f.complete(&request("b")),
            Err(FrontierError::Http { .. })
        ));
        assert_eq!(f.requests.len(), 2);
        assert_eq!(f.requests[1].user, "b");
    }

    #[test]
    fn exhausted_script_is_an_api_error_not_a_panic() {
        let mut f = ScriptedFrontier::new("scripted-model");
        let result = f.complete(&request("unscripted"));
        assert!(
            matches!(result, Err(FrontierError::Api { detail }) if detail.contains("exhausted"))
        );
    }
}
