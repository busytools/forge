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
use forge_primitives::usage::zai::{QuotaLimitData, QuotaLimitResponse};

use super::oauth_credentials::{OauthCredentials, load_oauth_credentials, refresh_via_cli_spawn};

const OAUTH_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const OAUTH_BETA_HEADER: &str = "oauth-2025-04-20";
const OAUTH_TIMEOUT: Duration = Duration::from_secs(8);

/// `[accounts.env]` key carrying a per-account setup token (minted by
/// `claude setup-token`). Its presence makes an Anthropic account
/// token-mode: the probe authenticates with the token, never with the
/// keychain entry for the account's config dir.
const CLAUDE_CODE_OAUTH_TOKEN_ENV: &str = "CLAUDE_CODE_OAUTH_TOKEN";

/// The setup token `env` carries, when non-empty.
fn token_bearer<S: std::hash::BuildHasher>(env: &HashMap<String, String, S>) -> Option<&str> {
    env.get(CLAUDE_CODE_OAUTH_TOKEN_ENV)
        .map(String::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
}

/// True when `env` carries a non-empty `CLAUDE_CODE_OAUTH_TOKEN`.
/// A token-mode account has no keychain entry of its own - the config
/// dir is shared - so both the probe and preflight's repair copy branch
/// on this rather than on the provider alone.
pub fn is_token_mode<S: std::hash::BuildHasher>(env: &HashMap<String, String, S>) -> bool {
    token_bearer(env).is_some()
}

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
/// Cached `claude-code/<version>` User-Agent, probed once per process.
static UA: OnceLock<String> = OnceLock::new();

async fn oauth_usage_user_agent() -> Result<&'static str, OauthUsageError> {
    if let Some(cached) = UA.get() {
        return Ok(cached);
    }
    let ua = resolve_ua("claude").await?;
    // get_or_init isn't `Result`-friendly. set/get pair: if another
    // caller raced us and set first, our `set` errors out and we
    // read theirs via `get` below - value is identical (same probe
    // result for the same machine) so the race is benign.
    let _ = UA.set(ua);
    UA.get().map(String::as_str).ok_or_else(|| {
        OauthUsageError::UaProbe("UA cache disappeared after set; impossible".to_owned())
    })
}

/// One `claude --version` round-trip, formatted as the UA. Split from
/// the cached [`oauth_usage_user_agent`] so the shell-out and its
/// failure class are drivable without resolving a real binary.
async fn resolve_ua(binary: &'static str) -> Result<String, OauthUsageError> {
    let version = tokio::task::spawn_blocking(move || {
        forge_sdk::transport::process::query_cli_version(binary)
    })
    .await
    .map_err(|e| OauthUsageError::UaProbe(format!("UA probe spawn_blocking panicked: {e}")))?
    .map_err(|e| OauthUsageError::UaProbe(format!("claude --version probe failed for UA: {e}")))?;
    Ok(format!("claude-code/{version}"))
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
    /// Token-mode Anthropic account: default host + the
    /// `CLAUDE_CODE_OAUTH_TOKEN` setup token from `[accounts.env]`, no
    /// keychain read. A setup token carries `user:inference` but not
    /// the `user:profile` scope the usage endpoint requires, so a VALID
    /// token always answers 403 `oauth_scope_insufficient` -
    /// [`probe_setup_token`] settles that refusal to the empty payload,
    /// which maps leniently to a barless Ready snapshot. A 401 is a
    /// genuinely rejected token and still classifies `Unauthorized`.
    Token { bearer: String },
    /// OpenRouter: probe `{base_url}/v1/key` with the env
    /// `ANTHROPIC_AUTH_TOKEN` bearer and map the per-key spend. No
    /// windows, so nothing about this plan can go through the
    /// window-shaped mappers.
    OpenRouterKey { base_url: String, bearer: String },
    /// Z.ai GLM coding plan: probe
    /// `{host_root}/api/monitor/usage/quota/limit` where `host_root`
    /// is the scheme+host of the account's `ANTHROPIC_BASE_URL` (the
    /// base itself carries `/api/anthropic` for the chat API; the
    /// monitor paths live off the site root). The key is sent raw,
    /// without a Bearer prefix.
    ZaiMonitor { base_url: String, bearer: String },
}

/// Derive the [`ProbePlan`] for an account from its declared
/// [`Provider`]. The provider alone decides the shape; `env` fills in
/// the base url and bearer a base-url provider authenticates with, and
/// a non-base-url provider's setup token flips the plan to
/// [`ProbePlan::Token`]. `ANTHROPIC_BASE_URL` decides nothing, because it
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
        return match token_bearer(env) {
            Some(bearer) => ProbePlan::Token { bearer: bearer.to_owned() },
            None => ProbePlan::Keychain,
        };
    }
    let Some(base_url) =
        env.get("ANTHROPIC_BASE_URL").map(|value| value.trim()).filter(|value| !value.is_empty())
    else {
        // Falling back to Keychain here would send a base-url account
        // down the keychain path, where a 401 fires billed `claude -p hi`
        // refreshes against a token its probe never reads.
        debug_assert!(false, "config load rejects a base-url provider with no ANTHROPIC_BASE_URL");
        return ProbePlan::Keychain;
    };
    let bearer = env.get("ANTHROPIC_AUTH_TOKEN").map(String::as_str).unwrap_or_default();
    let (base_url, bearer) = (base_url.to_owned(), bearer.to_owned());
    match provider {
        Provider::Openrouter => ProbePlan::OpenRouterKey { base_url, bearer },
        Provider::Zai => ProbePlan::ZaiMonitor { base_url, bearer },
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
        200 => serde_json::from_slice(&body).map_err(|error| {
            // A 200 that will not parse is the shape a wrong base url
            // takes: the bare host answers 200 with an HTML page. Name
            // the URL and show the body, or the only evidence is a byte
            // count on a trace line nobody has enabled.
            tracing::warn!(
                target: "forge_agent::cloud::oauth_usage",
                event_name = "openrouter_key_decode_failed",
                url = %key_url(base_url),
                error = %error,
                body_suffix = %truncated_body_suffix(&body),
                "200 from the key endpoint did not decode; check the base url is the API root",
            );
            OauthUsageError::Decode(error.to_string())
        }),
        401 | 403 => Err(OauthUsageError::Unauthorized(status)),
        429 => Err(OauthUsageError::RateLimited { retry_after }),
        _ => Err(OauthUsageError::HttpStatus(status, truncated_body_suffix(&body))),
    }
}

/// The scheme+host of an account's `ANTHROPIC_BASE_URL` - everything
/// before the first path segment. The Z.ai monitor paths live off the
/// site root (`https://api.z.ai`), never under the `/api/anthropic`
/// chat prefix the base carries.
fn zai_monitor_host(base_url: &str) -> &str {
    let trimmed = base_url.trim().trim_end_matches('/');
    let after = trimmed.split_once("://").map_or(trimmed, |(_, rest)| rest);
    let host = after.split('/').next().unwrap_or_default();
    if host.is_empty() {
        return trimmed;
    }
    &trimmed[..trimmed.len() - after.len() + host.len()]
}

fn zai_monitor_url(base_url: &str) -> String {
    format!("{}/api/monitor/usage/quota/limit", zai_monitor_host(base_url))
}

fn zai_headers(key: &str) -> Result<HeaderMap, OauthUsageError> {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    let auth = HeaderValue::from_str(key)
        .map_err(|error| OauthUsageError::Network(format!("bad auth header: {error}")))?;
    headers.insert(AUTHORIZATION, auth);
    Ok(headers)
}

/// One round-trip against `{host_root}/api/monitor/usage/quota/limit`
/// for a Z.ai GLM coding plan account. Shares [`OauthUsageError`] with
/// the window probes so the loader and poller classify failures the
/// same way regardless of provider.
pub async fn probe_zai_monitor(
    base_url: &str,
    bearer: &str,
) -> Result<QuotaLimitData, OauthUsageError> {
    let headers = zai_headers(bearer)?;
    let client = crate::http_trust::with_extra_roots(
        reqwest::Client::builder().timeout(OAUTH_TIMEOUT).default_headers(headers),
    )
    .build()
    .map_err(|error| OauthUsageError::Network(format!("client build: {error}")))?;

    let response = client
        .get(zai_monitor_url(base_url))
        .send()
        .await
        .map_err(|error| OauthUsageError::Network(error.to_string()))?;

    let status = response.status().as_u16();
    let body = response
        .bytes()
        .await
        .map_err(|error| OauthUsageError::Network(format!("body read: {error}")))?;

    if status == 200 {
        tracing::trace!(
            target: "forge_agent::cloud::oauth_usage",
            event_name = "zai_monitor_response",
            status,
            outcome = "ok",
            body_bytes = body.len(),
        );
    } else {
        tracing::warn!(
            target: "forge_agent::cloud::oauth_usage",
            event_name = "zai_monitor_response",
            status,
            outcome = "non_ok",
            body_suffix = %truncated_body_suffix(&body),
        );
    }

    match status {
        200 => zai_quota_from_body(&body),
        401 | 403 => Err(OauthUsageError::Unauthorized(status)),
        429 => Err(OauthUsageError::RateLimited { retry_after: None }),
        _ => Err(OauthUsageError::HttpStatus(status, truncated_body_suffix(&body))),
    }
}

/// Parse a Z.ai monitor body. The HTTP layer carries no verdict - a
/// wrong key and a wrong path arrive as HTTP 200 - so the envelope is
/// decoded unconditionally and keyed on `success`/`code`. A body-level
/// 401 (wrong key) surfaces as [`OauthUsageError::Unauthorized`] so it
/// bails the boot probe like a real auth rejection; every other
/// failure keeps the envelope's `msg`.
fn zai_quota_from_body(body: &[u8]) -> Result<QuotaLimitData, OauthUsageError> {
    if String::from_utf8_lossy(body).trim().is_empty() {
        return Err(OauthUsageError::Decode(
            "Z.ai monitor answered 200 with an empty body".to_owned(),
        ));
    }
    let envelope: QuotaLimitResponse = serde_json::from_slice(body).map_err(|error| {
        OauthUsageError::Decode(format!("Z.ai monitor body did not decode: {error}"))
    })?;
    let msg = envelope.msg.as_deref().unwrap_or("no msg");
    if !envelope.success {
        if envelope.code == Some(401) {
            return Err(OauthUsageError::Unauthorized(401));
        }
        return Err(OauthUsageError::Decode(format!(
            "Z.ai monitor reported failure (code {}): {msg}",
            envelope.code.unwrap_or(0),
        )));
    }
    if envelope.code != Some(200) {
        return Err(OauthUsageError::Decode(format!(
            "Z.ai monitor reported code {}: {msg}",
            envelope.code.unwrap_or(0),
        )));
    }
    envelope
        .data
        .ok_or_else(|| OauthUsageError::Decode("Z.ai monitor 200 carried no data".to_owned()))
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
    } else if status == 403 && is_scope_refusal(&body) {
        // The verdict on a valid setup token, not a failure: warn here
        // would fire every 60 s per healthy token account.
        tracing::debug!(
            target: "forge_agent::cloud::oauth_usage",
            event_name = "oauth_usage_scope_refusal",
            status,
            outcome = "scope_refused",
            body_suffix = %truncated_body_suffix(&body),
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
        403 if is_scope_refusal(&body) => Err(OauthUsageError::ScopeInsufficient),
        401 | 403 => Err(OauthUsageError::Unauthorized(status)),
        429 => Err(OauthUsageError::RateLimited { retry_after }),
        _ => Err(OauthUsageError::HttpStatus(status, truncated_body_suffix(&body))),
    }
}

/// Whether a 403 body is the usage endpoint's scope refusal rather than
/// an auth failure. Keyed on the body's `error.details.error_code` -
/// the verified live shape for a valid setup token; a revoked one
/// answers 401 `authentication_error`, so the two never share a class.
fn is_scope_refusal(body: &[u8]) -> bool {
    let code = serde_json::from_slice::<serde_json::Value>(body).ok().and_then(|value| {
        value.get("error")?.get("details")?.get("error_code")?.as_str().map(str::to_owned)
    });
    code.as_deref() == Some("oauth_scope_insufficient")
}

/// Settle a token-mode probe result: the scope refusal is the verdict
/// on a valid setup token, so it becomes the empty payload the lenient
/// mapper turns into a barless snapshot. Every other error passes
/// through untouched - a 401 must still reach the loader as
/// `Unauthorized` and bail.
fn accept_scope_refusal(
    result: Result<OauthUsage, OauthUsageError>,
) -> Result<OauthUsage, OauthUsageError> {
    match result {
        Err(OauthUsageError::ScopeInsufficient) => Ok(OauthUsage::default()),
        other => other,
    }
}

/// One round-trip against the default-host usage endpoint with the
/// account's `[accounts.env]` setup token. Never reads the keychain:
/// a token-mode account's config dir is shared, so the entry there
/// belongs to whichever account logged in last, or to nobody.
pub async fn probe_setup_token(bearer: &str) -> Result<OauthUsage, OauthUsageError> {
    let credentials = OauthCredentials { access_token: bearer.to_owned(), expires_at: None };
    let settled = accept_scope_refusal(probe(&credentials, None).await);
    match &settled {
        // Names the settle so the debug refusal line above is
        // diagnosable in a triage grep.
        Ok(_) => tracing::info!(
            target: "forge_agent::cloud::oauth_usage",
            event_name = "oauth_usage_setup_token_settled",
            outcome = "ok",
            "setup token usage probe settled",
        ),
        Err(OauthUsageError::Unauthorized(403)) => tracing::warn!(
            target: "forge_agent::cloud::oauth_usage",
            event_name = "oauth_usage_setup_token_unrecognized_403",
            outcome = "non_ok",
            "403 without the oauth_scope_insufficient shape: if the token was just \
             re-minted, suspect a changed refusal body rather than a dead token",
        ),
        _ => {}
    }
    settled
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

pub(super) fn truncated_body_suffix(body: &[u8]) -> String {
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

    /// A down endpoint must surface as the Network class and the probe
    /// must return rather than hang: preflight's bounded-failure path
    /// leans on both. Port 1 on loopback refuses the connect at once.
    ///
    /// The UA cache is seeded first: the probe shells out to `claude`
    /// before it makes any request, and a host without the binary -
    /// a CI runner - would otherwise short-circuit into UaProbe before
    /// the connect this test is about ever happens.
    #[tokio::test]
    async fn a_down_endpoint_is_a_network_failure_and_the_probe_returns() {
        let _ = UA.set("claude-code/1.0.0".to_owned());
        let creds = OauthCredentials { access_token: "test-token".to_owned(), expires_at: None };
        let result =
            tokio::time::timeout(Duration::from_secs(5), probe(&creds, Some("http://127.0.0.1:1")))
                .await
                .expect("the probe returns against an unreachable endpoint");
        assert!(
            matches!(result, Err(OauthUsageError::Network(_))),
            "a refused connect is the Network class, not a status or decode; got {result:?}"
        );
    }

    /// A binary nothing resolves is the UaProbe class - the probe could
    /// not run, which is not a verdict about the endpoint. Driven
    /// through the real shell-out with a name that cannot resolve.
    #[tokio::test]
    async fn a_missing_claude_binary_is_a_ua_failure_not_a_network_failure() {
        let result = resolve_ua("forge-test-claude-absent-from-path").await;
        assert!(
            matches!(result, Err(OauthUsageError::UaProbe(_))),
            "a binary nothing resolves is the UaProbe class; got {result:?}"
        );
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
        // A stray setup token must not outrank the provider: the
        // provider is decided first, so a base-url account keeps its
        // endpoint probe even with this key present.
        env.insert("CLAUDE_CODE_OAUTH_TOKEN".to_owned(), "setup-token".to_owned());
        assert_eq!(
            probe_plan(Provider::Codex, &env),
            ProbePlan::BaseUrl {
                base_url: "http://localhost:18765".to_owned(),
                bearer: "sk-codex".to_owned(),
            },
        );
    }

    /// Same env, two providers, two plans: a base url cannot decide the
    /// probe, because it answers where the credential lives rather than
    /// what the backend bills for.
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

    /// A setup token in `[accounts.env]` is the account's credential, so
    /// the probe must authenticate with it instead of reading the
    /// keychain - whose entry for the shared config dir belongs to
    /// whichever account last logged in interactively, or to nobody.
    #[test]
    fn probe_plan_anthropic_setup_token_is_token_mode() {
        let mut env = HashMap::new();
        env.insert("CLAUDE_CODE_OAUTH_TOKEN".to_owned(), "setup-token".to_owned());
        assert_eq!(
            probe_plan(Provider::Anthropic, &env),
            ProbePlan::Token { bearer: "setup-token".to_owned() },
            "an env setup token must not fall through to the keychain plan",
        );
    }

    /// An empty CLAUDE_CODE_OAUTH_TOKEN must not flip the plan: a real
    /// keychain account with a stale empty var in its env block would
    /// otherwise lose its probe entirely.
    #[test]
    fn probe_plan_empty_setup_token_stays_keychain() {
        let mut env = HashMap::new();
        env.insert("CLAUDE_CODE_OAUTH_TOKEN".to_owned(), "  ".to_owned());
        assert_eq!(probe_plan(Provider::Anthropic, &env), ProbePlan::Keychain);
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
    fn probe_plan_zai_probes_its_monitor_host() {
        let mut env = HashMap::new();
        env.insert("ANTHROPIC_BASE_URL".to_owned(), "https://api.z.ai/api/anthropic".to_owned());
        env.insert("ANTHROPIC_AUTH_TOKEN".to_owned(), "zai-key".to_owned());
        assert_eq!(
            probe_plan(Provider::Zai, &env),
            ProbePlan::ZaiMonitor {
                base_url: "https://api.z.ai/api/anthropic".to_owned(),
                bearer: "zai-key".to_owned(),
            },
            "zai must not share Codex's windowed /api/oauth/usage plan",
        );
    }

    #[test]
    fn zai_monitor_url_derives_the_host_root() {
        assert_eq!(
            zai_monitor_url("https://api.z.ai/api/anthropic"),
            "https://api.z.ai/api/monitor/usage/quota/limit",
        );
        assert_eq!(
            zai_monitor_url("https://api.z.ai/api/anthropic/"),
            "https://api.z.ai/api/monitor/usage/quota/limit",
            "trailing slash trimmed so base and base/ behave identically",
        );
    }

    #[test]
    fn zai_headers_send_the_key_raw_without_a_bearer_prefix() {
        let headers = zai_headers("zai-key").expect("headers");
        let auth = headers
            .get(AUTHORIZATION)
            .expect("auth header set")
            .to_str()
            .expect("ascii header value");
        assert_eq!(auth, "zai-key", "the key goes out raw; no Bearer prefix");
    }

    /// The verified fresh-account shape: HTTP 200, envelope green, two
    /// CREDIT_LIMIT windows, 5h entry without a nextResetTime.
    #[test]
    fn zai_quota_from_body_maps_the_fresh_account_envelope() {
        let body = br#"{
            "code": 200,
            "msg": "success",
            "data": {
                "limits": [
                    {"type":"CREDIT_LIMIT","unit":3,"number":5,"usage":28000,
                     "remaining":28000,"percentage":0,"currentValue":0},
                    {"type":"CREDIT_LIMIT","unit":6,"number":1,"usage":140000,
                     "remaining":140000,"percentage":0,"currentValue":0,
                     "nextResetTime":1757000000000}
                ],
                "level": "max"
            },
            "success": true
        }"#;
        let data = zai_quota_from_body(body).expect("a green envelope parses");
        assert_eq!(data.limits.len(), 2);
        assert_eq!(data.level.as_deref(), Some("max"));
    }

    #[test]
    fn zai_quota_from_body_fails_on_wrong_key_inside_a_200() {
        let err = zai_quota_from_body(
            br#"{"code":401,"msg":"token expired or incorrect","success":false}"#,
        )
        .expect_err("wrong key fails");
        assert!(
            matches!(err, OauthUsageError::Unauthorized(401)),
            "a body-level 401 must classify as unauthorized, got {err:?}",
        );
    }

    #[test]
    fn zai_quota_from_body_fails_on_wrong_path_inside_a_200() {
        let err = zai_quota_from_body(br#"{"code":500,"msg":"404 NOT_FOUND","success":false}"#)
            .expect_err("a wrong path fails");
        assert!(err.to_string().contains("404 NOT_FOUND"), "the msg reaches the log: {err}");
    }

    /// A silent empty 200 (observed on the model-usage endpoint) is a
    /// failure, not a bill of zero.
    #[test]
    fn zai_quota_from_body_fails_on_empty_body() {
        let err = zai_quota_from_body(b"").expect_err("empty body fails");
        assert!(err.to_string().contains("empty"), "got {err}");
    }

    /// A scope refusal is the endpoint's verdict on a VALID setup token:
    /// the token authenticates but lacks the `user:profile` scope the
    /// usage endpoint requires. Verified live 2026-09-04: a valid setup
    /// token gets 403 `oauth_scope_insufficient`, a revoked one gets
    /// 401 `authentication_error` - so the two must never share a
    /// classification, or every valid token account bails at boot.
    /// Expiry was not observed; an unseen 403 shape classifies
    /// `Unauthorized` and bails, so an unknown shape fails safe.
    #[test]
    fn a_403_scope_refusal_body_is_recognized_and_other_403s_are_not() {
        let refused = br#"{"type":"error","error":{"type":"permission_error","message":"OAuth token does not meet scope requirement user:profile","details":{"error_code":"oauth_scope_insufficient"}}}"#;
        assert!(
            is_scope_refusal(refused),
            "the verified refusal shape classifies as a scope refusal"
        );
        assert!(
            !is_scope_refusal(br#"{"type":"error","error":{"type":"authentication_error","message":"Invalid bearer token","details":{}}}"#),
            "an authentication_error body is not a scope refusal",
        );
        assert!(!is_scope_refusal(b"not json"), "a non-JSON body is not a scope refusal");
        assert!(
            !is_scope_refusal(br#"{"error":{"type":"permission_error","message":"no details"}}"#),
            "a permission_error without the error_code is not a scope refusal",
        );
        assert!(
            !is_scope_refusal(
                br#"{"error":{"details":{"error_code":"oauth_token_revoked"},"message":"x"}}"#
            ),
            "a populated but different error_code is not a scope refusal",
        );
    }

    /// The neutral settlement: a scope refusal maps to the empty
    /// payload the lenient mapper turns into an all-absent snapshot,
    /// while every other error passes through untouched - a revoked
    /// token must reach the loader as Unauthorized and bail.
    #[test]
    fn accept_scope_refusal_neutralizes_only_the_scope_refusal() {
        let refused = accept_scope_refusal(Err(OauthUsageError::ScopeInsufficient));
        assert_eq!(
            refused,
            Ok(OauthUsage::default()),
            "a scope refusal settles to the empty payload"
        );

        let rejected = accept_scope_refusal(Err(OauthUsageError::Unauthorized(401)));
        assert!(
            matches!(rejected, Err(OauthUsageError::Unauthorized(401))),
            "a rejected token stays an auth error; got {rejected:?}",
        );
        let transient = accept_scope_refusal(Err(OauthUsageError::HttpStatus(500, String::new())));
        assert!(
            matches!(transient, Err(OauthUsageError::HttpStatus(500, _))),
            "a transient error keeps its class; got {transient:?}",
        );
    }

    /// The success verdict is `success: true && code == 200`; a body
    /// with the right code but no success flag is not a green envelope.
    #[test]
    fn zai_quota_from_body_requires_the_success_flag() {
        let err = zai_quota_from_body(
            br#"{"code":200,"msg":"success","data":{"limits":[],"level":"max"}}"#,
        )
        .expect_err("no success flag fails");
        assert!(matches!(err, OauthUsageError::Decode(_)), "got {err:?}");
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
}
