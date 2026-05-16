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

use std::path::Path;
use std::time::Duration;

use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};

pub use forge_primitives::usage::oauth::{
    OauthExtraUsage, OauthUsage, OauthUsageError, OauthUsageWindow,
};

use super::oauth_credentials::load_oauth_credentials;

const OAUTH_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const OAUTH_BETA_HEADER: &str = "oauth-2025-04-20";
const OAUTH_TIMEOUT: Duration = Duration::from_secs(8);

/// Fetch the live OAuth usage payload from the Anthropic API using
/// the bearer in `<config_dir>/.credentials.json` (or, on macOS, the
/// matching keychain entry — see
/// [`super::oauth_credentials::load_oauth_credentials`] for the
/// resolution order).
///
/// The caller (typically a `ForgeSdkBridge`) is the source of truth
/// for `config_dir`; there is no fallback to a process-env-derived
/// path.
///
/// # Errors
///
/// Returns [`OauthUsageError`] when credentials are missing/expired,
/// the HTTPS request fails, or the response can't be decoded.
pub async fn oauth_usage(config_dir: &Path) -> Result<OauthUsage, OauthUsageError> {
    let credentials = load_oauth_credentials(config_dir).ok_or(OauthUsageError::NoCredentials)?;

    // `credentials.expires_at` is a stale cache: the CLI refreshes
    // the access token silently before each of its own requests,
    // out-of-band from our probe. Trust the upstream API response
    // (401 → Unauthorized, anything else → live) instead of the
    // on-disk timestamp.
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
    // Parse Retry-After BEFORE consuming the response body — once
    // we call .bytes() the response object is moved. Anthropic
    // returns 429 with a per-account hold-down value in seconds;
    // honouring it prevents the poller from re-tripping the limit
    // every cycle.
    let retry_after = if status == 429 {
        response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(Duration::from_secs)
    } else {
        None
    };
    let body = response
        .bytes()
        .await
        .map_err(|error| OauthUsageError::Network(format!("body read: {error}")))?;

    match status {
        200 => serde_json::from_slice::<OauthUsage>(&body)
            .map_err(|error| OauthUsageError::Decode(error.to_string())),
        401 | 403 => Err(OauthUsageError::Unauthorized(status)),
        429 => Err(OauthUsageError::RateLimited { retry_after }),
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
