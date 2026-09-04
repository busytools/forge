//! Workspace-side routing of account probes through the
//! forge-providers backends, shared by the boot loader and the 60 s
//! usage poller.

use std::collections::HashMap;
use std::path::Path;
use std::time::SystemTime;

use forge_agent::cloud::AgentHost;
use forge_agent::cloud::oauth::OauthFetchError;
use forge_agent::cloud::oauth_credentials::{load_oauth_credentials, refresh_via_cli_spawn};
use forge_agent::cloud::oauth_usage::is_token_mode;
use forge_primitives::account::Provider;
use forge_primitives::usage::oauth::OauthUsageError;
use forge_providers::{AccountEnv, ProbeError, ProviderBackend, UsageSnapshot};

fn backend_for(provider: Provider) -> Result<&'static dyn ProviderBackend, ProbeError> {
    if let Some(backend) = forge_providers::backend(provider) {
        return Ok(backend);
    }
    debug_assert!(
        false,
        "every provider the probe arms reach must have a backend registered; {provider:?}"
    );
    Err(ProbeError::Unmappable(format!("no backend registered for {provider:?}")))
}

/// One probe round-trip through the account's backend and the shared
/// host.
pub(crate) async fn probe_via_backend(
    provider: Provider,
    config_dir: &Path,
    env: &HashMap<String, String>,
) -> Result<UsageSnapshot, ProbeError> {
    let backend = backend_for(provider)?;
    let account_env = AccountEnv { config_dir, env };
    backend.probe(&account_env, &AgentHost).await
}

/// Whether the account's probe authenticates from `[accounts.env]`
/// rather than the keychain: an env-bearer auth failure must never
/// fire the keychain CLI-spawn refresh, which burns billed
/// `claude -p hi` spawns against a token the probe never reads. The
/// base-url providers authenticate from env by definition; a
/// token-mode anthropic account carries its setup token there.
pub(crate) fn env_bearer(provider: Provider, env: &HashMap<String, String>) -> bool {
    provider.uses_base_url() || is_token_mode(env)
}

/// The 60 s poller's keychain recovery, over the backend: on a 401
/// whose keychain token is locally expired (or undated), fire the
/// CLI-spawn refresh once and re-probe; any refresh failure surfaces
/// the original 401. The boot loader does not take this path - its own
/// Refresh action handles recovery.
pub(crate) async fn probe_with_keychain_recovery(
    provider: Provider,
    config_dir: &Path,
    env: &HashMap<String, String>,
) -> Result<UsageSnapshot, ProbeError> {
    let first = probe_via_backend(provider, config_dir, env).await;
    // The refresh can only repair a keychain-authenticated probe.
    if env_bearer(provider, env) {
        return first;
    }
    let Err(ProbeError::Fetch(OauthUsageError::Unauthorized(_))) = &first else {
        return first;
    };
    // Treating an absent expires_at as expired is deliberate: refresh
    // is one-shot per account (the per-account mutex prevents a probe
    // storm), and surfacing 401 forever because an older claude write
    // omitted the field is worse than one refresh against a blob
    // whose expiry nobody recorded.
    let expired = load_oauth_credentials(config_dir)
        .is_some_and(|creds| creds.expires_at.is_none_or(|t| t < SystemTime::now()));
    if !expired {
        return first;
    }
    match refresh_via_cli_spawn(config_dir).await {
        Ok(_) => probe_via_backend(provider, config_dir, env).await,
        Err(refresh_err) => {
            tracing::warn!(
                target: "forge_workspace::provider_probe",
                event_name = "oauth_usage_refresh_failed",
                config_dir = %config_dir.display(),
                error = %refresh_err,
                "refresh attempt did not produce fresh creds; surfacing original Unauthorized",
            );
            first
        }
    }
}

/// Fold a backend result into the (mapping, transport) pair the
/// loader and poller predate, so their shared branches keep
/// classifying and retrying exactly as before.
pub(crate) fn flatten_probe_error(
    result: Result<UsageSnapshot, ProbeError>,
) -> Result<Result<UsageSnapshot, OauthFetchError>, OauthUsageError> {
    match result {
        Ok(snapshot) => Ok(Ok(snapshot)),
        Err(ProbeError::Fetch(err)) => Err(err),
        Err(ProbeError::NoCredentials) => Err(OauthUsageError::NoCredentials),
        Err(ProbeError::Unmappable(message)) => Ok(Err(OauthFetchError::Failed(message))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flatten_keeps_the_transport_classes_the_loader_branches_on() {
        let transport = flatten_probe_error(Err(ProbeError::Fetch(OauthUsageError::RateLimited {
            retry_after: None,
        })));
        assert!(
            matches!(transport, Err(OauthUsageError::RateLimited { retry_after: None })),
            "wire classes must reach boot_probe_action unchanged, got {transport:?}",
        );

        let no_creds = flatten_probe_error(Err(ProbeError::NoCredentials));
        assert!(
            matches!(no_creds, Err(OauthUsageError::NoCredentials)),
            "the loader's Refresh decision keys on NoCredentials; got {no_creds:?}",
        );
    }

    #[test]
    fn flatten_reports_an_unmappable_200_as_a_mapping_failure_not_transport() {
        let mapped = flatten_probe_error(Err(ProbeError::Unmappable("no window".to_owned())));
        assert!(
            matches!(
                &mapped,
                Ok(Err(OauthFetchError::Failed(message))) if message == "no window",
            ),
            "a 200 that maps to nothing retries instead of bailing; got {mapped:?}",
        );
    }

    /// The registry holds every token the anthropic-shaped arms can
    /// reach; a missing registration is the one error this module
    /// invents, and it must never surface for Anthropic.
    #[test]
    fn anthropic_backend_always_resolves() {
        assert!(backend_for(Provider::Anthropic).is_ok());
    }

    /// The repair-class pin: every non-keychain credential source
    /// counts as env-bearer, so its auth failures terminal instead of
    /// firing billed keychain refreshes. Only keychain-mode anthropic
    /// is refreshable.
    #[test]
    fn env_bearer_covers_every_non_keychain_credential_source() {
        assert!(!env_bearer(Provider::Anthropic, &HashMap::new()));

        let mut token = HashMap::new();
        token.insert("CLAUDE_CODE_OAUTH_TOKEN".to_owned(), "setup-token".to_owned());
        assert!(env_bearer(Provider::Anthropic, &token));

        for provider in [Provider::Codex, Provider::Openrouter, Provider::Zai] {
            assert!(env_bearer(provider, &HashMap::new()), "{provider:?}");
        }
    }
}
