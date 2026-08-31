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

use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};

pub use forge_primitives::account::Provider;
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
    // read theirs via `get` below - value is identical (same probe
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

    let first = probe(&credentials, None).await;
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
                Ok(new_creds) => probe(&new_creds, None).await,
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

/// The `/api/oauth/usage` endpoint URL. Defaults to the hardcoded
/// Anthropic host; a `base_url` override (an account's
/// `ANTHROPIC_BASE_URL`) redirects the probe to an alternate endpoint
/// serving the same `OauthUsage` shape. Any trailing slash on the
/// override is trimmed so `http://host/` and `http://host` behave
/// identically.
fn usage_url(base_url: Option<&str>) -> String {
    match base_url {
        Some(base) => format!("{}/api/oauth/usage", base.trim_end_matches('/')),
        None => OAUTH_USAGE_URL.to_owned(),
    }
}

/// How an account's usage should be probed, derived once from its
/// declared [`Provider`]. The loader and poller both read this single
/// decision so the probe source AND the response-mapping strictness
/// stay in lockstep.
///
/// Deliberately not [`Provider`] itself: the `BaseUrl` variant carries
/// a bearer, so this type must not cross into a view the TUI renders.
/// `forge_workspace::AccountAuth` is the secret-free counterpart.
#[derive(Debug, PartialEq, Eq)]
pub enum ProbePlan {
    /// Base-url provider: probe `{base_url}/api/oauth/usage`
    /// with the env `ANTHROPIC_AUTH_TOKEN` bearer (the macOS keychain is
    /// skipped - a base-url account has no keychain entry), and map the
    /// response leniently via [`super::oauth::snapshot_from_payload_lenient`]
    /// (each window independently optional). A base-url auth failure
    /// must NOT trigger the keychain CLI-spawn refresh: the probe never
    /// reads that token, so refreshing it burns billed `claude -p hi`
    /// spawns to no effect.
    BaseUrl { base_url: String, bearer: String },
    /// Normal Anthropic account: default host + macOS keychain bearer,
    /// strict mapping (a 200 must carry the five-hour window), and the
    /// CLI-spawn auth-recovery refresh on a 401.
    Keychain,
    /// OpenRouter: probe `{base_url}/v1/key` with the env
    /// `ANTHROPIC_AUTH_TOKEN` bearer and map the per-key spend. No
    /// windows, so nothing about this plan can go through the
    /// window-shaped mappers.
    OpenRouterKey { base_url: String, bearer: String },
}

/// Derive the [`ProbePlan`] for an account from its declared
/// [`Provider`]. The provider alone decides the shape; `env` is read
/// only to fill in the base url and bearer a base-url provider
/// authenticates with. `ANTHROPIC_BASE_URL` decides nothing, because it
/// answers where the credential lives rather than what the backend
/// bills for - Codex sets one and is still a windowed subscription.
///
/// Config load rejects a base-url provider with no `ANTHROPIC_BASE_URL`
/// (`WorkspaceError::AccountProviderNeedsBaseUrl`), so the empty-base
/// case here is unreachable in production and falls back to the
/// keychain rather than probing a malformed url.
pub fn probe_plan<S: std::hash::BuildHasher>(
    provider: Provider,
    env: &HashMap<String, String, S>,
) -> ProbePlan {
    if !provider.uses_base_url() {
        return ProbePlan::Keychain;
    }
    let Some(base_url) =
        env.get("ANTHROPIC_BASE_URL").map(|value| value.trim()).filter(|value| !value.is_empty())
    else {
        return ProbePlan::Keychain;
    };
    let bearer = env.get("ANTHROPIC_AUTH_TOKEN").map(String::as_str).unwrap_or_default();
    let (base_url, bearer) = (base_url.to_owned(), bearer.to_owned());
    match provider {
        Provider::Openrouter => ProbePlan::OpenRouterKey { base_url, bearer },
        Provider::Anthropic | Provider::Codex => ProbePlan::BaseUrl { base_url, bearer },
    }
}

/// OpenRouter's per-key endpoint. The documented path is `/api/v1/key`
/// relative to the site root, but `ANTHROPIC_BASE_URL` already ends in
/// `/api` because that is what the chat API wants, so only the `/v1/key`
/// tail is appended. Measured: appending the documented path to that
/// base yields `/api/api/v1/key`, which 404s.
fn key_url(base_url: &str) -> String {
    format!("{}/v1/key", base_url.trim_end_matches('/'))
}

/// One round-trip against `{base_url}/v1/key` for a pay-per-token
/// account. Shares [`OauthUsageError`] with the window probe so the
/// loader and poller classify a 401 / 429 / network failure the same
/// way regardless of billing kind.
pub async fn probe_openrouter_key(
    base_url: &str,
    bearer: &str,
) -> Result<forge_primitives::usage::openrouter::KeyResponse, OauthUsageError> {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    let auth = HeaderValue::from_str(&format!("Bearer {bearer}"))
        .map_err(|error| OauthUsageError::Network(format!("bad bearer header: {error}")))?;
    headers.insert(AUTHORIZATION, auth);

    let client = crate::http_trust::with_extra_roots(
        reqwest::Client::builder().timeout(OAUTH_TIMEOUT).default_headers(headers),
    )
    .build()
    .map_err(|error| OauthUsageError::Network(format!("client build: {error}")))?;

    let response = client
        .get(key_url(base_url))
        .send()
        .await
        .map_err(|error| OauthUsageError::Network(error.to_string()))?;

    let status = response.status().as_u16();
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

    // Body is logged only on a non-200, and only as a truncated
    // suffix. A 200 here carries a truncated copy of the key itself.
    if status == 200 {
        tracing::trace!(
            target: "forge_agent::cloud::oauth_usage",
            event_name = "openrouter_key_response",
            status,
            outcome = "ok",
            body_bytes = body.len(),
        );
    } else {
        tracing::warn!(
            target: "forge_agent::cloud::oauth_usage",
            event_name = "openrouter_key_response",
            status,
            outcome = "non_ok",
            retry_after_secs = ?retry_after.map(|d| d.as_secs()),
            body_suffix = %truncated_body_suffix(&body),
        );
    }

    match status {
        200 => serde_json::from_slice(&body)
            .map_err(|error| OauthUsageError::Decode(error.to_string())),
        401 | 403 => Err(OauthUsageError::Unauthorized(status)),
        429 => Err(OauthUsageError::RateLimited { retry_after }),
        _ => Err(OauthUsageError::HttpStatus(status, truncated_body_suffix(&body))),
    }
}

/// One round-trip against `/api/oauth/usage` using `credentials.access_token`.
///
/// `base_url` overrides the default Anthropic host when `Some` (an
/// account carrying an `ANTHROPIC_BASE_URL` env override polls its own
/// endpoint); the `/api/oauth/usage` path is always appended.
///
/// Exposed as a separate entry point from [`oauth_usage`] so the
/// boot-time per-account loading task in
/// `forge_workspace::account_loader` can drive its own refresh logic
/// (the loading state machine wants the raw probe result to branch
/// on `auth_status` rather than going through `oauth_usage`'s
/// internal auto-refresh). Other callers should still prefer
/// `oauth_usage` for the auto-refresh convenience.
pub async fn probe(
    credentials: &OauthCredentials,
    base_url: Option<&str>,
) -> Result<OauthUsage, OauthUsageError> {
    let headers = oauth_headers(&credentials.access_token).await?;
    let client = crate::http_trust::with_extra_roots(
        reqwest::Client::builder().timeout(OAUTH_TIMEOUT).default_headers(headers),
    )
    .build()
    .map_err(|error| OauthUsageError::Network(format!("client build: {error}")))?;

    let response = client
        .get(usage_url(base_url))
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
    fn usage_url_defaults_to_anthropic_host() {
        assert_eq!(usage_url(None), OAUTH_USAGE_URL);
    }

    #[test]
    fn usage_url_uses_base_url_override_and_trims_trailing_slash() {
        assert_eq!(
            usage_url(Some("http://localhost:18765")),
            "http://localhost:18765/api/oauth/usage",
        );
        assert_eq!(
            usage_url(Some("http://localhost:18765/")),
            "http://localhost:18765/api/oauth/usage",
            "trailing slash trimmed so host and host/ behave identically",
        );
    }

    /// OpenRouter documents the endpoint as `/api/v1/key` relative to
    /// the site root, but the configured base url already ends in
    /// `/api`, so the documented path appended to it 404s. Measured:
    /// `https://openrouter.ai/api/api/v1/key` is 404 and
    /// `https://openrouter.ai/api/v1/key` is 401, i.e. the endpoint
    /// exists and only auth is missing.
    #[test]
    fn key_url_joins_one_v1_segment_onto_the_configured_base() {
        assert_eq!(key_url("https://openrouter.ai/api"), "https://openrouter.ai/api/v1/key");
        assert_eq!(
            key_url("https://openrouter.ai/api/"),
            "https://openrouter.ai/api/v1/key",
            "trailing slash trimmed so base and base/ behave identically",
        );
        assert!(
            !key_url("https://openrouter.ai/api").contains("/api/api/"),
            "the documented /api/v1/key path must not be appended to a base that ends in /api",
        );
    }

    #[test]
    fn probe_plan_openrouter_probes_its_own_key_endpoint() {
        let mut env = HashMap::new();
        env.insert("ANTHROPIC_BASE_URL".to_owned(), "https://openrouter.ai/api".to_owned());
        env.insert("ANTHROPIC_AUTH_TOKEN".to_owned(), "sk-or-test".to_owned());
        assert_eq!(
            probe_plan(Provider::Openrouter, &env),
            ProbePlan::OpenRouterKey {
                base_url: "https://openrouter.ai/api".to_owned(),
                bearer: "sk-or-test".to_owned(),
            },
            "openrouter must not share Codex's windowed plan",
        );
    }

    #[test]
    fn probe_plan_codex_reads_base_and_token_from_env() {
        let mut env = HashMap::new();
        env.insert("ANTHROPIC_BASE_URL".to_owned(), "http://localhost:18765".to_owned());
        env.insert("ANTHROPIC_AUTH_TOKEN".to_owned(), "sk-codex".to_owned());
        assert_eq!(
            probe_plan(Provider::Codex, &env),
            ProbePlan::BaseUrl {
                base_url: "http://localhost:18765".to_owned(),
                bearer: "sk-codex".to_owned(),
            },
        );
    }

    /// The defect the `provider` key exists to remove: the plan used to
    /// key on `ANTHROPIC_BASE_URL`, which answers where the credential
    /// lives rather than what the backend bills for. Same env, two
    /// providers, two plans - so a base url can never again decide it.
    #[test]
    fn probe_plan_keys_on_provider_not_on_base_url() {
        let mut env = HashMap::new();
        env.insert("ANTHROPIC_BASE_URL".to_owned(), "http://localhost:18765".to_owned());
        env.insert("ANTHROPIC_AUTH_TOKEN".to_owned(), "sk-codex".to_owned());
        assert_eq!(
            probe_plan(Provider::Anthropic, &env),
            ProbePlan::Keychain,
            "an Anthropic account keeps the keychain even with a base url set",
        );
        assert!(
            matches!(probe_plan(Provider::Codex, &env), ProbePlan::BaseUrl { .. }),
            "the same env under Codex probes the base url",
        );
    }

    #[test]
    fn probe_plan_anthropic_is_keychain() {
        let mut env = HashMap::new();
        env.insert("ANTHROPIC_AUTH_TOKEN".to_owned(), "sk-anything".to_owned());
        assert_eq!(probe_plan(Provider::Anthropic, &env), ProbePlan::Keychain);
        assert_eq!(probe_plan(Provider::Anthropic, &HashMap::new()), ProbePlan::Keychain);
    }

    #[test]
    fn probe_plan_codex_missing_token_defaults_to_empty_bearer() {
        // A proxy on localhost ignores the bearer; an absent
        // ANTHROPIC_AUTH_TOKEN must not suppress the base-url probe.
        let mut env = HashMap::new();
        env.insert("ANTHROPIC_BASE_URL".to_owned(), "http://localhost:18765".to_owned());
        assert_eq!(
            probe_plan(Provider::Codex, &env),
            ProbePlan::BaseUrl {
                base_url: "http://localhost:18765".to_owned(),
                bearer: String::new(),
            },
        );
    }

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
        // HTTP-date in the past - duration_since(now) returns Err → None.
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
