//! The Anthropic backend. Keychain credentials probe the default-host
//! `/api/oauth/usage` endpoint with strict mapping. Token-mode
//! credentials probe a minimal billed `/v1/messages` call instead -
//! the usage endpoint refuses setup tokens, while a 200 response
//! there carries the `anthropic-ratelimit-unified-*` windows as
//! headers.

use std::collections::HashMap;

use async_trait::async_trait;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use serde_json::json;

use forge_primitives::usage::oauth::{OauthUsage, OauthUsageError};
use forge_primitives::usage::{UsageSnapshot, UsageSourceKind, UsageWindow};

use crate::helpers::{
    OAUTH_TIMEOUT, anthropic_windowed_probe, map_extra_usage, map_window, parse_retry_after,
    system_time_from_epoch, truncated_body_suffix,
};
use crate::{AccountEnv, BillingModel, ProbeError, Provider, ProviderBackend, ProviderHost};

/// `[accounts.env]` key carrying a per-account setup token (minted by
/// `claude setup-token`). Its presence makes an Anthropic account
/// token-mode: the probe authenticates with the token, never with the
/// keychain entry for the account's config dir.
const CLAUDE_CODE_OAUTH_TOKEN_ENV: &str = "CLAUDE_CODE_OAUTH_TOKEN";

/// The setup token `env` carries, when non-empty. An empty value stays
/// None so a real keychain account with a stale empty var in its env
/// block keeps its probe.
pub fn token_bearer<S: std::hash::BuildHasher>(env: &HashMap<String, String, S>) -> Option<&str> {
    env.get(CLAUDE_CODE_OAUTH_TOKEN_ENV)
        .map(String::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
}

/// True when `env` carries a non-empty setup token, making an
/// Anthropic account token-mode. A token-mode account has no keychain
/// entry of its own - the config dir is shared - so the mapper choice
/// and the repair policy branch on this rather than on the provider
/// alone.
pub fn is_token_mode<S: std::hash::BuildHasher>(env: &HashMap<String, String, S>) -> bool {
    token_bearer(env).is_some()
}

/// The Anthropic `[[accounts]] provider` token.
pub struct Anthropic;

#[async_trait]
impl ProviderBackend for Anthropic {
    fn token(&self) -> Provider {
        Provider::Anthropic
    }

    fn billing(&self) -> BillingModel {
        BillingModel::Windows
    }

    fn source(&self) -> UsageSourceKind {
        UsageSourceKind::Oauth
    }

    async fn probe(
        &self,
        account: &AccountEnv<'_>,
        host: &dyn ProviderHost,
    ) -> Result<UsageSnapshot, ProbeError> {
        match choose_mapper(token_bearer(account.env)) {
            Mapper::Token(bearer) => {
                let ua = host.user_agent().await.map_err(OauthUsageError::UaProbe)?;
                let client = host.http_client(OAUTH_TIMEOUT).map_err(OauthUsageError::Network)?;
                let headers =
                    messages_probe(&client, &ua, None, bearer).await.map_err(ProbeError::Fetch)?;
                tracing::info!(
                    target: "forge_providers::anthropic",
                    event_name = "unified_usage_probe_settled",
                    outcome = "ok",
                    "token account unified usage probe settled",
                );
                Ok(snapshot_from_unified_headers(&headers))
            }
            Mapper::Keychain => {
                let Some(credentials) = host.keychain(account.config_dir) else {
                    return Err(ProbeError::NoCredentials);
                };
                let ua = host.user_agent().await.map_err(OauthUsageError::UaProbe)?;
                let client = host.http_client(OAUTH_TIMEOUT).map_err(OauthUsageError::Network)?;
                let payload =
                    anthropic_windowed_probe(&client, &ua, None, &credentials.access_token)
                        .await
                        .map_err(ProbeError::Fetch)?;
                snapshot_from_payload(payload)
            }
        }
    }
}

/// The arm an account's credentials earn, paired with the credential
/// that earns it: the token arm runs the minimal messages probe, the
/// keychain arm the windowed `/api/oauth/usage` probe. Pure so the
/// routing stays unit-pinned - routing a keychain account onto the
/// token arm bills probe calls against a credential it does not own.
fn choose_mapper(token: Option<&str>) -> Mapper<'_> {
    match token {
        Some(bearer) => Mapper::Token(bearer),
        None => Mapper::Keychain,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Mapper<'a> {
    Token(&'a str),
    Keychain,
}

/// The token probe's model: the cheapest haiku-class ID; its retired
/// predecessor 404s, and 404s carry no headers.
const PROBE_MODEL: &str = "claude-haiku-4-5";

const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const UNIFIED_PREFIX: &str = "anthropic-ratelimit-unified-";

/// `/v1/messages` on the official host, or an override with any
/// trailing slash trimmed. Production always passes `None`: the
/// unified headers only ride the official base.
fn messages_url(base_url: Option<&str>) -> String {
    match base_url {
        Some(base) => format!("{}/v1/messages", base.trim_end_matches('/')),
        None => MESSAGES_URL.to_owned(),
    }
}

fn messages_headers(user_agent: &str, access_token: &str) -> Result<HeaderMap, OauthUsageError> {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    let ua = HeaderValue::from_str(user_agent)
        .map_err(|error| OauthUsageError::Network(format!("bad UA header: {error}")))?;
    headers.insert(USER_AGENT, ua);
    let bearer = HeaderValue::from_str(&format!("Bearer {access_token}"))
        .map_err(|error| OauthUsageError::Network(format!("bad bearer header: {error}")))?;
    headers.insert(AUTHORIZATION, bearer);
    Ok(headers)
}

/// One round-trip against `/v1/messages` with the minimal probe call:
/// the cheapest haiku model, `max_tokens` 1, a tiny input - about
/// nine billed tokens. On a 200 the response headers carry the
/// unified windows, so the HeaderMap is the payload; every other
/// status classifies exactly like the windowed probe so the loader
/// and poller treat both probes alike.
async fn messages_probe(
    client: &reqwest::Client,
    user_agent: &str,
    base_url: Option<&str>,
    access_token: &str,
) -> Result<HeaderMap, OauthUsageError> {
    let payload = json!({
        "model": PROBE_MODEL,
        "max_tokens": 1,
        "messages": [{ "role": "user", "content": "hi" }],
    });
    let response = client
        .post(messages_url(base_url))
        .headers(messages_headers(user_agent, access_token)?)
        .json(&payload)
        .send()
        .await
        .map_err(|error| OauthUsageError::Network(error.to_string()))?;

    let status = response.status().as_u16();
    // Parse Retry-After BEFORE consuming the response body - once we
    // call .bytes() the response object is moved.
    let retry_after = if status == 429 {
        response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(parse_retry_after)
    } else {
        None
    };
    let headers = response.headers().clone();
    let body = response
        .bytes()
        .await
        .map_err(|error| OauthUsageError::Network(format!("body read: {error}")))?;

    if status == 200 {
        tracing::debug!(
            target: "forge_providers::anthropic",
            event_name = "unified_usage_response",
            status,
            outcome = "ok",
            body_bytes = body.len(),
            five_hour_status = %unified_header_value(&headers, "5h-status"),
            seven_day_status = %unified_header_value(&headers, "7d-status"),
            unified_status = %unified_header_value(&headers, "status"),
            unified_fallback = %unified_header_value(&headers, "fallback"),
            overage_status = %unified_header_value(&headers, "overage-status"),
        );
    } else {
        tracing::warn!(
            target: "forge_providers::anthropic",
            event_name = "unified_usage_response",
            status,
            outcome = "non_ok",
            retry_after_secs = ?retry_after.map(|duration| duration.as_secs()),
            body_suffix = %truncated_body_suffix(&body),
        );
    }

    match status {
        200 => Ok(headers),
        401 | 403 => Err(OauthUsageError::Unauthorized(status)),
        429 => Err(OauthUsageError::RateLimited { retry_after }),
        _ => Err(OauthUsageError::HttpStatus(status, truncated_body_suffix(&body))),
    }
}

fn unified_header_value(headers: &HeaderMap, suffix: &str) -> String {
    headers
        .get(format!("{UNIFIED_PREFIX}{suffix}"))
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

/// One unified window off the 200's headers: utilization arrives on a
/// 0..1 scale and maps into the snapshot's percentage scale, the reset
/// arrives as an epoch. Each header is independently optional.
fn unified_window(headers: &HeaderMap, window: &str) -> Option<UsageWindow> {
    let utilization = headers
        .get(format!("{UNIFIED_PREFIX}{window}-utilization"))
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<f64>().ok())
        .map(|raw| (raw * 100.0).clamp(0.0, 100.0))?;
    let resets_at = headers
        .get(format!("{UNIFIED_PREFIX}{window}-reset"))
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<i64>().ok())
        .and_then(system_time_from_epoch);
    Some(UsageWindow { utilization, resets_at, reset_description: None })
}

/// Map a 200's unified headers into a snapshot. Lenient by design: a
/// 200 without the unified headers (an API-key bearer rather than a
/// plan credential, or a plan without unified limits) is the barless
/// row the scope refusal used to settle to, not a fetch error.
fn snapshot_from_unified_headers(headers: &HeaderMap) -> UsageSnapshot {
    UsageSnapshot {
        source: UsageSourceKind::Oauth,
        fetched_at: std::time::SystemTime::now(),
        five_hour: unified_window(headers, "5h"),
        seven_day: unified_window(headers, "7d"),
        seven_day_opus: None,
        seven_day_sonnet: None,
        extra_usage: None,
        spend: None,
    }
}

/// Map a fetched payload into a snapshot, requiring the five-hour
/// window: on the keychain path a 200 without it signals response-
/// shape drift, so it errors instead of rendering an all-absent row.
fn snapshot_from_payload(payload: OauthUsage) -> Result<UsageSnapshot, ProbeError> {
    let five_hour = map_window(payload.five_hour);
    if five_hour.is_none() {
        return Err(ProbeError::Unmappable(
            "Claude OAuth usage response did not include the current session window.".to_owned(),
        ));
    }
    Ok(UsageSnapshot {
        source: UsageSourceKind::Oauth,
        fetched_at: std::time::SystemTime::now(),
        five_hour,
        seven_day: map_window(payload.seven_day),
        seven_day_opus: map_window(payload.seven_day_opus),
        seven_day_sonnet: map_window(payload.seven_day_sonnet),
        extra_usage: map_extra_usage(payload.extra_usage),
        spend: None,
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    use async_trait::async_trait;

    use super::*;

    #[test]
    fn token_bearer_takes_a_non_empty_env_token() {
        let mut env = HashMap::new();
        env.insert("CLAUDE_CODE_OAUTH_TOKEN".to_owned(), "setup-token".to_owned());
        assert_eq!(token_bearer(&env), Some("setup-token"));
    }

    /// An empty CLAUDE_CODE_OAUTH_TOKEN must not flip the account into
    /// token mode: a real keychain account with a stale empty var in
    /// its env block would otherwise lose its probe entirely.
    #[test]
    fn token_bearer_rejects_blank_tokens() {
        let mut env = HashMap::new();
        env.insert("CLAUDE_CODE_OAUTH_TOKEN".to_owned(), "  ".to_owned());
        assert_eq!(token_bearer(&env), None);
        assert_eq!(token_bearer(&HashMap::new()), None);
    }

    /// The arm-routing pin: a token credential earns the token arm
    /// (the minimal messages probe), the keychain the windowed
    /// `/api/oauth/usage` probe. Inverting this bills probe calls
    /// against a credential the account does not own.
    #[test]
    fn token_bearer_earns_the_token_arm_and_keychain_the_windowed_arm() {
        assert_eq!(choose_mapper(Some("tok")), Mapper::Token("tok"));
        assert_eq!(choose_mapper(None), Mapper::Keychain);
    }

    fn unified_header_map() -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("anthropic-ratelimit-unified-5h-utilization", "0.53"),
            ("anthropic-ratelimit-unified-5h-reset", "1766664000"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.06"),
            ("anthropic-ratelimit-unified-7d-reset", "1767268800"),
        ] {
            headers.insert(name, HeaderValue::from_static(value));
        }
        headers
    }

    /// The unified utilization rides the headers on a 0..1 scale; the
    /// snapshot's windows are percentages, so the mapping scales it
    /// and the reset epoch lands on SystemTime.
    #[test]
    fn unified_headers_map_both_windows() {
        let snapshot = snapshot_from_unified_headers(&unified_header_map());
        let five_hour = snapshot.five_hour.expect("five hour window");
        assert!((five_hour.utilization - 53.0).abs() < 1e-9, "got {}", five_hour.utilization);
        assert_eq!(
            five_hour.resets_at,
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_766_664_000)),
        );
        let seven_day = snapshot.seven_day.expect("seven day window");
        assert!((seven_day.utilization - 6.0).abs() < 1e-9, "got {}", seven_day.utilization);
        assert_eq!(
            seven_day.resets_at,
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_767_268_800)),
        );
        assert!(snapshot.seven_day_opus.is_none());
        assert!(snapshot.seven_day_sonnet.is_none());
        assert_eq!(snapshot.source, UsageSourceKind::Oauth);
    }

    #[test]
    fn unified_utilization_clamps_to_the_percentage_range() {
        let mut headers = HeaderMap::new();
        headers
            .insert("anthropic-ratelimit-unified-5h-utilization", HeaderValue::from_static("1.4"));
        let window = unified_window(&headers, "5h").expect("window");
        assert!((window.utilization - 100.0).abs() < f64::EPSILON, "got {}", window.utilization);
    }

    /// A 200 without the unified headers - an API-key bearer rather
    /// than a plan credential, or a plan without unified limits -
    /// maps to the barless snapshot the scope refusal used to settle
    /// to, not a fetch error.
    #[test]
    fn a_200_without_unified_headers_maps_to_a_barless_snapshot() {
        let snapshot = snapshot_from_unified_headers(&HeaderMap::new());
        assert!(snapshot.five_hour.is_none());
        assert!(snapshot.seven_day.is_none());
        assert_eq!(snapshot.source, UsageSourceKind::Oauth);
    }

    const PROBE_RESPONSE_200: &str = concat!(
        "HTTP/1.1 200 OK\r\n",
        "content-type: application/json\r\n",
        "anthropic-ratelimit-unified-5h-status: ok\r\n",
        "anthropic-ratelimit-unified-5h-utilization: 0.21\r\n",
        "anthropic-ratelimit-unified-5h-reset: 1766664000\r\n",
        "anthropic-ratelimit-unified-7d-utilization: 0.15\r\n",
        "anthropic-ratelimit-unified-7d-reset: 1767268800\r\n",
        "content-length: 2\r\n\r\n{}",
    );

    const PROBE_RESPONSE_429: &str =
        "HTTP/1.1 429 Too Many Requests\r\nretry-after: 30\r\ncontent-length: 0\r\n\r\n";

    const PROBE_RESPONSE_401: &str =
        "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: 0\r\n\r\n";

    /// Answer one request, then hand the raw request text back so a
    /// test can pin the probe's request shape. The request body is
    /// drained before answering: closing with unread bytes pending
    /// sends an RST that can destroy the in-flight response.
    fn serve_one(
        listener: std::net::TcpListener,
        response: &'static str,
    ) -> std::sync::mpsc::Receiver<String> {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::{Read, Write as _};
            let Ok((mut sock, _)) = listener.accept() else { return };
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                match sock.read(&mut byte) {
                    Ok(1) => request.push(byte[0]),
                    _ => return,
                }
            }
            let length: usize = String::from_utf8_lossy(&request)
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(0);
            for _ in 0..length {
                match sock.read(&mut byte) {
                    Ok(1) => request.push(byte[0]),
                    _ => return,
                }
            }
            let _ = sock.write_all(response.as_bytes());
            let _ = sock.shutdown(std::net::Shutdown::Both);
            let _ = sender.send(String::from_utf8_lossy(&request).to_string());
        });
        receiver
    }

    /// The offline round-trip: the probe posts the minimal call to
    /// /v1/messages and the 200's headers map into windows. The
    /// bearer here is a fake; no credential material is printed.
    #[tokio::test]
    async fn the_messages_probe_posts_the_minimal_call_and_maps_the_headers() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let requests = serve_one(listener, PROBE_RESPONSE_200);
        let client = reqwest::Client::builder().no_proxy().build().expect("client");
        let headers = tokio::time::timeout(
            Duration::from_secs(5),
            messages_probe(
                &client,
                "claude-code/1.0.0",
                Some(&format!("http://{addr}")),
                "probe-token",
            ),
        )
        .await
        .expect("the probe returns")
        .expect("probe");
        let request =
            requests.recv_timeout(Duration::from_secs(5)).expect("the server saw a request");
        assert!(request.starts_with("POST /v1/messages "), "got {request:?}");
        assert!(request.contains("anthropic-version: 2023-06-01"));
        assert!(request.contains("authorization: Bearer probe-token"));
        assert!(request.contains("user-agent: claude-code/1.0.0"));
        assert!(request.contains("\"max_tokens\":1"));
        assert!(request.contains("\"model\":\"claude-haiku-4-5\""));

        let snapshot = snapshot_from_unified_headers(&headers);
        let five_hour = snapshot.five_hour.expect("five hour window");
        assert!((five_hour.utilization - 21.0).abs() < 1e-9, "got {}", five_hour.utilization);
        let seven_day = snapshot.seven_day.expect("seven day window");
        assert!((seven_day.utilization - 15.0).abs() < 1e-9, "got {}", seven_day.utilization);
    }

    /// A 429 classifies as RateLimited carrying the server Retry-After
    /// - the headers-only-on-200 contract means an error response can
    /// never be read as a window.
    #[tokio::test]
    async fn a_429_is_a_rate_limited_error_with_the_retry_after() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let _requests = serve_one(listener, PROBE_RESPONSE_429);
        let client = reqwest::Client::builder().no_proxy().build().expect("client");
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            messages_probe(&client, "claude-code/1.0.0", Some(&format!("http://{addr}")), "tok"),
        )
        .await
        .expect("the probe returns");
        assert!(
            matches!(
                result,
                Err(OauthUsageError::RateLimited { retry_after: Some(retry_after) })
                    if retry_after == Duration::from_secs(30)
            ),
            "got {result:?}",
        );
    }

    /// A rejected token reaches the loader as Unauthorized - the
    /// re-mint repair hinges on the classification.
    #[tokio::test]
    async fn a_401_is_an_unauthorized_error() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let _requests = serve_one(listener, PROBE_RESPONSE_401);
        let client = reqwest::Client::builder().no_proxy().build().expect("client");
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            messages_probe(&client, "claude-code/1.0.0", Some(&format!("http://{addr}")), "tok"),
        )
        .await
        .expect("the probe returns");
        assert!(matches!(result, Err(OauthUsageError::Unauthorized(401))), "got {result:?}");
    }

    /// The full unified family the CLI binary enumerates; only the
    /// two windows map out of it, the rest are ignored.
    #[test]
    fn the_complete_unified_family_maps_only_the_two_windows() {
        let mut headers = unified_header_map();
        for (name, value) in [
            ("anthropic-ratelimit-unified-5h-status", "ok"),
            ("anthropic-ratelimit-unified-7d-status", "ok"),
            ("anthropic-ratelimit-unified-status", "ok"),
            ("anthropic-ratelimit-unified-reset", "1767268800"),
            ("anthropic-ratelimit-unified-representative-claim", "5h"),
            ("anthropic-ratelimit-unified-fallback", "0"),
            ("anthropic-ratelimit-unified-overage-status", "active"),
            ("anthropic-ratelimit-unified-overage-disabled-reason", ""),
            ("anthropic-ratelimit-unified-grace-status", "none"),
            ("anthropic-ratelimit-unified-grace-5h-utilization", "0"),
            ("anthropic-ratelimit-unified-grace-7d-utilization", "0"),
            ("anthropic-ratelimit-unified-upgrade-paths", "x"),
        ] {
            headers.insert(name, HeaderValue::from_static(value));
        }
        let snapshot = snapshot_from_unified_headers(&headers);
        let five_hour = snapshot.five_hour.expect("five hour window");
        assert!((five_hour.utilization - 53.0).abs() < 1e-9, "got {}", five_hour.utilization);
        let seven_day = snapshot.seven_day.expect("seven day window");
        assert!((seven_day.utilization - 6.0).abs() < 1e-9, "got {}", seven_day.utilization);
        assert!(snapshot.seven_day_opus.is_none());
        assert!(snapshot.seven_day_sonnet.is_none());
        assert!(snapshot.extra_usage.is_none());
        assert!(snapshot.spend.is_none());
    }

    /// Either header alone is a legal shape: utilization without a
    /// reset maps a window with no reset instant, and a reset without
    /// utilization maps no window at all.
    #[test]
    fn a_partial_window_maps_what_is_present() {
        let mut utilization_only = HeaderMap::new();
        utilization_only
            .insert("anthropic-ratelimit-unified-5h-utilization", HeaderValue::from_static("0.2"));
        let snapshot = snapshot_from_unified_headers(&utilization_only);
        let five_hour = snapshot.five_hour.expect("window from utilization alone");
        assert!((five_hour.utilization - 20.0).abs() < 1e-9, "got {}", five_hour.utilization);
        assert_eq!(five_hour.resets_at, None);

        let mut reset_only = HeaderMap::new();
        reset_only
            .insert("anthropic-ratelimit-unified-7d-reset", HeaderValue::from_static("1767268800"));
        let snapshot = snapshot_from_unified_headers(&reset_only);
        assert!(snapshot.seven_day.is_none(), "a reset without utilization maps no window");
    }

    #[test]
    fn strict_mapping_requires_the_session_window() {
        // The seven-day-only shape (post-5h-reset steady state) is
        // valid on the lenient path but not the keychain path.
        let payload: OauthUsage =
            serde_json::from_slice(br#"{"seven_day":{"utilization":10.0}}"#).expect("decode");
        let err = snapshot_from_payload(payload).expect_err("no five_hour must not map");
        assert!(
            matches!(err, ProbeError::Unmappable(_)),
            "the keychain path reports a 200 without the session window as unmappable; got {err:?}",
        );
    }

    #[test]
    fn strict_mapping_keeps_the_present_windows() {
        let payload: OauthUsage = serde_json::from_slice(
            br#"{
                "five_hour": { "utilization": 12.5, "resets_at": "2025-12-25T12:00:00.000Z" },
                "seven_day_sonnet": { "utilization": 5 },
                "unknown_field": true
            }"#,
        )
        .expect("decode");
        let snapshot = snapshot_from_payload(payload).expect("snapshot");
        assert_eq!(snapshot.five_hour.as_ref().map(|window| window.utilization), Some(12.5));
        assert_eq!(snapshot.seven_day_sonnet.as_ref().map(|window| window.utilization), Some(5.0));
        assert!(snapshot.seven_day.is_none());
        assert_eq!(snapshot.source, UsageSourceKind::Oauth);
    }

    /// A host that cannot resolve the UA surfaces the UaProbe class -
    /// a local exec problem, not a verdict about the endpoint - so the
    /// callers' retry path engages.
    #[tokio::test]
    async fn a_host_ua_failure_is_a_ua_probe_error_not_a_network_failure() {
        let backend = Anthropic;
        let account = AccountEnv { config_dir: Path::new("/tmp/unused"), env: &HashMap::new() };
        let result = backend.probe(&account, &FailingUaHost).await;
        assert!(
            matches!(result, Err(ProbeError::Fetch(OauthUsageError::UaProbe(_)))),
            "got {result:?}",
        );
    }

    /// A keychain the host cannot read is the probe's NoCredentials,
    /// and the probe must not reach the network for it.
    #[tokio::test]
    async fn an_unreadable_keychain_is_no_credentials_without_probing() {
        let backend = Anthropic;
        let account = AccountEnv { config_dir: Path::new("/tmp/unused"), env: &HashMap::new() };
        let result = backend.probe(&account, &EmptyHost).await;
        assert!(matches!(result, Err(ProbeError::NoCredentials)), "got {result:?}");
    }

    struct FailingUaHost;

    #[async_trait]
    impl ProviderHost for FailingUaHost {
        fn keychain(&self, _config_dir: &Path) -> Option<crate::OauthCredentials> {
            Some(crate::OauthCredentials { access_token: "tok".to_owned(), expires_at: None })
        }

        fn http_client(&self, _timeout: Duration) -> Result<reqwest::Client, String> {
            reqwest::Client::builder().build().map_err(|e| e.to_string())
        }

        async fn user_agent(&self) -> Result<String, String> {
            Err("claude missing from PATH".to_owned())
        }
    }

    struct EmptyHost;

    #[async_trait]
    impl ProviderHost for EmptyHost {
        fn keychain(&self, _config_dir: &Path) -> Option<crate::OauthCredentials> {
            None
        }

        fn http_client(&self, _timeout: Duration) -> Result<reqwest::Client, String> {
            unreachable!("the probe must not build a client for a missing credential")
        }

        async fn user_agent(&self) -> Result<String, String> {
            unreachable!("the probe must not resolve a UA for a missing credential")
        }
    }
}
