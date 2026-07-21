//! Cost calculation for provider token usage against a per-model pricing
//! table. `Usage` (Phase 3) already tracks four token-count fields per
//! provider call — `input_tokens`/`output_tokens`/`cache_read_tokens`/
//! `cache_write_tokens` — this module folds those against a matching
//! per-token USD rate for one model. Lives in `rokr-core` next to `Usage`
//! itself (ticket 56, cost-pricing-math) rather than in `rokr-config`: the
//! two crates have no dependency edge on each other (`rokr-config` doesn't
//! depend on `rokr-core`, and vice versa), so `rokr-config`'s `ModelPricing`
//! config type and this module's `PricingEntry` are deliberately separate,
//! same-shaped types — mirroring how `Config::mcp`/`Config::hooks` already
//! define their own crate-local types rather than reusing another crate's.

use crate::Usage;

/// Per-token USD pricing for one model, one rate per token type [`Usage`]
/// tracks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PricingEntry {
    pub input_price_per_token: f64,
    pub output_price_per_token: f64,
    pub cache_read_price_per_token: f64,
    pub cache_write_price_per_token: f64,
}

/// Computes the USD cost of `usage` against `pricing`: each of `Usage`'s
/// four token-count fields multiplied by its matching per-token rate,
/// summed. `pricing: None` (no entry for the model in question -- the
/// caller's job to resolve, not this function's, since it stays decoupled
/// from string model keys) falls back to a predictable `0.0` rather than
/// panicking or guessing.
pub fn calculate_cost(usage: Usage, pricing: Option<&PricingEntry>) -> f64 {
    match pricing {
        Some(pricing) => {
            usage.input_tokens as f64 * pricing.input_price_per_token
                + usage.output_tokens as f64 * pricing.output_price_per_token
                + usage.cache_read_tokens as f64 * pricing.cache_read_price_per_token
                + usage.cache_write_tokens as f64 * pricing.cache_write_price_per_token
        }
        // An unpriced model (no pricing entry) is common (a new/unlisted
        // model, or a user override table missing an entry) and must never
        // panic or produce a misleading nonzero figure -- $0.00 is the
        // predictable fallback a caller can display or sum without special-
        // casing.
        None => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Usage;

    #[test]
    fn calculate_cost_multiplies_each_token_type_by_its_configured_price_and_sums() {
        let usage = Usage {
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 200,
            cache_write_tokens: 100,
        };
        let pricing = PricingEntry {
            input_price_per_token: 0.000_003,
            output_price_per_token: 0.000_015,
            cache_read_price_per_token: 0.000_000_3,
            cache_write_price_per_token: 0.000_003_75,
        };

        let cost = calculate_cost(usage, Some(&pricing));

        let expected = 1000.0 * 0.000_003
            + 500.0 * 0.000_015
            + 200.0 * 0.000_000_3
            + 100.0 * 0.000_003_75;
        assert!(
            (cost - expected).abs() < 1e-12,
            "expected cost {expected}, got {cost}"
        );
    }

    #[test]
    fn calculate_cost_falls_back_predictably_for_a_model_with_no_pricing_entry() {
        let usage = Usage {
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 200,
            cache_write_tokens: 100,
        };

        let cost = calculate_cost(usage, None);

        assert_eq!(
            cost, 0.0,
            "an unpriced model (no pricing entry) should fall back to a predictable $0.00, \
             never panic or produce a nonsensical figure"
        );
    }
}
