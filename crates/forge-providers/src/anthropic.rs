//! The Anthropic backend: macOS keychain or setup-token credentials
//! against the default-host `/api/oauth/usage` endpoint. The keychain
//! path maps strictly (a 200 must carry the five-hour window); the
//! token path settles the endpoint's scope refusal and maps leniently.

use std::collections::HashMap;

use async_trait::async_trait;

use forge_primitives::usage::oauth::{OauthUsage, OauthUsageError};
use forge_primitives::usage::{UsageSnapshot, UsageSourceKind};

use crate::helpers::{
    OAUTH_TIMEOUT, anthropic_windowed_probe, map_extra_usage, map_window,
    snapshot_from_payload_lenient,
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
            Mapper::Lenient(bearer) => {
                let ua = host.user_agent().await.map_err(OauthUsageError::UaProbe)?;
                let client = host.http_client(OAUTH_TIMEOUT).map_err(OauthUsageError::Network)?;
                let settled = accept_scope_refusal(
                    anthropic_windowed_probe(&client, &ua, None, bearer).await,
                );
                match &settled {
                    Ok(_) => tracing::info!(
                        target: "forge_providers::anthropic",
                        event_name = "oauth_usage_setup_token_settled",
                        outcome = "ok",
                        "setup token usage probe settled",
                    ),
                    Err(OauthUsageError::Unauthorized(403)) => tracing::warn!(
                        target: "forge_providers::anthropic",
                        event_name = "oauth_usage_setup_token_unrecognized_403",
                        outcome = "non_ok",
                        "403 without the oauth_scope_insufficient shape: if the token was just \
                         re-minted, suspect a changed refusal body rather than a dead token",
                    ),
                    _ => {}
                }
                let payload = settled.map_err(ProbeError::Fetch)?;
                Ok(snapshot_from_payload_lenient(payload))
            }
            Mapper::Strict => {
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

/// The mapper an arm applies, paired with the credential that earns
/// it: the token arm maps leniently (the settled empty payload must
/// map), the keychain arm strictly (a 200 without the session window
/// is response-shape drift). Pure so the routing stays unit-pinned -
/// routing the seven-day-only shape to the strict mapper is a bug that
/// has shipped before and flipped accounts to fetch errors every 5h
/// cycle.
fn choose_mapper(token: Option<&str>) -> Mapper<'_> {
    match token {
        Some(bearer) => Mapper::Lenient(bearer),
        None => Mapper::Strict,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Mapper<'a> {
    Lenient(&'a str),
    Strict,
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
    use std::time::Duration;

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

    /// The arm-routing pin: a token credential earns the lenient
    /// mapper, the keychain earns the strict one. Inverting this sent
    /// the seven-day-only shape to the strict mapper and flipped
    /// accounts to fetch errors every 5h cycle.
    #[test]
    fn token_bearer_earns_the_lenient_mapper_and_keychain_the_strict() {
        assert_eq!(choose_mapper(Some("tok")), Mapper::Lenient("tok"));
        assert_eq!(choose_mapper(None), Mapper::Strict);
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
