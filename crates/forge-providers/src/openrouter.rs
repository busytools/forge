//! The OpenRouter backend: the per-key spend probe against
//! `{base}/v1/key`, plus the public `/v1/models` catalog behind the
//! [`ModelCatalog`] half. The configured base url already ends in
//! `/api` (that is what the chat API wants), so only the `/v1/...`
//! tails are appended; appending the documented site-relative paths
//! yields `/api/api/...`, which 404s.

use std::time::SystemTime;

use async_trait::async_trait;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue};

use forge_primitives::usage::oauth::OauthUsageError;
use forge_primitives::usage::openrouter::KeyResponse;
use forge_primitives::usage::{ApiSpend, UsageSnapshot, UsageSourceKind};

use crate::helpers::{
    BaseUrlCredential, MissingBase, OAUTH_TIMEOUT, base_url_credential, parse_retry_after,
    truncated_body_suffix,
};
use crate::model_catalog::{
    self, CATALOG_TIMEOUT, CachedCatalog, CatalogDecision, CatalogModel, ModelCatalog,
    ModelCatalogError,
};
use crate::{AccountEnv, BillingModel, ProbeError, Provider, ProviderBackend, ProviderHost};

/// The OpenRouter `[[accounts]] provider` token.
pub struct Openrouter;

#[async_trait]
impl ProviderBackend for Openrouter {
    fn token(&self) -> Provider {
        Provider::Openrouter
    }

    fn billing(&self) -> BillingModel {
        BillingModel::Spend
    }

    fn source(&self) -> UsageSourceKind {
        UsageSourceKind::OpenRouterKey
    }

    fn model_catalog(&self) -> Option<&'static dyn ModelCatalog> {
        Some(&Openrouter)
    }

    async fn probe(
        &self,
        account: &AccountEnv<'_>,
        host: &dyn ProviderHost,
    ) -> Result<UsageSnapshot, ProbeError> {
        match choose_mapper(base_url_credential(account.env)) {
            Mapper::Spend(credential) => {
                let client = host.http_client(OAUTH_TIMEOUT).map_err(OauthUsageError::Network)?;
                let payload = key_probe(&client, &credential.base_url, &credential.bearer)
                    .await
                    .map_err(ProbeError::Fetch)?;
                snapshot_from_openrouter_key(payload)
            }
            Mapper::MissingBase(missing) => Err(ProbeError::Unmappable(missing.to_string())),
        }
    }
}

/// The mapper the account's env earns, paired with the credential that
/// earns it: the base-url pair earns the spend mapper, and a missing
/// base url carries its error to the probe's Unmappable surface. Pure
/// so the routing stays unit-pinned - the probe cannot be driven
/// against the network offline.
fn choose_mapper(credential: Result<BaseUrlCredential, MissingBase>) -> Mapper {
    match credential {
        Ok(credential) => Mapper::Spend(credential),
        Err(missing) => Mapper::MissingBase(missing),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Mapper {
    /// The credential bound to the spend mapper: the arm holding this
    /// cannot map windows or read the keychain.
    Spend(BaseUrlCredential),
    MissingBase(MissingBase),
}

/// OpenRouter's per-key endpoint: `{base}/v1/key`, trailing slash
/// trimmed so base and base/ behave identically.
fn key_url(base_url: &str) -> String {
    format!("{}/v1/key", base_url.trim_end_matches('/'))
}

#[async_trait]
impl ModelCatalog for Openrouter {
    async fn fetch(
        &self,
        base_url: &str,
        host: &dyn ProviderHost,
    ) -> Result<Vec<CatalogModel>, ModelCatalogError> {
        let client = host.http_client(CATALOG_TIMEOUT).map_err(ModelCatalogError::Network)?;
        model_catalog::fetch_catalog(&client, base_url).await
    }

    fn curated(&self, models: &[CatalogModel]) -> Vec<forge_primitives::AvailableModel> {
        model_catalog::curated_available_models(models)
    }

    fn decision(&self, cached: Option<CachedCatalog>, now: SystemTime) -> CatalogDecision {
        model_catalog::catalog_decision(cached, now)
    }
}

/// One round-trip against `{base_url}/v1/key` for a pay-per-token
/// account. Shares [`OauthUsageError`] with the window probes so the
/// loader and poller classify a 401 / 429 / network failure the same
/// way regardless of billing kind.
async fn key_probe(
    client: &reqwest::Client,
    base_url: &str,
    bearer: &str,
) -> Result<KeyResponse, OauthUsageError> {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    let auth = HeaderValue::from_str(&format!("Bearer {bearer}"))
        .map_err(|error| OauthUsageError::Network(format!("bad bearer header: {error}")))?;
    headers.insert(AUTHORIZATION, auth);

    let response = client
        .get(key_url(base_url))
        .headers(headers)
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
            target: "forge_providers::openrouter",
            event_name = "openrouter_key_response",
            status,
            outcome = "ok",
            body_bytes = body.len(),
        );
    } else {
        tracing::warn!(
            target: "forge_providers::openrouter",
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
                target: "forge_providers::openrouter",
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

/// Map an OpenRouter key payload into a snapshot. Window-free: a
/// pay-per-token key has no plan window and, when uncapped, no
/// denominator, so nothing here synthesises a utilization.
///
/// Fallible on purpose. A 200 whose body carries no `data` envelope, or
/// an envelope with none of the three usage figures, is a response
/// forge cannot read rather than a bill of zero - and since `set_usage`
/// takes any snapshot to `Ready` without inspecting it, mapping those
/// to zeroes would report a confident number nothing prompts anyone to
/// doubt. An absent figure alongside a present sibling is a real zero
/// and maps as one.
fn snapshot_from_openrouter_key(payload: KeyResponse) -> Result<UsageSnapshot, ProbeError> {
    let Some(data) = payload.data else {
        return Err(ProbeError::Unmappable(
            "OpenRouter key response carried no data envelope.".to_owned(),
        ));
    };
    if data.usage_daily.is_none() && data.usage_weekly.is_none() && data.usage_monthly.is_none() {
        return Err(ProbeError::Unmappable(
            "OpenRouter key response carried no usage figures.".to_owned(),
        ));
    }
    Ok(UsageSnapshot {
        source: UsageSourceKind::OpenRouterKey,
        fetched_at: SystemTime::now(),
        five_hour: None,
        seven_day: None,
        seven_day_opus: None,
        seven_day_sonnet: None,
        extra_usage: None,
        spend: Some(ApiSpend {
            daily: data.usage_daily.unwrap_or(0.0),
            weekly: data.usage_weekly.unwrap_or(0.0),
            monthly: data.usage_monthly.unwrap_or(0.0),
            // Carried through as-is: a cap that is absent stays absent
            // rather than becoming a zero, because zero is a cap that
            // permits nothing and absent is a key with no cap at all.
            limit: data.limit,
            limit_remaining: data.limit_remaining,
            limit_reset: data.limit_reset,
            expires_at: data.expires_at,
        }),
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
        env.insert("ANTHROPIC_AUTH_TOKEN".to_owned(), "sk-or-test".to_owned());
        env
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
    fn openrouter_backend_is_spend() {
        assert_eq!(Openrouter.token(), Provider::Openrouter);
        assert_eq!(Openrouter.billing(), BillingModel::Spend);
    }

    /// The arm-routing pin: a base-url credential earns the spend
    /// mapper carrying that credential; a missing base earns the
    /// Unmappable arm. Wiring the pair to a window mapper, or probing
    /// with a credential the env did not produce, cannot compile.
    #[test]
    fn a_base_url_credential_earns_the_spend_mapper_and_a_missing_base_the_error_arm() {
        let credential =
            base_url_credential(&env_with_base("https://openrouter.ai/api")).expect("credential");
        assert_eq!(
            choose_mapper(Ok(credential.clone())),
            Mapper::Spend(credential),
            "the spend arm runs with the credential that earned it",
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
        let backend = Openrouter;
        let account = AccountEnv { config_dir: Path::new("/tmp/unused"), env: &HashMap::new() };
        let result = backend.probe(&account, &UnreachableHost).await;
        assert!(matches!(result, Err(ProbeError::Unmappable(_))), "got {result:?}");
    }

    /// The production wiring through the real backend and host: a 200
    /// key body round-trips into the spend snapshot, on the
    /// Bearer-prefixed env token. Proves the injected client, the
    /// per-request headers and the status classification, which the
    /// pure mapper pins cannot see.
    #[tokio::test]
    async fn a_200_key_body_probes_through_to_a_spend_snapshot() {
        let body = br#"{"data":{"usage_daily":0.25}}"#;
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
                request_text.contains("GET /v1/key HTTP/1.1"),
                "the request must hit /v1/key, got: {}",
                request_text.lines().next().unwrap_or_default()
            );
            assert!(
                request_text.contains("authorization: Bearer sk-or-test"),
                "the env token goes out Bearer-prefixed, got: {request_text}",
            );
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(header.as_bytes());
            let _ = sock.write_all(body);
            let _ = sock.shutdown(std::net::Shutdown::Both);
        });
        let env = env_with_base(&format!("http://{addr}"));
        let account = AccountEnv { config_dir: Path::new("/tmp/unused"), env: &env };
        let snapshot = Openrouter.probe(&account, &LocalHost).await.expect("snapshot");
        let spend = snapshot.spend.expect("spend");
        assert!((spend.daily - 0.25).abs() < f64::EPSILON, "got {spend:?}");
    }

    struct LocalHost;

    #[async_trait]
    impl ProviderHost for LocalHost {
        fn keychain(&self, _config_dir: &Path) -> Option<crate::OauthCredentials> {
            unreachable!("the openrouter probe never reads the keychain")
        }

        fn http_client(&self, timeout: Duration) -> Result<reqwest::Client, String> {
            reqwest::Client::builder().timeout(timeout).build().map_err(|e| e.to_string())
        }

        async fn user_agent(&self) -> Result<String, String> {
            unreachable!("the openrouter probe sends no User-Agent header")
        }
    }

    struct UnreachableHost;

    #[async_trait]
    impl ProviderHost for UnreachableHost {
        fn keychain(&self, _config_dir: &Path) -> Option<crate::OauthCredentials> {
            unreachable!("the openrouter probe never reads the keychain")
        }

        fn http_client(&self, _timeout: Duration) -> Result<reqwest::Client, String> {
            unreachable!("the probe must not build a client for a missing base url")
        }

        async fn user_agent(&self) -> Result<String, String> {
            unreachable!("the probe must not resolve a UA for a missing base url")
        }
    }

    /// The verified after-usage shape maps to spend with no windows:
    /// an API-billed account fabricates neither a window nor a
    /// percentage, and extra_usage stays unmapped (it is Anthropic
    /// overage in minor units, not this).
    #[test]
    fn openrouter_key_maps_to_spend_with_no_windows() {
        let payload: KeyResponse = serde_json::from_str(
            r#"{"data":{
                "label":"sk-or-v1-TEST...TEST",
                "limit":null,"limit_reset":null,"limit_remaining":null,
                "usage":198.552152461,
                "usage_daily":0.5632267,
                "usage_weekly":1.25,
                "usage_monthly":20.296155711,
                "byok_usage":0.000365,
                "is_free_tier":false
            }}"#,
        )
        .expect("decode");
        let snapshot = snapshot_from_openrouter_key(payload).expect("a real payload maps");

        assert_eq!(snapshot.source, UsageSourceKind::OpenRouterKey);
        let spend = snapshot.spend.expect("spend is populated");
        assert!(
            (spend.daily - 0.563_226_7).abs() < f64::EPSILON,
            "daily spend comes straight off usage_daily",
        );
        assert!((spend.weekly - 1.25).abs() < f64::EPSILON, "weekly spend maps");
        assert!((spend.monthly - 20.296_155_711).abs() < f64::EPSILON, "monthly spend maps");
        assert!(
            snapshot.five_hour.is_none()
                && snapshot.seven_day.is_none()
                && snapshot.seven_day_opus.is_none()
                && snapshot.seven_day_sonnet.is_none(),
            "an API-billed account has no plan window to fabricate",
        );
        assert!(
            snapshot.extra_usage.is_none(),
            "extra_usage is Anthropic overage in minor units, not this",
        );
    }

    /// A 200 whose body forge cannot read is not a zero bill. Both
    /// shapes below used to decode to a confident (0.0, 0.0, 0.0) and
    /// take the account Ready, with the only log line saying success.
    #[test]
    fn an_unreadable_openrouter_body_is_an_error_not_zero_spend() {
        let no_envelope: KeyResponse =
            serde_json::from_str(r#"{"error":{"message":"User not found.","code":401}}"#)
                .expect("decodes structurally");
        assert!(
            snapshot_from_openrouter_key(no_envelope).is_err(),
            "a body with no data envelope carries no spend and must not read as zero",
        );

        let no_figures: KeyResponse =
            serde_json::from_str(r#"{"data":{"label":"sk-or-v1-TEST","is_free_tier":false}}"#)
                .expect("decodes structurally");
        assert!(
            snapshot_from_openrouter_key(no_figures).is_err(),
            "an envelope with none of the three usage figures must not read as zero",
        );
    }

    /// A cap can be added or removed from the provider's dashboard
    /// between polls, so both shapes have to map: the capped key
    /// carries a denominator the panel can draw a bar against, the
    /// uncapped one carries none and must not be given a synthesised
    /// zero to stand in for it.
    #[test]
    fn a_cap_maps_when_present_and_stays_absent_when_not() {
        let capped: KeyResponse = serde_json::from_str(
            r#"{"data":{"usage_daily":0.038869563,"usage_weekly":0.038869563,
                        "usage_monthly":0.038869563,"limit":20,
                        "limit_remaining":19.961130437,"limit_reset":"monthly",
                        "expires_at":null}}"#,
        )
        .expect("decode");
        let spend = snapshot_from_openrouter_key(capped).expect("maps").spend.expect("spend");
        assert_eq!(spend.limit, Some(20.0), "the cap is the denominator a bar needs");
        assert_eq!(spend.limit_remaining, Some(19.961_130_437), "what is left to spend");
        assert_eq!(spend.limit_reset.as_deref(), Some("monthly"), "the window the cap resets on");
        assert_eq!(spend.expires_at, None, "a key with no expiry reports none");

        let uncapped: KeyResponse = serde_json::from_str(
            r#"{"data":{"usage_daily":0.56,"usage_weekly":1.25,"usage_monthly":20.30,
                        "limit":null,"limit_remaining":null,"limit_reset":null}}"#,
        )
        .expect("decode");
        let spend = snapshot_from_openrouter_key(uncapped).expect("maps").spend.expect("spend");
        assert_eq!(spend.limit, None, "an uncapped key has no denominator to invent");
        assert_eq!(spend.limit_remaining, None);
        assert_eq!(spend.limit_reset, None);
        assert!((spend.monthly - 20.30).abs() < f64::EPSILON, "spend still maps without a cap");
    }

    /// One figure present is a real report; its absent siblings are
    /// genuinely zero, which is what the endpoint means by omitting a
    /// figure that has a sibling.
    #[test]
    fn a_partial_openrouter_body_maps_its_present_figure() {
        let partial: KeyResponse =
            serde_json::from_str(r#"{"data":{"usage_daily":0.25}}"#).expect("decode");
        let spend = snapshot_from_openrouter_key(partial)
            .expect("one present figure is a readable report")
            .spend
            .expect("spend");
        assert!((spend.daily - 0.25).abs() < f64::EPSILON, "the present figure maps");
        assert!(
            spend.weekly.abs() < f64::EPSILON && spend.monthly.abs() < f64::EPSILON,
            "absent siblings of a present figure are zero",
        );
    }
}
