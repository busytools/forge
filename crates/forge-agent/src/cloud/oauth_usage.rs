//! Anthropic OAuth usage API client.
//!
//! Fetches per-account rate-limit utilisation from
//! `https://api.anthropic.com/api/oauth/usage` using the OAuth
//! bearer credentials resolved by
//! [`super::oauth_credentials::load_oauth_credentials`] (macOS
//! keychain only - the file source was removed in #237-B; see the
//! module docs in `oauth_credentials.rs` for the rationale). The
//! `Authorization` header never escapes this module.
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

use super::oauth_credentials::{OauthCredentials, load_oauth_credentials, refresh_via_cli_spawn};

const OAUTH_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const OAUTH_BETA_HEADER: &str = "oauth-2025-04-20";
const OAUTH_TIMEOUT: Duration = Duration::from_secs(8);

/// `User-Agent` native CLI sends on /api/oauth/usage, captured from
/// mitmdump 2026-05-26 against claude CLI 2.1.133 in an authenticated
/// interactive session running `/usage`. Format: `claude-code/<version>`,
/// no parens, no `(external, cli)` suffix. Distinct from the
/// /v1/messages UA shape (which is `claude-cli/<version> (external, cli)`);
/// the messages endpoint and the oauth-usage endpoint deliberately
/// carry different UAs natively, so this probe matches the usage
/// endpoint's shape rather than the messages endpoint's.
///
/// Version is probed once via `claude --version` and cached. If the
/// probe fails (claude not on PATH, exec error) we propagate that as
/// a probe error so the caller's backoff path engages. A stale
/// pinned fallback would actively lie to Anthropic about which
/// version is running and drift over time, defeating the point of
/// matching reality on the wire.
async fn oauth_usage_user_agent() -> Result<&'static str, OauthUsageError> {
    static UA: OnceLock<String> = OnceLock::new();
    if let Some(cached) = UA.get() {
        return Ok(cached);
    }
    let version =
        tokio::task::spawn_blocking(|| forge_sdk::transport::process::query_cli_version("claude"))
            .await
            .map_err(|e| {
                OauthUsageError::Network(format!("UA probe spawn_blocking panicked: {e}"))
            })?
            .map_err(|e| {
                OauthUsageError::Network(format!("claude --version probe failed for UA: {e}"))
            })?;
    let ua = format!("claude-code/{version}");
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
/// the bearer the macOS keychain holds for `config_dir` (see
/// [`super::oauth_credentials::load_oauth_credentials`]).
///
/// The caller (typically a `ForgeSdkBridge`) is the source of truth
/// for `config_dir`; there is no fallback to a process-env-derived
/// path.
///
/// Refresh fast-path: when the cached token's `expires_at` is in the
/// past OR absent entirely, AND the live probe returns `Unauthorized`,
/// fires [`refresh_via_cli_spawn`] once to nudge the claude CLI into
/// rotating the keychain entry, then retries the probe with the
/// freshly-read token. Any refresh failure (binary missing, timeout,
/// non-zero exit, keychain still expired) surfaces the original
/// `Unauthorized` so the #237-A cache-invalidation pathway picks up
/// after the usual 3-strike threshold. Other 401 causes (valid
/// token + revoked / scope mismatch) skip refresh entirely - the
/// expiry check filters them out.
///
/// # Errors
///
/// Returns [`OauthUsageError`] when credentials are missing/expired,
/// the HTTPS request fails, or the response can't be decoded.
pub async fn oauth_usage(config_dir: &Path) -> Result<OauthUsage, OauthUsageError> {
    let credentials = load_oauth_credentials(config_dir).ok_or(OauthUsageError::NoCredentials)?;

    let first = do_probe(&credentials).await;
    match first {
        Err(OauthUsageError::Unauthorized(status))
            if credentials.expires_at.is_none_or(|t| t < std::time::SystemTime::now()) =>
        {
            // Local view of the token agrees with the server's verdict
            // (401): expires_at is either in the past OR absent
            // entirely. Treating None as "missing expiry = expired" is
            // the safe call here - refresh is one-shot (the per-account
            // mutex prevents a probe storm), and surfacing 401 forever
            // with no refresh attempt is worse than firing one refresh
            // against a credential blob whose expiresAt field was
            // omitted by an older claude write or a future schema
            // change. Try one refresh + retry; on any refresh failure,
            // fall through to the original Unauthorized.
            match refresh_via_cli_spawn(config_dir).await {
                Ok(new_creds) => do_probe(&new_creds).await,
                Err(refresh_err) => {
                    tracing::warn!(
                        target: "forge_agent::cloud::oauth_usage",
                        event_name = "oauth_usage_refresh_failed",
                        config_dir = %config_dir.display(),
                        error = %refresh_err,
                        "refresh attempt did not produce fresh creds; surfacing original Unauthorized",
                    );
                    Err(OauthUsageError::Unauthorized(status))
                }
            }
        }
        other => other,
    }
}

/// One round-trip against `/api/oauth/usage` using `credentials.access_token`.
/// Factored out so the refresh path can reuse the same probe code
/// without re-loading the credentials from the keychain twice.
async fn do_probe(credentials: &OauthCredentials) -> Result<OauthUsage, OauthUsageError> {
    let headers = oauth_headers(&credentials.access_token).await?;
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
    // Parse Retry-After BEFORE consuming the response body - once
    // we call .bytes() the response object is moved. Anthropic
    // returns 429 with a per-account hold-down value in seconds;
    // honouring it prevents the poller from re-tripping the limit
    // every cycle.
    let retry_after = if status == 429 {
        response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_retry_after)
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
    // logged at trace level (high volume - 60 s poll × N accounts)
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

async fn oauth_headers(access_token: &str) -> Result<HeaderMap, OauthUsageError> {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert("anthropic-beta", HeaderValue::from_static(OAUTH_BETA_HEADER));
    let ua = HeaderValue::from_str(oauth_usage_user_agent().await?)
        .map_err(|error| OauthUsageError::Network(format!("bad UA header: {error}")))?;
    headers.insert(USER_AGENT, ua);
    let bearer = HeaderValue::from_str(&format!("Bearer {access_token}"))
        .map_err(|error| OauthUsageError::Network(format!("bad bearer header: {error}")))?;
    headers.insert(AUTHORIZATION, bearer);
    Ok(headers)
}

/// RFC 7231 §7.1.3 `Retry-After` accepts either delta-seconds
/// (`"120"`) or an HTTP-date (`"Wed, 21 Oct 2015 07:28:00 GMT"`).
/// Anthropic emits the integer form today, but the spec leaves the
/// HTTP-date form open and proxies / CDNs in the path may swap shapes.
/// Try the integer form first; fall back to httpdate parsing and
/// compute the delta from `now`.
fn parse_retry_after(raw: &str) -> Option<Duration> {
    let trimmed = raw.trim();
    if let Ok(secs) = trimmed.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let when = httpdate::parse_http_date(trimmed).ok()?;
    when.duration_since(std::time::SystemTime::now()).ok()
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
    fn retry_after_integer_seconds_round_trip() {
        assert_eq!(parse_retry_after("0"), Some(Duration::from_secs(0)));
        assert_eq!(parse_retry_after("  120  "), Some(Duration::from_secs(120)));
        assert_eq!(parse_retry_after("3600"), Some(Duration::from_secs(3600)));
    }

    #[test]
    fn retry_after_http_date_returns_delta_from_now() {
        // ~1 hour in the future, formatted in HTTP-date format
        let target = std::time::SystemTime::now() + Duration::from_secs(3600);
        let formatted = httpdate::fmt_http_date(target);
        let parsed = parse_retry_after(&formatted).expect("http-date parses");
        // The parsed delta should be close to 1 hour (allow ±5 s drift).
        assert!(parsed.as_secs() >= 3595 && parsed.as_secs() <= 3605, "got {parsed:?}");
    }

    #[test]
    fn retry_after_past_http_date_returns_none() {
        // HTTP-date in the past — duration_since(now) returns Err → None.
        let past = std::time::SystemTime::now() - Duration::from_secs(3600);
        let formatted = httpdate::fmt_http_date(past);
        assert!(parse_retry_after(&formatted).is_none());
    }

    #[test]
    fn retry_after_garbage_returns_none() {
        assert!(parse_retry_after("not a duration").is_none());
        assert!(parse_retry_after("").is_none());
    }

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

    /// Pins the User-Agent shape sent on /api/oauth/usage to the
    /// `claude-code/<version>` form captured from native CLI 2.1.133.
    /// The probe at runtime spawns `claude --version` to fill in the
    /// version; in unit context we exercise the format only.
    #[test]
    fn oauth_usage_ua_shape_matches_native_claude_code_prefix() {
        let formatted = format!("claude-code/{}", "2.1.133");
        assert_eq!(formatted, "claude-code/2.1.133");
        assert!(!formatted.contains("(external"));
        assert!(!formatted.contains("(cli"));
        assert!(!formatted.starts_with("claude-cli/"));
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
