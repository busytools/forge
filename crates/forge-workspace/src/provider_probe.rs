//! Workspace-side routing of account probes through the
//! forge-providers backends, shared by the boot loader and the 60 s
//! usage poller.

use std::collections::HashMap;
use std::path::Path;
use std::time::SystemTime;

use forge_agent::cloud::AgentHost;
use forge_agent::cloud::oauth_credentials::{load_oauth_credentials, refresh_via_cli_spawn};
use forge_primitives::account::Provider;
use forge_primitives::usage::oauth::OauthUsageError;
use forge_providers::{AccountEnv, ProbeError, ProviderBackend, RepairAction, UsageSnapshot};

pub(crate) fn backend_for(provider: Provider) -> Result<&'static dyn ProviderBackend, ProbeError> {
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

/// Whether a failed first probe should move on to the CLI-spawn
/// refresh: a 401 - the one class a rotated keychain token repairs -
/// that the backend's repair policy actually routes to the keychain.
/// An env-bearer probe is never refresh-eligible: refreshing would
/// burn billed `claude -p hi` spawns against a credential the probe
/// never reads.
fn should_attempt_keychain_refresh(
    backend: &dyn ProviderBackend,
    account: &AccountEnv<'_>,
    first: &Result<UsageSnapshot, ProbeError>,
) -> bool {
    let Err(err) = first else { return false };
    matches!(err, ProbeError::Fetch(OauthUsageError::Unauthorized(_)))
        && backend.repair(account, err) == RepairAction::Refresh
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
    let backend = backend_for(provider)?;
    let account = AccountEnv { config_dir, env };
    let first = backend.probe(&account, &AgentHost).await;
    if !should_attempt_keychain_refresh(backend, &account, &first) {
        return first;
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry holds every token the anthropic-shaped arms can
    /// reach; a missing registration is the one error this module
    /// invents, and it must never surface for Anthropic.
    #[test]
    fn anthropic_backend_always_resolves() {
        assert!(backend_for(Provider::Anthropic).is_ok());
    }

    fn gate_account(env: &HashMap<String, String>) -> AccountEnv<'_> {
        AccountEnv { config_dir: Path::new("/tmp/unused"), env }
    }

    /// The refresh-gate pin: a 401 on a keychain-authenticated probe
    /// is the only shape that moves on to the CLI-spawn refresh. An
    /// env-bearer 401 (codex base url, anthropic setup token) stops
    /// here, and so does every non-auth failure class.
    #[test]
    fn only_a_keychain_401_is_refresh_eligible() {
        let unauthorized = Err(ProbeError::Fetch(OauthUsageError::Unauthorized(401)));
        let network = Err(ProbeError::Fetch(OauthUsageError::Network("dns".to_owned())));

        let empty = HashMap::new();
        let keychain = gate_account(&empty);
        let anthropic = backend_for(Provider::Anthropic).expect("registered");
        assert!(should_attempt_keychain_refresh(anthropic, &keychain, &unauthorized));
        assert!(!should_attempt_keychain_refresh(anthropic, &keychain, &network));

        let mut token = HashMap::new();
        token.insert("CLAUDE_CODE_OAUTH_TOKEN".to_owned(), "setup-token".to_owned());
        let token_mode = gate_account(&token);
        assert!(!should_attempt_keychain_refresh(anthropic, &token_mode, &unauthorized));

        let mut base = HashMap::new();
        base.insert("ANTHROPIC_BASE_URL".to_owned(), "http://localhost:18765".to_owned());
        base.insert("ANTHROPIC_AUTH_TOKEN".to_owned(), "sk-codex".to_owned());
        let base_acct = gate_account(&base);
        for provider in [Provider::Codex, Provider::Openrouter, Provider::Zai] {
            let resolved = backend_for(provider).expect("registered");
            assert!(
                !should_attempt_keychain_refresh(resolved, &base_acct, &unauthorized),
                "{provider:?}",
            );
        }
    }

    /// The production wiring through the real backend and host: an
    /// env-bearer codex probe's transport-class error comes back
    /// untouched - no refresh fired, no remapping. On a machine with
    /// the claude binary the local endpoint's 401 surfaces; a
    /// claude-less runner fails the UA step first, which surfaces the
    /// same way. The keychain read itself cannot be planted offline -
    /// it reads the real macOS keychain - so the no-refresh decision's
    /// teeth live in the test above.
    #[tokio::test]
    async fn an_env_bearer_codex_transport_error_surfaces_untouched() {
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
            let _ = sock.write_all(b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\n\r\n");
            let _ = sock.shutdown(std::net::Shutdown::Both);
        });
        let mut env = HashMap::new();
        env.insert("ANTHROPIC_BASE_URL".to_owned(), format!("http://{addr}"));
        env.insert("ANTHROPIC_AUTH_TOKEN".to_owned(), "sk-codex".to_owned());
        let result =
            probe_with_keychain_recovery(Provider::Codex, Path::new("/tmp/unused"), &env).await;
        assert!(
            matches!(
                result,
                Err(ProbeError::Fetch(
                    OauthUsageError::Unauthorized(401) | OauthUsageError::UaProbe(_)
                ))
            ),
            "the transport-class error must surface untouched; got {result:?}",
        );
    }
}
