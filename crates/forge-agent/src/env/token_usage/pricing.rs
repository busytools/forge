//! LiteLLM-sourced per-model pricing: a bundled seed table plus the
//! parser the background refresh reuses on a freshly-fetched copy.
//!
//! Seed regenerated from the LiteLLM raw file:
//! `https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json`

use std::collections::HashMap;

use forge_primitives::token_usage::ModelPricing;

/// Bundled seed copy of the LiteLLM pricing table, embedded so the
/// first `/usage` open (and any offline run) prices known models with
/// no network round-trip.
const SEED_JSON: &str = include_str!("model_prices_seed.json");

/// Per-model USD pricing keyed by the exact `message.model` id.
pub struct PricingTable {
    map: HashMap<String, ModelPricing>,
}

impl PricingTable {
    /// Parse a LiteLLM `model_prices_and_context_window.json` body.
    /// Entries missing either core per-token cost are skipped; a
    /// malformed body yields an empty table (logged), never a panic.
    pub fn from_litellm_json(json: &str) -> Self {
        let value: serde_json::Value = match serde_json::from_str(json) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(
                    target: "forge_agent::env::token_usage::pricing",
                    %error,
                    "litellm pricing json did not parse; using an empty table",
                );
                return Self { map: HashMap::new() };
            }
        };
        let Some(entries) = value.as_object() else {
            return Self { map: HashMap::new() };
        };
        let mut map = HashMap::with_capacity(entries.len());
        for (model_id, entry) in entries {
            // LiteLLM ships a `sample_spec` documentation entry with
            // placeholder cost fields; it is not a real model id.
            if model_id == "sample_spec" {
                continue;
            }
            let field = |name: &str| entry.get(name).and_then(serde_json::Value::as_f64);
            let (Some(input), Some(output)) =
                (field("input_cost_per_token"), field("output_cost_per_token"))
            else {
                continue;
            };
            map.insert(
                model_id.clone(),
                ModelPricing {
                    input,
                    output,
                    cache_write_5m: field("cache_creation_input_token_cost").unwrap_or(0.0),
                    cache_write_1h: field("cache_creation_input_token_cost_above_1hr")
                        .unwrap_or(0.0),
                    cache_read: field("cache_read_input_token_cost").unwrap_or(0.0),
                },
            );
        }
        Self { map }
    }

    /// Parse the bundled seed table.
    pub fn seed() -> Self {
        Self::from_litellm_json(SEED_JSON)
    }

    /// Exact-match price for `model_id`; `None` when the id is absent
    /// (an unknown/new model, or `<synthetic>`).
    pub fn price(&self, model_id: &str) -> Option<&ModelPricing> {
        self.map.get(model_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bit-exact float equality: both sides parse the same decimal, so
    /// the values are identical, not merely close.
    fn exact(actual: f64, expected: f64) {
        assert_eq!(actual.to_bits(), expected.to_bits(), "expected {expected}, got {actual}");
    }

    #[test]
    fn seed_prices_opus_with_exact_per_token_values() {
        let table = PricingTable::seed();
        let p = table.price("claude-opus-4-8").expect("opus priced in seed");
        exact(p.input, 5e-6);
        exact(p.output, 2.5e-5);
        exact(p.cache_write_5m, 6.25e-6);
        exact(p.cache_write_1h, 1e-5);
        exact(p.cache_read, 5e-7);
    }

    #[test]
    fn seed_prices_gpt_5_codex_with_zeroed_cache_write() {
        let table = PricingTable::seed();
        let p = table.price("gpt-5-codex").expect("codex priced in seed");
        exact(p.input, 1.25e-6);
        exact(p.output, 1e-5);
        exact(p.cache_read, 1.25e-7);
        // The OpenAI entry carries no cache-creation cost -> zeroed.
        exact(p.cache_write_5m, 0.0);
        exact(p.cache_write_1h, 0.0);
    }

    #[test]
    fn unknown_and_synthetic_ids_are_unpriced() {
        let table = PricingTable::seed();
        assert!(table.price("no-such-model-xyz").is_none());
        assert!(table.price("<synthetic>").is_none());
    }
}
