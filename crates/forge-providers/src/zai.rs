//! The Zai backend: the GLM coding plan's monitor probe against
//! `{host_root}/api/monitor/usage/quota/limit`, authenticated by the
//! raw `[accounts.env]` `ANTHROPIC_AUTH_TOKEN` (no Bearer prefix).
//! The configured base url carries the `/api/anthropic` chat prefix,
//! so the monitor URL derives from the scheme+host alone. Every Z.ai
//! monitor endpoint answers HTTP 200 regardless of outcome - a wrong
//! key and a wrong path share the status with a healthy response - so
//! the verdict lives inside the `{success, code, msg}` envelope.

use std::time::SystemTime;

use async_trait::async_trait;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue};

use forge_primitives::usage::oauth::OauthUsageError;
use forge_primitives::usage::zai::{QuotaLimitData, QuotaLimitEntry, QuotaLimitResponse};
use forge_primitives::usage::{UsageSnapshot, UsageSourceKind, UsageWindow};

use crate::helpers::{
    BaseUrlCredential, MissingBase, OAUTH_TIMEOUT, base_url_credential, system_time_from_epoch,
    truncated_body_suffix,
};
use crate::{AccountEnv, BillingModel, ProbeError, Provider, ProviderBackend, ProviderHost};

/// The Zai `[[accounts]] provider` token.
pub struct Zai;

#[async_trait]
impl ProviderBackend for Zai {
    fn token(&self) -> Provider {
        Provider::Zai
    }

    fn billing(&self) -> BillingModel {
        BillingModel::Windows
    }

    fn source(&self) -> UsageSourceKind {
        UsageSourceKind::ZaiMonitor
    }

    async fn probe(
        &self,
        account: &AccountEnv<'_>,
        host: &dyn ProviderHost,
    ) -> Result<UsageSnapshot, ProbeError> {
        match choose_mapper(base_url_credential(account.env)) {
            Mapper::Monitor(credential) => {
                let client = host.http_client(OAUTH_TIMEOUT).map_err(OauthUsageError::Network)?;
                let payload = monitor_probe(&client, &credential.base_url, &credential.bearer)
                    .await
                    .map_err(ProbeError::Fetch)?;
                snapshot_from_zai_quota(payload)
            }
            Mapper::MissingBase(missing) => Err(ProbeError::Unmappable(missing.to_string())),
        }
    }
}

/// The mapper the account's env earns, paired with the credential that
/// earns it: the base-url pair earns the monitor mapper, and a missing
/// base url carries its error to the probe's Unmappable surface. Pure
/// so the routing stays unit-pinned - the probe cannot be driven
/// against the network offline.
fn choose_mapper(credential: Result<BaseUrlCredential, MissingBase>) -> Mapper {
    match credential {
        Ok(credential) => Mapper::Monitor(credential),
        Err(missing) => Mapper::MissingBase(missing),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Mapper {
    /// The credential bound to the monitor mapper: the arm holding
    /// this cannot map the windowed `/api/oauth/usage` shape or read
    /// the keychain.
    Monitor(BaseUrlCredential),
    MissingBase(MissingBase),
}

/// The scheme+host of an account's `ANTHROPIC_BASE_URL` - everything
/// before the first path segment. The Z.ai monitor paths live off the
/// site root (`https://api.z.ai`), never under the `/api/anthropic`
/// chat prefix the base carries.
fn monitor_host(base_url: &str) -> &str {
    let trimmed = base_url.trim().trim_end_matches('/');
    let after = trimmed.split_once("://").map_or(trimmed, |(_, rest)| rest);
    let host = after.split('/').next().unwrap_or_default();
    if host.is_empty() {
        return trimmed;
    }
    &trimmed[..trimmed.len() - after.len() + host.len()]
}

fn monitor_url(base_url: &str) -> String {
    format!("{}/api/monitor/usage/quota/limit", monitor_host(base_url))
}

fn monitor_headers(key: &str) -> Result<HeaderMap, OauthUsageError> {
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
async fn monitor_probe(
    client: &reqwest::Client,
    base_url: &str,
    key: &str,
) -> Result<QuotaLimitData, OauthUsageError> {
    let response = client
        .get(monitor_url(base_url))
        .headers(monitor_headers(key)?)
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
            target: "forge_providers::zai",
            event_name = "zai_monitor_response",
            status,
            outcome = "ok",
            body_bytes = body.len(),
        );
    } else {
        tracing::warn!(
            target: "forge_providers::zai",
            event_name = "zai_monitor_response",
            status,
            outcome = "non_ok",
            body_suffix = %truncated_body_suffix(&body),
        );
    }

    match status {
        200 => quota_from_body(&body),
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
fn quota_from_body(body: &[u8]) -> Result<QuotaLimitData, OauthUsageError> {
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

/// Map a Z.ai quota-limit payload into a
/// [`forge_primitives::usage::UsageSnapshot`].
///
/// CREDIT_LIMIT entries carry the windows in credits: `usage` is the
/// per-window limit and consumption is `usage - remaining`, which is
/// where the utilization percentage comes from - the payload's own
/// `percentage` field is not mapped. The unit-3 (hours) entry is the
/// 5-hour window, unit-6 (weeks) the weekly one. An absent 5-hour
/// `nextResetTime`, the steady state before the first successful
/// request, maps to a window with no reset moment.
///
/// Fallible like the forge-providers spend and window mappers: a
/// payload with no mappable window entries is a response forge cannot
/// read rather than a bill of zero.
fn snapshot_from_zai_quota(payload: QuotaLimitData) -> Result<UsageSnapshot, ProbeError> {
    let mut five_hour = None;
    let mut seven_day = None;
    for entry in payload.limits {
        if entry.kind.as_deref() != Some("CREDIT_LIMIT") {
            continue;
        }
        match entry.unit {
            Some(3) => five_hour = window_from_entry(&entry),
            Some(6) => seven_day = window_from_entry(&entry),
            _ => {}
        }
    }
    if five_hour.is_none() && seven_day.is_none() {
        return Err(ProbeError::Unmappable(
            "Z.ai quota response carried no CREDIT_LIMIT window entries.".to_owned(),
        ));
    }
    Ok(UsageSnapshot {
        source: UsageSourceKind::ZaiMonitor,
        fetched_at: SystemTime::now(),
        five_hour,
        seven_day,
        seven_day_opus: None,
        seven_day_sonnet: None,
        extra_usage: None,
        spend: None,
    })
}

fn window_from_entry(entry: &QuotaLimitEntry) -> Option<UsageWindow> {
    let usage = entry.usage?;
    let remaining = entry.remaining?;
    let utilization = if usage > 0.0 {
        ((usage - remaining).max(0.0) / usage * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };
    Some(UsageWindow {
        utilization,
        resets_at: entry.next_reset_time.and_then(system_time_from_epoch),
        reset_description: None,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;
    use std::time::Duration;

    use async_trait::async_trait;

    use super::*;

    fn env_with_base(base: &str) -> HashMap<String, String> {
        let mut env = HashMap::new();
        env.insert("ANTHROPIC_BASE_URL".to_owned(), base.to_owned());
        env.insert("ANTHROPIC_AUTH_TOKEN".to_owned(), "zai-key".to_owned());
        env
    }

    fn green_envelope_body() -> &'static [u8] {
        br#"{
            "code": 200,
            "msg": "success",
            "data": {
                "limits": [
                    {"type":"CREDIT_LIMIT","unit":3,"number":5,"usage":28000,
                     "remaining":27104,"percentage":3.2,"currentValue":0,
                     "nextResetTime":1757025600000},
                    {"type":"CREDIT_LIMIT","unit":6,"number":1,"usage":140000,
                     "remaining":139000,"percentage":0.71,"currentValue":0,
                     "nextResetTime":1757000000000}
                ],
                "level": "max"
            },
            "success": true
        }"#
    }

    #[test]
    fn zai_backend_is_windowed() {
        assert_eq!(Zai.token(), Provider::Zai);
        assert_eq!(Zai.billing(), BillingModel::Windows);
    }

    /// The arm-routing pin: a base-url credential earns the monitor
    /// mapper carrying that credential; a missing base earns the
    /// Unmappable arm. Wiring the pair to the windowed usage mapper,
    /// or probing with a credential the env did not produce, cannot
    /// compile.
    #[test]
    fn a_base_url_credential_earns_the_monitor_mapper_and_a_missing_base_the_error_arm() {
        let credential =
            base_url_credential(&env_with_base("https://api.z.ai/api/anthropic")).expect("cred");
        assert_eq!(
            choose_mapper(Ok(credential.clone())),
            Mapper::Monitor(credential),
            "the monitor arm runs with the credential that earned it",
        );
        assert!(
            matches!(choose_mapper(Err(MissingBase)), Mapper::MissingBase(_)),
            "a missing base surfaces Unmappable, not a probe against a default host",
        );
    }

    /// A missing base url never reaches the network: the error
    /// surfaces before a client is built.
    #[tokio::test]
    async fn a_missing_base_is_unmappable_without_probing() {
        let backend = Zai;
        let account = AccountEnv { config_dir: Path::new("/tmp/unused"), env: &HashMap::new() };
        let result = backend.probe(&account, &UnreachableHost).await;
        assert!(matches!(result, Err(ProbeError::Unmappable(_))), "got {result:?}");
    }

    #[test]
    fn monitor_url_derives_the_host_root() {
        assert_eq!(
            monitor_url("https://api.z.ai/api/anthropic"),
            "https://api.z.ai/api/monitor/usage/quota/limit",
        );
        assert_eq!(
            monitor_url("https://api.z.ai/api/anthropic/"),
            "https://api.z.ai/api/monitor/usage/quota/limit",
            "trailing slash trimmed so base and base/ behave identically",
        );
    }

    #[test]
    fn monitor_headers_send_the_key_raw_without_a_bearer_prefix() {
        let headers = monitor_headers("zai-key").expect("headers");
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
    fn quota_from_body_maps_the_fresh_account_envelope() {
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
        let data = quota_from_body(body).expect("a green envelope parses");
        assert_eq!(data.limits.len(), 2);
        assert_eq!(data.level.as_deref(), Some("max"));
    }

    #[test]
    fn quota_from_body_fails_on_wrong_key_inside_a_200() {
        let err =
            quota_from_body(br#"{"code":401,"msg":"token expired or incorrect","success":false}"#)
                .expect_err("wrong key fails");
        assert!(
            matches!(err, OauthUsageError::Unauthorized(401)),
            "a body-level 401 must classify as unauthorized, got {err:?}",
        );
    }

    #[test]
    fn quota_from_body_fails_on_wrong_path_inside_a_200() {
        let err = quota_from_body(br#"{"code":500,"msg":"404 NOT_FOUND","success":false}"#)
            .expect_err("a wrong path fails");
        assert!(err.to_string().contains("404 NOT_FOUND"), "the msg reaches the log: {err}");
    }

    /// A silent empty 200 (observed on the model-usage endpoint) is a
    /// failure, not a bill of zero.
    #[test]
    fn quota_from_body_fails_on_empty_body() {
        let err = quota_from_body(b"").expect_err("empty body fails");
        assert!(err.to_string().contains("empty"), "got {err}");
    }

    /// The success verdict is `success: true && code == 200`; a body
    /// with the right code but no success flag is not a green envelope.
    #[test]
    fn quota_from_body_requires_the_success_flag() {
        let err =
            quota_from_body(br#"{"code":200,"msg":"success","data":{"limits":[],"level":"max"}}"#)
                .expect_err("no success flag fails");
        assert!(matches!(err, OauthUsageError::Decode(_)), "got {err:?}");
    }

    /// The production wiring through the real backend and host: a
    /// green envelope round-trips into windowed percentages, on the
    /// raw env key against the host-root monitor path. Proves the
    /// injected client, the per-request headers and the envelope
    /// verdict, which the pure mapper pins cannot see. No UA is
    /// resolved - the monitor probe sends none - so this runs on a
    /// runner without the claude binary.
    #[tokio::test]
    async fn a_200_envelope_probes_through_to_a_windowed_snapshot() {
        let body = green_envelope_body();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            use std::io::{Read, Write as _};
            let Ok((mut sock, _)) = listener.accept() else { return };
            // Drain the request before answering: closing with unread
            // request bytes pending sends an RST that can destroy the
            // response already in flight.
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                match sock.read(&mut byte) {
                    Ok(1) => request.push(byte[0]),
                    _ => break,
                }
            }
            let request_text = String::from_utf8_lossy(&request).into_owned();
            assert!(
                request_text.contains("GET /api/monitor/usage/quota/limit HTTP/1.1"),
                "the request must hit the host-root monitor path, got: {}",
                request_text.lines().next().unwrap_or_default()
            );
            assert!(
                request_text.contains("authorization: zai-key"),
                "the env key goes out raw, no Bearer prefix, got: {request_text}",
            );
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(header.as_bytes());
            let _ = sock.write_all(body);
            let _ = sock.shutdown(std::net::Shutdown::Both);
        });
        let env = env_with_base(&format!("http://{addr}/api/anthropic"));
        let account = AccountEnv { config_dir: Path::new("/tmp/unused"), env: &env };
        let snapshot = Zai.probe(&account, &LocalHost).await.expect("snapshot");
        assert_eq!(snapshot.source, UsageSourceKind::ZaiMonitor);
        let five = snapshot.five_hour.expect("5h window");
        assert!(
            (five.utilization - 3.2).abs() < 1e-9,
            "896 of 28000 credits is 3.2%, got {}",
            five.utilization,
        );
    }

    struct LocalHost;

    #[async_trait]
    impl ProviderHost for LocalHost {
        fn keychain(&self, _config_dir: &Path) -> Option<crate::OauthCredentials> {
            unreachable!("the zai probe never reads the keychain")
        }

        fn http_client(&self, timeout: Duration) -> Result<reqwest::Client, String> {
            reqwest::Client::builder().timeout(timeout).build().map_err(|e| e.to_string())
        }

        async fn user_agent(&self) -> Result<String, String> {
            unreachable!("the zai monitor probe sends no User-Agent header")
        }
    }

    struct UnreachableHost;

    #[async_trait]
    impl ProviderHost for UnreachableHost {
        fn keychain(&self, _config_dir: &Path) -> Option<crate::OauthCredentials> {
            unreachable!("the zai probe never reads the keychain")
        }

        fn http_client(&self, _timeout: Duration) -> Result<reqwest::Client, String> {
            unreachable!("the probe must not build a client for a missing base url")
        }

        async fn user_agent(&self) -> Result<String, String> {
            unreachable!("the probe must not resolve a UA for a missing base url")
        }
    }

    /// The verified after-usage shape maps credit arithmetic to window
    /// percentages: utilization is `usage - remaining` against `usage`,
    /// and `nextResetTime` epoch milliseconds become the window's
    /// reset moment.
    #[test]
    fn zai_quota_maps_credit_windows_to_percentages() {
        let payload: QuotaLimitData = serde_json::from_str(
            r#"{
                "limits": [
                    {"type":"CREDIT_LIMIT","unit":3,"number":5,"usage":28000,
                     "remaining":27104,"percentage":3.2,"currentValue":0,
                     "nextResetTime":1757025600000},
                    {"type":"CREDIT_LIMIT","unit":6,"number":1,"usage":140000,
                     "remaining":139000,"percentage":0.71,"currentValue":0,
                     "nextResetTime":1757000000000}
                ],
                "level": "max"
            }"#,
        )
        .expect("decode");
        let snapshot = snapshot_from_zai_quota(payload).expect("maps");
        assert_eq!(snapshot.source, UsageSourceKind::ZaiMonitor);
        let five = snapshot.five_hour.expect("5h window");
        assert!(
            (five.utilization - 3.2).abs() < 1e-9,
            "896 of 28000 credits is 3.2%, got {}",
            five.utilization,
        );
        assert_eq!(
            five.resets_at,
            // 1757025600000 ms on the wire; the same instant in seconds.
            Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_757_025_600)),
            "epoch-ms nextResetTime becomes the reset moment",
        );
        let weekly = snapshot.seven_day.expect("weekly window");
        assert!(
            (weekly.utilization - 1000.0 / 1400.0).abs() < 1e-9,
            "1000 of 140000 credits, got {}",
            weekly.utilization,
        );
        assert!(snapshot.spend.is_none(), "a subscription carries no per-key spend");
    }

    /// A fresh account has consumed nothing and the 5-hour entry has
    /// no `nextResetTime` yet - that maps to a zero window with no
    /// reset moment, not an error and not a fabricated reset.
    #[test]
    fn zai_quota_maps_a_fresh_account_with_no_five_hour_reset() {
        let payload: QuotaLimitData = serde_json::from_str(
            r#"{
                "limits": [
                    {"type":"CREDIT_LIMIT","unit":3,"number":5,"usage":28000,
                     "remaining":28000},
                    {"type":"CREDIT_LIMIT","unit":6,"number":1,"usage":140000,
                     "remaining":140000,"nextResetTime":1757000000000}
                ],
                "level": "max"
            }"#,
        )
        .expect("decode");
        let snapshot = snapshot_from_zai_quota(payload).expect("maps");
        let five = snapshot.five_hour.expect("5h window");
        assert!(five.utilization.abs() < f64::EPSILON, "fresh account has consumed nothing");
        assert_eq!(five.resets_at, None, "no nextResetTime means no reset moment yet");
    }

    /// An entry with `remaining` absent is an unreadable half-entry,
    /// not a full one: it is skipped like a missing `usage`, because
    /// asserting a default would render a saturated red row off a field
    /// the payload never carried.
    #[test]
    fn zai_quota_skips_an_entry_without_remaining() {
        let payload: QuotaLimitData = serde_json::from_str(
            r#"{
                "limits": [
                    {"type":"CREDIT_LIMIT","unit":3,"number":5,"usage":28000},
                    {"type":"CREDIT_LIMIT","unit":6,"number":1,"usage":140000,
                     "remaining":139000,"nextResetTime":1757000000000}
                ],
                "level": "max"
            }"#,
        )
        .expect("decode");
        let snapshot = snapshot_from_zai_quota(payload).expect("the weekly entry maps");
        assert!(
            snapshot.five_hour.is_none(),
            "an entry with no remaining must not map to 100% utilization",
        );
        assert!(snapshot.seven_day.is_some(), "its present sibling still maps");
    }

    /// Both an empty `limits` array and entries of some future
    /// non-CREDIT_LIMIT kind leave no mappable window: that is a
    /// response forge cannot read, not a bill of zero.
    #[test]
    fn zai_quota_with_no_mappable_entries_is_an_error_not_zero() {
        let empty: QuotaLimitData =
            serde_json::from_str(r#"{"limits":[],"level":"max"}"#).expect("decode");
        assert!(snapshot_from_zai_quota(empty).is_err(), "no windows must not read as a zero bill");
        let foreign_kind: QuotaLimitData = serde_json::from_str(
            r#"{"limits":[{"type":"TOKENS_LIMIT","unit":3,"number":5,"usage":1,"remaining":1}]}"#,
        )
        .expect("decode");
        assert!(
            snapshot_from_zai_quota(foreign_kind).is_err(),
            "a non-CREDIT_LIMIT entry must not become a window",
        );
    }
}
