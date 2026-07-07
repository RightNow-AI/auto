//! Pinned price table.
//!
//! Prices are stored as **micro-USD per 1,000,000 tokens** (µ$/MTok). Every
//! Anthropic list price is a whole number of cents per MTok, so this is an
//! exact integer — no floats anywhere in the cost path. Cost is computed with
//! integer **ceiling** division, so a fractional per-request cost never rounds
//! *down*: the ledger and the cap always see a figure at or above the true
//! charge (the safe, fail-closed direction).
//!
//! Price table version: `v1`
//! Sources (both retrieved 2026-07-04):
//! - Anthropic: <https://platform.claude.com/docs/en/about-claude/pricing>
//! - OpenAI: <https://developers.openai.com/api/docs/pricing>
//!
//! Sonnet-5 introductory-pricing note: Claude Sonnet 5 lists an introductory
//! rate of $2/$10 per MTok through 2026-08-31, reverting to the standard
//! $3/$15. We pin the **standard** $3/$15. Over-stating the price only makes
//! the worst-case cap check and the recorded spend more conservative — never
//! less — so pinning the higher (permanent) rate keeps the client fail-closed.

use crate::FrontierError;

/// Version stamp for the pinned table (goes into ADR-0010 and manifests).
pub const PRICE_TABLE_VERSION: &str = "v1";
/// Where the pinned numbers came from.
pub const PRICE_TABLE_SOURCES: &[&str] = &[
    "https://platform.claude.com/docs/en/about-claude/pricing",
    "https://developers.openai.com/api/docs/pricing",
];
/// When the pinned numbers were retrieved.
pub const PRICE_TABLE_RETRIEVED: &str = "2026-07-04";

/// micro-USD in one US dollar.
const UMICROS_PER_DOLLAR: u64 = 1_000_000;
/// Denominator for the µ$/MTok representation.
const MTOK: u128 = 1_000_000;

/// One pinned model price. All cost math derives from these two integers only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelPrice {
    /// The model id as sent on the wire (e.g. `claude-haiku-4-5`).
    pub model_id: &'static str,
    /// micro-USD per 1,000,000 input tokens.
    pub input_umicros_per_mtok: u64,
    /// micro-USD per 1,000,000 output tokens.
    pub output_umicros_per_mtok: u64,
}

impl ModelPrice {
    /// Ceiling cost in µ$ for `tokens` input tokens.
    pub fn input_cost(&self, tokens: u64) -> u64 {
        ceil_umicros(tokens, self.input_umicros_per_mtok)
    }

    /// Ceiling cost in µ$ for `tokens` output tokens.
    pub fn output_cost(&self, tokens: u64) -> u64 {
        ceil_umicros(tokens, self.output_umicros_per_mtok)
    }
}

/// `$whole.cents / MTok` expressed in µ$/MTok. `$1/MTok` -> `1_000_000`.
const fn per_mtok(whole_dollars: u64, cents: u64) -> u64 {
    whole_dollars * UMICROS_PER_DOLLAR + cents * (UMICROS_PER_DOLLAR / 100)
}

/// The pinned table. A model absent from this list is **refused**, never priced
/// by guess (see [`price_of`]). Verified 2026-07-04 from the pricing page.
pub const PRICES: &[ModelPrice] = &[
    // Claude Haiku 4.5 — the cheap default. $1 / $5 per MTok.
    ModelPrice {
        model_id: "claude-haiku-4-5",
        input_umicros_per_mtok: per_mtok(1, 0),
        output_umicros_per_mtok: per_mtok(5, 0),
    },
    // Dated snapshot of Haiku 4.5 (same price; the API may resolve to either).
    ModelPrice {
        model_id: "claude-haiku-4-5-20251001",
        input_umicros_per_mtok: per_mtok(1, 0),
        output_umicros_per_mtok: per_mtok(5, 0),
    },
    // Claude Sonnet 4.6 — $3 / $15 per MTok.
    ModelPrice {
        model_id: "claude-sonnet-4-6",
        input_umicros_per_mtok: per_mtok(3, 0),
        output_umicros_per_mtok: per_mtok(15, 0),
    },
    // Claude Sonnet 5 — pinned at the standard $3 / $15 (intro $2/$10 ignored;
    // see module docs — over-stating price is the fail-closed direction).
    ModelPrice {
        model_id: "claude-sonnet-5",
        input_umicros_per_mtok: per_mtok(3, 0),
        output_umicros_per_mtok: per_mtok(15, 0),
    },
    // Claude Opus 4.8 — $5 / $25 per MTok.
    ModelPrice {
        model_id: "claude-opus-4-8",
        input_umicros_per_mtok: per_mtok(5, 0),
        output_umicros_per_mtok: per_mtok(25, 0),
    },
    // OpenAI gpt-5.4-nano — $0.20 / $1.25 per MTok (cheapest text model).
    ModelPrice {
        model_id: "gpt-5.4-nano",
        input_umicros_per_mtok: per_mtok(0, 20),
        output_umicros_per_mtok: per_mtok(1, 25),
    },
    // OpenAI gpt-5.4-mini — $0.75 / $4.50 per MTok (the live-fire default:
    // the owner's key is OpenAI).
    ModelPrice {
        model_id: "gpt-5.4-mini",
        input_umicros_per_mtok: per_mtok(0, 75),
        output_umicros_per_mtok: per_mtok(4, 50),
    },
    // OpenAI gpt-5.4 — $2.50 / $15 per MTok.
    ModelPrice {
        model_id: "gpt-5.4",
        input_umicros_per_mtok: per_mtok(2, 50),
        output_umicros_per_mtok: per_mtok(15, 0),
    },
    // OpenAI gpt-5.5 — $5 / $30 per MTok.
    ModelPrice {
        model_id: "gpt-5.5",
        input_umicros_per_mtok: per_mtok(5, 0),
        output_umicros_per_mtok: per_mtok(30, 0),
    },
];

/// `ceil(tokens * umicros_per_mtok / 1_000_000)` in µ$ — exact integer, rounds UP.
///
/// The u128 intermediate cannot overflow for any realistic token count
/// (`u64::MAX` tokens times `u64::MAX` price is still `< u128::MAX`).
pub(crate) fn ceil_umicros(tokens: u64, umicros_per_mtok: u64) -> u64 {
    let num = tokens as u128 * umicros_per_mtok as u128;
    let ceil = num.div_ceil(MTOK);
    // Fits in u64: ceil <= num/MTOK + 1, and num/MTOK <= u64::MAX for any
    // input where tokens*price < MTOK * u64::MAX (always true here).
    ceil as u64
}

/// Look up a pinned price, or refuse with [`FrontierError::UnknownModel`].
/// Never guesses a price for an unknown model.
pub fn price_of(model_id: &str) -> Result<&'static ModelPrice, FrontierError> {
    PRICES
        .iter()
        .find(|p| p.model_id == model_id)
        .ok_or_else(|| FrontierError::UnknownModel {
            model: model_id.to_owned(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dollars_per_mtok_is_umicros_per_token_for_whole_dollars() {
        // $1/MTok == 1 µ$/token exactly.
        assert_eq!(per_mtok(1, 0), 1_000_000);
        assert_eq!(per_mtok(5, 0), 5_000_000);
        assert_eq!(per_mtok(25, 0), 25_000_000);
        // sub-dollar rates stay exact integers too: $0.80/MTok == 800_000.
        assert_eq!(per_mtok(0, 80), 800_000);
    }

    #[test]
    fn ceil_one_token_rounds_to_the_per_token_rate() {
        // 1 input token on Haiku ($1/MTok) costs exactly 1 µ$.
        assert_eq!(ceil_umicros(1, per_mtok(1, 0)), 1);
        // 1 output token on Haiku ($5/MTok) costs exactly 5 µ$.
        assert_eq!(ceil_umicros(1, per_mtok(5, 0)), 5);
        // 1 output token on Opus ($25/MTok) costs exactly 25 µ$.
        assert_eq!(ceil_umicros(1, per_mtok(25, 0)), 25);
    }

    #[test]
    fn ceil_rounds_up_a_fractional_per_token_cost() {
        // $0.80/MTok == 0.8 µ$/token; ceil never rounds down.
        assert_eq!(ceil_umicros(1, 800_000), 1); // ceil(0.8) = 1
        assert_eq!(ceil_umicros(2, 800_000), 2); // ceil(1.6) = 2
        assert_eq!(ceil_umicros(3, 800_000), 3); // ceil(2.4) = 3
        assert_eq!(ceil_umicros(5, 800_000), 4); // ceil(4.0) = 4 (exact)
    }

    #[test]
    fn ceil_zero_tokens_is_zero() {
        assert_eq!(ceil_umicros(0, per_mtok(25, 0)), 0);
        assert_eq!(ceil_umicros(0, 0), 0);
    }

    #[test]
    fn model_cost_is_input_plus_output() {
        let haiku = price_of("claude-haiku-4-5").expect("haiku is pinned");
        assert_eq!(haiku.input_cost(1000), 1000); // 1000 tokens * 1 µ$/tok
        assert_eq!(haiku.output_cost(500), 2500); // 500 tokens * 5 µ$/tok
    }

    #[test]
    fn known_models_resolve_to_verified_prices() {
        let haiku = price_of("claude-haiku-4-5").expect("pinned");
        assert_eq!(haiku.input_umicros_per_mtok, 1_000_000);
        assert_eq!(haiku.output_umicros_per_mtok, 5_000_000);
        let opus = price_of("claude-opus-4-8").expect("pinned");
        assert_eq!(opus.input_umicros_per_mtok, 5_000_000);
        assert_eq!(opus.output_umicros_per_mtok, 25_000_000);
        let sonnet = price_of("claude-sonnet-4-6").expect("pinned");
        assert_eq!(sonnet.output_umicros_per_mtok, 15_000_000);
    }

    #[test]
    fn openai_models_resolve_to_verified_prices() {
        let mini = price_of("gpt-5.4-mini").expect("pinned");
        assert_eq!(mini.input_umicros_per_mtok, 750_000);
        assert_eq!(mini.output_umicros_per_mtok, 4_500_000);
        let nano = price_of("gpt-5.4-nano").expect("pinned");
        assert_eq!(nano.input_umicros_per_mtok, 200_000);
        assert_eq!(nano.output_umicros_per_mtok, 1_250_000);
    }

    #[test]
    fn unknown_model_is_refused_not_guessed() {
        let err = price_of("gpt-4o").expect_err("must refuse unknown model");
        assert!(matches!(err, FrontierError::UnknownModel { model } if model == "gpt-4o"));
    }
}
