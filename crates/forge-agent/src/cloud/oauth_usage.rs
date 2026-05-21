//! Anthropic OAuth usage API client.
//!
//! Fetches per-account rate-limit utilisation from
//! `https://api.anthropic.com/api/oauth/usage` using the OAuth
//! bearer credentials resolved by
//! [`super::oauth_credentials::load_oauth_credentials`] (file or —
//! on macOS — keychain). The `Authorization` header never escapes
//! this module.
//!
//! The response shape mirrors the live API as of 2026-04, exposed as
//! plain optional fields. Timestamp parsing is left to consumers
//! because the field is documented inconsistently (sometimes ISO-8601,
//! sometimes a numeric epoch).

use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};

pub use forge_primitives::usage::oauth::{
    OauthExtraUsage, OauthUsage, OauthUsageError, OauthUsageWindow,
};

use super::oauth_credentials::load_oauth_credentials;

const OAUTH_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const OAUTH_BETA_HEADER: &str = "oauth-2025-04-20";
const OAUTH_TIMEOUT: Duration = Duration::from_secs(8);

/// Canonical `User-Agent` Anthropic expects on /api/oauth/usage —
/// matches what the spawned `claude` subprocess sends on /v1/messages
/// after the wire-rewriter normalises it. Non-canonical UAs
/// (e.g. `forge-sdk/X.Y.Z`) get 429'd aggressively by Anthropic's
/// per-IP rate limiter, even on the first probe of an idle account.
/// Format: `claude-cli/<version> (external, cli)`. Version is probed
/// once via `claude --version` and cached. If the probe ever fails
/// (claude not on PATH, exec error) we propagate that as a probe
/// error so the caller's backoff path engages — a stale pinned
/// fallback would actively lie to Anthropic about which version is
/// running and drift over time, defeating the point of matching
/// reality on the wire.
fn canonical_user_agent() -> Result<&'static str, OauthUsageError> {
    static UA: OnceLock<String> = OnceLock::new();
    if let Some(cached) = UA.get() {
        return Ok(cached);
    }
    let version = forge_sdk::transport::process::query_cli_version("claude").map_err(|e| {
        OauthUsageError::Network(format!("claude --version probe failed for UA: {e}"))
    })?;
    let ua = format!("claude-cli/{version} (external, cli)");
    // get_or_init isn't `Result`-friendly. set/get pair: if another
    // caller raced us and set first, our `set` errors out and we
    // read theirs via `get` below — value is identical (same probe
    // result for the same machine) so the race is benign.
    let _ = UA.set(ua);
    UA.get().map(String::as_str).ok_or_else(|| {
        OauthUsageError::Network("UA cache disappeared after set; impossible".to_owned())
    })
}

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
    let client = crate::http_trust::with_extra_roots(
        reqwest::Client::builder().timeout(OAUTH_TIMEOUT).default_headers(headers),
    )
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

    // Diagnostic tracing for the "Anthropic 429s us on the first
    // probe" suspicion. Log status + a body suffix for every
    // non-200 response so a triage can correlate "which account /
    // when / what did the API actually say." Successful 200s are
    // logged at trace level (high volume — 60 s poll × N accounts)
    // with no body. config_dir is logged at the caller (workspace
    // poll loop) so we don't repeat it here.
    if status == 200 {
        tracing::trace!(
            target: "forge_agent::cloud::oauth_usage",
            event_name = "oauth_usage_response",
            status,
            outcome = "ok",
            body_bytes = body.len(),
        );
    } else {
        tracing::warn!(
            target: "forge_agent::cloud::oauth_usage",
            event_name = "oauth_usage_response",
            status,
            outcome = "non_ok",
            retry_after_secs = ?retry_after.map(|d| d.as_secs()),
            body_suffix = %truncated_body_suffix(&body),
        );
    }

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
    let ua = HeaderValue::from_str(canonical_user_agent()?)
        .map_err(|error| OauthUsageError::Network(format!("bad UA header: {error}")))?;
    headers.insert(USER_AGENT, ua);
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
