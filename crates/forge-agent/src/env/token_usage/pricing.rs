//! LiteLLM-sourced per-model pricing: the parser plus the proxy-aware
//! fetch that populates the runtime cache.
//!
//! Pricing is sourced at runtime from LiteLLM's raw file and cached in
//! redb (`forge_workspace::store::pricing`); there is no bundled copy.

use std::collections::HashMap;
use std::time::Duration;

use forge_primitives::token_usage::ModelPricing;

/// LiteLLM's community-maintained price table (prices Claude and the
/// OpenAI/Codex ids in one file).
const LITELLM_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";

/// Bound on the pricing fetch; the file is ~1.6 MB.
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);

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

    /// Exact-match price for `model_id`; `None` when the id is absent
    /// (an unknown/new model, or `<synthetic>`).
    pub fn price(&self, model_id: &str) -> Option<&ModelPricing> {
        self.map.get(model_id)
    }

    /// Number of priced models; the refresh path rejects an empty parse
    /// so a bad fetch never replaces a good cache.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// Fetch the raw LiteLLM pricing file. Honours an ambient `HTTPS_PROXY`
/// first, then retries once directly so a dead proxy doesn't block the
/// fetch. `None` on any network failure (the caller keeps the last-good
/// cache).
pub async fn fetch_litellm() -> Option<String> {
    if let Some(body) = attempt_fetch(false).await {
        return Some(body);
    }
    attempt_fetch(true).await
}

async fn attempt_fetch(direct: bool) -> Option<String> {
    let mut builder =
        crate::http_trust::with_extra_roots(reqwest::Client::builder().timeout(FETCH_TIMEOUT));
    if direct {
        builder = builder.no_proxy();
    }
    let client = builder.build().ok()?;
    let response = match client.get(LITELLM_URL).send().await {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(
                target: "forge_agent::env::token_usage::pricing",
                %error,
                direct,
                "litellm pricing fetch failed",
            );
            return None;
        }
    };
    if !response.status().is_success() {
        tracing::warn!(
            target: "forge_agent::env::token_usage::pricing",
            status = %response.status(),
            direct,
            "litellm pricing fetch returned non-success",
        );
        return None;
    }
    response.text().await.ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bit-exact float equality: both sides parse the same decimal, so
    /// the values are identical, not merely close.
    fn exact(actual: f64, expected: f64) {
        assert_eq!(actual.to_bits(), expected.to_bits(), "expected {expected}, got {actual}");
    }

    const SAMPLE: &str = r#"{
        "sample_spec": {"input_cost_per_token": 1.0, "output_cost_per_token": 1.0},
        "claude-opus-4-8": {
            "input_cost_per_token": 5e-06,
            "output_cost_per_token": 2.5e-05,
            "cache_creation_input_token_cost": 6.25e-06,
            "cache_creation_input_token_cost_above_1hr": 1e-05,
            "cache_read_input_token_cost": 5e-07
        },
        "gpt-5-codex": {
            "input_cost_per_token": 1.25e-06,
            "output_cost_per_token": 1e-05,
            "cache_read_input_token_cost": 1.25e-07
        },
        "text-embedding-3-small": {"max_tokens": 8191}
    }"#;

    #[test]
    fn maps_litellm_fields_onto_model_pricing() {
        let table = PricingTable::from_litellm_json(SAMPLE);
        let opus = table.price("claude-opus-4-8").expect("opus priced");
        exact(opus.input, 5e-6);
        exact(opus.output, 2.5e-5);
        exact(opus.cache_write_5m, 6.25e-6);
        exact(opus.cache_write_1h, 1e-5);
        exact(opus.cache_read, 5e-7);
    }

    #[test]
    fn gpt_entry_without_cache_creation_zeroes_cache_write() {
        let table = PricingTable::from_litellm_json(SAMPLE);
        let codex = table.price("gpt-5-codex").expect("codex priced");
        exact(codex.input, 1.25e-6);
        exact(codex.cache_read, 1.25e-7);
        exact(codex.cache_write_5m, 0.0);
        exact(codex.cache_write_1h, 0.0);
    }

    #[test]
    fn skips_doc_entry_and_entries_without_core_costs() {
        let table = PricingTable::from_litellm_json(SAMPLE);
        assert!(table.price("sample_spec").is_none(), "the doc entry is skipped");
        assert!(table.price("text-embedding-3-small").is_none(), "no core cost -> skipped");
        assert!(table.price("no-such-model").is_none());
        assert!(table.price("<synthetic>").is_none());
    }

    #[test]
    fn malformed_json_is_an_empty_table() {
        assert!(PricingTable::from_litellm_json("not json").is_empty());
        assert!(PricingTable::from_litellm_json("{}").is_empty());
    }
}
