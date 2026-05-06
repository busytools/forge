//! Anthropic OAuth usage API client.
//!
//! Fetches per-account rate-limit utilisation from
//! `https://api.anthropic.com/api/oauth/usage` using the OAuth
//! bearer credentials resolved by
//! [`super::oauth_credentials::load_oauth_credentials`] (file or —
//! on macOS — keychain). The `Authorization` header never escapes
//! this module.
//!
//! Lifted from forge-sdk in 2026-05-05. Direct hits on
//! `api.anthropic.com` belong with the agent — forge-sdk's job is
//! to wrap the `claude` CLI subprocess, not to talk HTTP to
//! Anthropic.
//!
//! The response shape mirrors the live API as of 2026-04, exposed as
//! plain optional fields. Timestamp parsing is left to consumers
//! because the field is documented inconsistently (sometimes ISO-8601,
//! sometimes a numeric epoch).

use std::time::Duration;

use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use serde::{Deserialize, Serialize};

use super::oauth_credentials::load_oauth_credentials;

const OAUTH_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const OAUTH_BETA_HEADER: &str = "oauth-2025-04-20";
const OAUTH_TIMEOUT: Duration = Duration::from_secs(8);

/// Top-level OAuth usage payload. All fields are optional because the
/// API can omit any window for accounts that don't qualify (free tier,
/// new account, etc.).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
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
#[non_exhaustive]
pub struct OauthUsageWindow {
    /// Percentage of the window consumed (0.0–100.0). `None` when the
    /// API omits the field for this window.
    pub utilization: Option<f64>,
    /// When the window resets. Either an ISO-8601 string or a numeric
    /// epoch — kept as raw `serde_json::Value` so callers can parse
    /// whichever form they prefer.
    pub resets_at: Option<serde_json::Value>,
}

/// "Extra usage" pay-as-you-go credit balance.
///
/// Money fields are in **minor units** (cents for USD) as the API
/// returns them — consumers convert to major units (`/ 100.0`) for
/// display.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OauthExtraUsage {
    /// `true` when the account has opted in to pay-as-you-go.
    pub is_enabled: Option<bool>,
    /// Monthly spending cap in minor units (e.g. cents for USD).
    pub monthly_limit: Option<f64>,
    /// Credits consumed in the current period in minor units.
    pub used_credits: Option<f64>,
    /// Percentage of `monthly_limit` consumed (0.0–100.0).
    pub utilization: Option<f64>,
    /// Currency code (e.g. `"USD"`) for the money fields.
    pub currency: Option<String>,
}

/// Failure modes for [`oauth_usage`]. Variants split fallback-eligible
/// states (`NoCredentials`, `Expired`, `Unauthorized`) from terminal
/// ones so callers can decide whether to retry against a different
/// auth source.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
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
    /// API returned an unexpected non-success status.
    #[error("Claude OAuth usage request failed with HTTP {0}{1}")]
    HttpStatus(u16, String),
    /// Network error reaching the API.
    #[error("Claude OAuth network error: {0}")]
    Network(String),
    /// Response body could not be parsed.
    #[error("Failed to decode Claude OAuth usage response: {0}")]
    Decode(String),
}

impl OauthUsageError {
    /// True for transient/auth-related failures where falling back to
    /// a different usage source (e.g. the CLI fetcher) makes sense.
    /// `Network` and `HttpStatus` are excluded because they typically
    /// indicate the API is unreachable / broken — falling back to a
    /// different source for the same backend won't help.
    #[must_use]
    pub fn should_fallback(&self) -> bool {
        matches!(self, Self::NoCredentials | Self::Expired | Self::Unauthorized(_))
    }
}

/// Fetch the live OAuth usage payload from the Anthropic API using
/// the bearer in `<config_dir>/.credentials.json` (or, on macOS, the
/// matching keychain entry — see
/// [`super::oauth_credentials::load_oauth_credentials`] for the
/// resolution order).
///
/// # Errors
///
/// Returns [`OauthUsageError`] when credentials are missing/expired,
/// the HTTPS request fails, or the response can't be decoded.
pub async fn oauth_usage() -> Result<OauthUsage, OauthUsageError> {
    let credentials = load_oauth_credentials().ok_or(OauthUsageError::NoCredentials)?;

    if credentials.expires_at.is_some_and(|expires_at| expires_at <= std::time::SystemTime::now()) {
        return Err(OauthUsageError::Expired);
    }

    let headers = oauth_headers(&credentials.access_token)?;
    let client = reqwest::Client::builder()
        .timeout(OAUTH_TIMEOUT)
        .default_headers(headers)
        .build()
        .map_err(|error| OauthUsageError::Network(format!("client build: {error}")))?;

    let response = client
        .get(OAUTH_USAGE_URL)
        .send()
        .await
        .map_err(|error| OauthUsageError::Network(error.to_string()))?;

    let status = response.status().as_u16();
    let body = response
        .bytes()
        .await
        .map_err(|error| OauthUsageError::Network(format!("body read: {error}")))?;

    match status {
        200 => serde_json::from_slice::<OauthUsage>(&body)
            .map_err(|error| OauthUsageError::Decode(error.to_string())),
        401 | 403 => Err(OauthUsageError::Unauthorized(status)),
        _ => Err(OauthUsageError::HttpStatus(status, truncated_body_suffix(&body))),
    }
}

fn oauth_headers(access_token: &str) -> Result<HeaderMap, OauthUsageError> {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert("anthropic-beta", HeaderValue::from_static(OAUTH_BETA_HEADER));
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static(concat!("forge-sdk/", env!("CARGO_PKG_VERSION"))),
    );
    let bearer = HeaderValue::from_str(&format!("Bearer {access_token}"))
        .map_err(|error| OauthUsageError::Network(format!("bad bearer header: {error}")))?;
    headers.insert(AUTHORIZATION, bearer);
    Ok(headers)
}

fn truncated_body_suffix(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body).trim().replace('\n', " ");
    if text.is_empty() {
        return String::new();
    }
    let shortened = if text.chars().count() > 200 {
        let mut out = text.chars().take(200).collect::<String>();
        out.push_str("...");
        out
    } else {
        text
    };
    format!(": {shortened}")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn decodes_sparse_oauth_payload() {
        let usage: OauthUsage = serde_json::from_slice(
            br#"{
                "five_hour": { "utilization": 12.5, "resets_at": "2025-12-25T12:00:00.000Z" },
                "seven_day_sonnet": { "utilization": 5 },
                "unknown_field": true
            }"#,
        )
        .expect("decode");
        assert_eq!(usage.five_hour.as_ref().and_then(|w| w.utilization), Some(12.5));
        assert_eq!(usage.seven_day_sonnet.as_ref().and_then(|w| w.utilization), Some(5.0));
        assert!(usage.seven_day.is_none());
    }

    #[test]
    fn decodes_extra_usage_in_minor_units() {
        let usage: OauthUsage = serde_json::from_slice(
            br#"{
                "five_hour": { "utilization": 1, "resets_at": "2025-12-25T12:00:00.000Z" },
                "extra_usage": {
                    "is_enabled": true,
                    "monthly_limit": 2000,
                    "used_credits": 1240,
                    "utilization": 62,
                    "currency": "USD"
                }
            }"#,
        )
        .expect("decode");
        let extra = usage.extra_usage.expect("extra usage");
        assert_eq!(extra.monthly_limit, Some(2000.0));
        assert_eq!(extra.used_credits, Some(1240.0));
        assert_eq!(extra.utilization, Some(62.0));
        assert_eq!(extra.currency.as_deref(), Some("USD"));
    }

    #[test]
    fn should_fallback_only_for_auth_failures() {
        assert!(OauthUsageError::NoCredentials.should_fallback());
        assert!(OauthUsageError::Expired.should_fallback());
        assert!(OauthUsageError::Unauthorized(401).should_fallback());
        assert!(!OauthUsageError::Network("dns".to_owned()).should_fallback());
        assert!(!OauthUsageError::HttpStatus(500, String::new()).should_fallback());
        assert!(!OauthUsageError::Decode("bad".to_owned()).should_fallback());
    }
}
