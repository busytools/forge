//! Anthropic OAuth usage API response shapes.
//!
//! Type-only - the HTTP fetcher lives in the forge-providers
//! backends. These are the JSON wire shapes; the fetcher
//! deserializes into them.

use serde::{Deserialize, Serialize};

/// Top-level OAuth usage payload. All fields are optional because the
/// API can omit any window for accounts that don't qualify (free tier,
/// new account, etc.).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OauthUsage {
    /// Rolling 5-hour rate-limit window (the "session" budget).
    pub five_hour: Option<OauthUsageWindow>,
    /// Rolling 7-day rate-limit window across all models.
    pub seven_day: Option<OauthUsageWindow>,
    /// Rolling 7-day rate-limit window scoped to Opus.
    pub seven_day_opus: Option<OauthUsageWindow>,
    /// Rolling 7-day rate-limit window scoped to Sonnet.
    pub seven_day_sonnet: Option<OauthUsageWindow>,
    /// Pay-as-you-go credit balance, when the account opted in.
    pub extra_usage: Option<OauthExtraUsage>,
}

/// Per-window utilisation. `utilization` is a percentage (0-100);
/// `resets_at` is whatever the API emits (ISO-8601 string or numeric
/// epoch). Consumers parse it themselves.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OauthUsageWindow {
    /// Percentage of the window consumed (0.0-100.0). `None` when the
    /// API omits the field for this window.
    pub utilization: Option<f64>,
    /// When the window resets. Either an ISO-8601 string or a numeric
    /// epoch - kept as raw `serde_json::Value` so callers can parse
    /// whichever form they prefer.
    pub resets_at: Option<serde_json::Value>,
}

/// "Extra usage" pay-as-you-go credit balance.
///
/// Money fields are in **minor units** (cents for USD) as the API
/// returns them - consumers convert to major units (`/ 100.0`) for
/// display.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OauthExtraUsage {
    /// `true` when the account has opted in to pay-as-you-go.
    pub is_enabled: Option<bool>,
    /// Monthly spending cap in minor units (e.g. cents for USD).
    pub monthly_limit: Option<f64>,
    /// Credits consumed in the current period in minor units.
    pub used_credits: Option<f64>,
    /// Percentage of `monthly_limit` consumed (0.0-100.0).
    pub utilization: Option<f64>,
    /// Currency code (e.g. `"USD"`) for the money fields.
    pub currency: Option<String>,
}

/// Failure modes for the OAuth usage fetcher. Variants split
/// fallback-eligible states (`NoCredentials`, `Expired`,
/// `Unauthorized`) from terminal ones so callers can decide whether
/// to retry against a different auth source.
#[derive(Debug, PartialEq, thiserror::Error)]
pub enum OauthUsageError {
    /// No OAuth credentials were resolved from file or keychain.
    /// Caller should advise `/login`.
    #[error("No Claude OAuth credentials found")]
    NoCredentials,
    /// Credentials present but expired locally; caller should advise
    /// `/login` to refresh.
    #[error("Claude OAuth credentials expired")]
    Expired,
    /// API returned 401/403. Token may be stale or revoked.
    #[error("Claude OAuth usage request was rejected (HTTP {0})")]
    Unauthorized(u16),
    /// API returned 403 with `oauth_scope_insufficient`: the token
    /// authenticated but lacks the `user:profile` scope this endpoint
    /// requires. The verdict on a VALID setup token, not an auth
    /// failure - setup tokens carry `user:inference` only.
    #[error("Claude OAuth usage endpoint refused the token's scopes")]
    ScopeInsufficient,
    /// API returned 429. `retry_after` is the parsed `Retry-After`
    /// header value (in seconds) when present - Anthropic returns
    /// per-account hold-down durations that the caller should honour
    /// to avoid hammering the endpoint and keeping the limit hot.
    #[error("Claude OAuth usage request was rate-limited (retry_after={retry_after:?})")]
    RateLimited { retry_after: Option<std::time::Duration> },
    /// API returned an unexpected non-success status (anything other
    /// than 200 / 401 / 403 / 429).
    #[error("Claude OAuth usage request failed with HTTP {0}{1}")]
    HttpStatus(u16, String),
    /// Network error reaching the API.
    #[error("Claude OAuth network error: {0}")]
    Network(String),
    /// The `claude --version` probe that supplies the User-Agent
    /// failed - a local exec problem, not a reachability verdict.
    #[error("Claude CLI version probe failed: {0}")]
    UaProbe(String),
    /// Response body could not be parsed.
    #[error("Failed to decode Claude OAuth usage response: {0}")]
    Decode(String),
}

impl OauthUsageError {
    /// True when this is a 429 rate-limit response. Used by callers
    /// that schedule the next probe attempt against the
    /// `Retry-After` value.
    pub fn is_rate_limited(&self) -> bool {
        matches!(self, Self::RateLimited { .. })
    }
}
