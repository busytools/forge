//! OpenRouter `/api/v1/key` response shapes.
//!
//! Type-only - the HTTP fetcher lives in
//! `forge_agent::cloud::oauth_usage`. These are the JSON wire shapes;
//! the fetcher deserializes into them.

use serde::{Deserialize, Serialize};

/// Envelope. The endpoint wraps everything in a single `data` object.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct KeyResponse {
    pub data: Option<KeyData>,
}

/// Per-key spend. The figures are scoped to the key the request
/// authenticated with, not to the account: the same endpoint called
/// with a different key on the same account returns that key's own
/// numbers. Today, this week and this month arrive pre-computed, so
/// forge does no summation and no timezone arithmetic.
///
/// Deliberately partial. The payload also carries a truncated copy of
/// the key itself, a creator id, an all-time total, a cap triple and
/// `byok_*` figures for inference billed to a different provider
/// account; none of them are mapped. See `ApiSpend`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct KeyData {
    /// Spend since the start of today, in USD.
    pub usage_daily: Option<f64>,
    /// Spend since the start of this week, in USD.
    pub usage_weekly: Option<f64>,
    /// Spend since the start of this month, in USD.
    pub usage_monthly: Option<f64>,
}
