//! Token/cost accounting shapes for the `/usage` view.
//!
//! Distinct from [`crate::usage`] (Anthropic plan/subscription
//! utilization): these describe per-model and per-project token counts
//! plus a notional API-equivalent cost, rolled up from the session
//! JSONL pool. The scanner lives in `forge-agent::env::token_usage`,
//! the redb cache in `forge-workspace::store::token_usage`.

use serde::{Deserialize, Serialize};

/// Per-token USD pricing for one model, sourced from the LiteLLM table.
/// Cache-write splits by TTL tier to match the JSONL's
/// `cache_creation.ephemeral_{1h,5m}` breakdown.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelPricing {
    pub input: f64,
    pub output: f64,
    pub cache_write_1h: f64,
    pub cache_write_5m: f64,
    pub cache_read: f64,
}

/// One row of the usage table: `label` is a model id, a folded project
/// name, or `"TOTAL"` for the aggregate row. Token counts are split;
/// `cost_usd` is notional (at API pricing, not a bill).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageRow {
    pub label: String,
    pub input: u64,
    pub cache_write_1h: u64,
    pub cache_write_5m: u64,
    pub cache_read: u64,
    pub output: u64,
    pub cost_usd: f64,
}

impl UsageRow {
    /// Total tokens across the five token fields.
    pub fn tokens(&self) -> u64 {
        self.input
            .saturating_add(self.cache_write_1h)
            .saturating_add(self.cache_write_5m)
            .saturating_add(self.cache_read)
            .saturating_add(self.output)
    }
}

/// One time window's usage: the same data grouped two ways plus the
/// aggregate `total` row. Each `Vec` is sorted by cost descending.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowUsage {
    pub by_model: Vec<UsageRow>,
    pub by_project: Vec<UsageRow>,
    pub total: UsageRow,
}

/// The four rolling windows the `/usage` overlay renders. `lifetime` is
/// a static sum; the other three are recomputed relative to now.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageReport {
    pub today: WindowUsage,
    pub week: WindowUsage,
    pub month: WindowUsage,
    pub lifetime: WindowUsage,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(label: &str) -> UsageRow {
        UsageRow {
            label: label.to_owned(),
            input: 1,
            cache_write_1h: 2,
            cache_write_5m: 4,
            cache_read: 8,
            output: 16,
            cost_usd: 1.5,
        }
    }

    #[test]
    fn tokens_sums_the_five_fields() {
        assert_eq!(row("opus").tokens(), 1 + 2 + 4 + 8 + 16);
    }

    #[test]
    fn usage_report_round_trips_through_serde() {
        let window = WindowUsage {
            by_model: vec![row("opus"), row("sonnet")],
            by_project: vec![row("forge")],
            total: row("TOTAL"),
        };
        let report = UsageReport {
            today: window.clone(),
            week: window.clone(),
            month: window.clone(),
            lifetime: window,
        };
        let json = serde_json::to_string(&report).expect("serialize");
        let back: UsageReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(report, back);
    }
}
