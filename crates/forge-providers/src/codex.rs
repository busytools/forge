//! The Codex backend: a base-url proxy serving Anthropic's windowed
//! `/api/oauth/usage` shape, authenticated by the `[accounts.env]`
//! `ANTHROPIC_BASE_URL` + `ANTHROPIC_AUTH_TOKEN` pair. The response
//! always maps leniently - a proxy emits each window on its own and
//! `{}` is the cold steady state, not a malformed response.

use async_trait::async_trait;

use forge_primitives::usage::oauth::OauthUsageError;
use forge_primitives::usage::{UsageSnapshot, UsageSourceKind};

use crate::helpers::{
    BaseUrlCredential, MissingBase, OAUTH_TIMEOUT, anthropic_windowed_probe, base_url_credential,
    snapshot_from_payload_lenient,
};
use crate::{AccountEnv, BillingModel, ProbeError, Provider, ProviderBackend, ProviderHost};

/// The Codex `[[accounts]] provider` token.
pub struct Codex;

#[async_trait]
impl ProviderBackend for Codex {
    fn token(&self) -> Provider {
        Provider::Codex
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
        match choose_mapper(base_url_credential(account.env)) {
            Mapper::Lenient(credential) => {
                let ua = host.user_agent().await.map_err(OauthUsageError::UaProbe)?;
                let client = host.http_client(OAUTH_TIMEOUT).map_err(OauthUsageError::Network)?;
                let payload = anthropic_windowed_probe(
                    &client,
                    &ua,
                    Some(&credential.base_url),
                    &credential.bearer,
                )
                .await
                .map_err(ProbeError::Fetch)?;
                Ok(snapshot_from_payload_lenient(payload))
            }
            Mapper::MissingBase(missing) => Err(ProbeError::Unmappable(missing.to_string())),
        }
    }
}

/// The mapper the account's env earns, paired with the credential that
/// earns it: the base-url pair always maps leniently, and a missing
/// base url carries its error to the probe's Unmappable surface. Pure
/// so the routing stays unit-pinned - the probe cannot be driven
/// against the network offline.
fn choose_mapper(credential: Result<BaseUrlCredential, MissingBase>) -> Mapper {
    match credential {
        Ok(credential) => Mapper::Lenient(credential),
        Err(missing) => Mapper::MissingBase(missing),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Mapper {
    /// The credential bound to the lenient mapper: the arm holding
    /// this cannot map strictly or read another credential.
    Lenient(BaseUrlCredential),
    MissingBase(MissingBase),
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
        env.insert("ANTHROPIC_AUTH_TOKEN".to_owned(), "sk-codex".to_owned());
        env
    }

    /// The arm-routing pin: a base-url credential earns the lenient
    /// mapper carrying that credential; a missing base earns the
    /// Unmappable arm. Wiring the pair to a strict mapper, or probing
    /// with a credential the env did not produce, cannot compile.
    #[test]
    fn a_base_url_credential_earns_the_lenient_mapper_and_a_missing_base_the_error_arm() {
        let credential =
            base_url_credential(&env_with_base("http://localhost:18765")).expect("credential");
        assert_eq!(
            choose_mapper(Ok(credential.clone())),
            Mapper::Lenient(credential),
            "the lenient arm runs with the credential that earned it",
        );
        assert!(
            matches!(choose_mapper(Err(MissingBase)), Mapper::MissingBase(_)),
            "a missing base surfaces Unmappable, not a probe against a default host",
        );
    }

    #[test]
    fn codex_backend_is_windowed() {
        assert_eq!(Codex.token(), Provider::Codex);
        assert_eq!(Codex.billing(), BillingModel::Windows);
    }

    /// A host that cannot resolve the UA surfaces the UaProbe class -
    /// a local exec problem, not a verdict about the endpoint - so the
    /// callers' retry path engages.
    #[tokio::test]
    async fn a_host_ua_failure_is_a_ua_probe_error_not_a_network_failure() {
        let backend = Codex;
        let env = env_with_base("http://localhost:18765");
        let account = AccountEnv { config_dir: Path::new("/tmp/unused"), env: &env };
        let result = backend.probe(&account, &FailingUaHost).await;
        assert!(
            matches!(result, Err(ProbeError::Fetch(OauthUsageError::UaProbe(_)))),
            "got {result:?}",
        );
    }

    /// A missing base url never reaches the network: the error
    /// surfaces before a client or UA is built.
    #[tokio::test]
    async fn a_missing_base_is_unmappable_without_probing() {
        let backend = Codex;
        let account = AccountEnv { config_dir: Path::new("/tmp/unused"), env: &HashMap::new() };
        let result = backend.probe(&account, &UnreachableHost).await;
        assert!(matches!(result, Err(ProbeError::Unmappable(_))), "got {result:?}");
    }

    struct FailingUaHost;

    #[async_trait]
    impl ProviderHost for FailingUaHost {
        fn keychain(&self, _config_dir: &Path) -> Option<crate::OauthCredentials> {
            unreachable!("the codex probe never reads the keychain")
        }

        fn http_client(&self, _timeout: Duration) -> Result<reqwest::Client, String> {
            reqwest::Client::builder().build().map_err(|e| e.to_string())
        }

        async fn user_agent(&self) -> Result<String, String> {
            Err("claude missing from PATH".to_owned())
        }
    }

    struct UnreachableHost;

    #[async_trait]
    impl ProviderHost for UnreachableHost {
        fn keychain(&self, _config_dir: &Path) -> Option<crate::OauthCredentials> {
            unreachable!("the codex probe never reads the keychain")
        }

        fn http_client(&self, _timeout: Duration) -> Result<reqwest::Client, String> {
            unreachable!("the probe must not build a client for a missing base url")
        }

        async fn user_agent(&self) -> Result<String, String> {
            unreachable!("the probe must not resolve a UA for a missing base url")
        }
    }
}
