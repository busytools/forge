//! Provider probe entry points not yet behind the forge-providers
//! backends: the [`ProbePlan`] decision, the windowed
//! `/api/oauth/usage` probe the codex base-url arm drives, and the
//! OpenRouter key + Z.ai monitor probes. Each PR of #873 moves an arm
//! into forge-providers; this module shrinks until `ProbePlan` is
//! deleted.

use std::collections::HashMap;

use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue};

pub use forge_primitives::account::Provider;
pub use forge_primitives::usage::oauth::{OauthUsage, OauthUsageError};
use forge_primitives::usage::zai::{QuotaLimitData, QuotaLimitResponse};

use super::oauth_credentials::OauthCredentials;
use forge_providers::ProviderHost;
use forge_providers::helpers::{
    OAUTH_TIMEOUT, anthropic_windowed_probe, parse_retry_after, truncated_body_suffix,
};
use forge_providers::token_bearer;

/// True when `env` carries a non-empty `CLAUDE_CODE_OAUTH_TOKEN`.
/// A token-mode account has no keychain entry of its own - the config
/// dir is shared - so both the probe and preflight's repair copy branch
/// on this rather than on the provider alone.
pub fn is_token_mode<S: std::hash::BuildHasher>(env: &HashMap<String, String, S>) -> bool {
    token_bearer(env).is_some()
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
    /// response leniently
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
    /// token always answers 403 `oauth_scope_insufficient` - the probe
    /// settles that refusal to the empty payload,
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

/// One round-trip against `/api/oauth/usage` with the given bearer, on
/// the default host or a `base_url` override. The codex base-url
/// arm's engine: the request, status classification and scope-refusal
/// detection live in `forge_providers::helpers`; this wrapper adds the
/// host-resolved UA and the extra-roots client the backends receive
/// through the host port.
pub async fn probe(
    credentials: &OauthCredentials,
    base_url: Option<&str>,
) -> Result<OauthUsage, OauthUsageError> {
    let ua = crate::cloud::provider_host::AgentHost
        .user_agent()
        .await
        .map_err(OauthUsageError::UaProbe)?;
    let client =
        crate::http_trust::with_extra_roots(reqwest::Client::builder().timeout(OAUTH_TIMEOUT))
            .build()
            .map_err(|error| OauthUsageError::Network(format!("client build: {error}")))?;
    anthropic_windowed_probe(&client, &ua, base_url, &credentials.access_token).await
}

#[cfg(test)]
mod tests {

    use super::*;

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
}
