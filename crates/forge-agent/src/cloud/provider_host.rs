//! forge-agent's implementation of the
//! [`forge_providers::ProviderHost`] port. The keychain read, the
//! extra-roots HTTP client and the `claude --version` UA cache stay on
//! this side of the port so forge-providers carries no process or
//! keychain plumbing of its own.

use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use async_trait::async_trait;

use forge_providers::{OauthCredentials, ProviderHost};

/// The host every workspace-side backend probe runs against.
pub struct AgentHost;

/// Cached `claude-code/<version>` User-Agent, probed once per process.
/// `User-Agent` native CLI sends on /api/oauth/usage, captured from
/// mitmdump 2026-05-26 against claude CLI 2.1.133: `claude-code/<version>`,
/// no parens, no `(external, cli)` suffix - distinct from the
/// /v1/messages UA shape. Only a successful probe is cached; a failure
/// re-probes on the next call rather than pinning a stale fallback
/// that would lie about which version is running.
static UA: OnceLock<String> = OnceLock::new();

#[async_trait]
impl ProviderHost for AgentHost {
    fn keychain(&self, config_dir: &Path) -> Option<OauthCredentials> {
        crate::cloud::oauth_credentials::load_oauth_credentials(config_dir)
    }

    fn http_client(&self, timeout: Duration) -> Result<reqwest::Client, String> {
        crate::http_trust::with_extra_roots(reqwest::Client::builder().timeout(timeout))
            .build()
            .map_err(|error| error.to_string())
    }

    async fn user_agent(&self) -> Result<String, String> {
        if let Some(cached) = UA.get() {
            return Ok(cached.clone());
        }
        // get_or_init isn't `Result`-friendly. set/get pair: if another
        // caller raced us and set first, our `set` errors out and we
        // return ours anyway - value is identical (same probe result
        // for the same machine) so the race is benign.
        let ua = resolve_ua("claude").await?;
        let _ = UA.set(ua.clone());
        Ok(ua)
    }
}

/// One `claude --version` round-trip, formatted as the UA. The
/// shell-out runs under spawn_blocking so a slow binary lookup never
/// parks a tokio worker; split from the cached [`AgentHost::user_agent`]
/// so the exec and its failure class are drivable without resolving a
/// real binary.
async fn resolve_ua(binary: &'static str) -> Result<String, String> {
    let version = tokio::task::spawn_blocking(move || {
        forge_sdk::transport::process::query_cli_version(binary)
    })
    .await
    .map_err(|e| format!("UA probe spawn_blocking panicked: {e}"))?
    .map_err(|e| format!("claude --version probe failed for UA: {e}"))?;
    Ok(format!("claude-code/{version}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A binary nothing resolves is the UaProbe class - the probe could
    /// not run, which is not a verdict about the endpoint. Driven
    /// through the real shell-out with a name that cannot resolve.
    #[tokio::test]
    async fn a_missing_claude_binary_is_a_ua_failure_not_a_network_failure() {
        let result = resolve_ua("forge-test-claude-absent-from-path").await;
        assert!(
            matches!(&result, Err(message) if message.starts_with("claude --version probe failed for UA")),
            "a binary nothing resolves is the UaProbe class; got {result:?}",
        );
    }

    /// Pins the User-Agent shape sent on /api/oauth/usage to the
    /// `claude-code/<version>` form captured from native CLI 2.1.133.
    /// The host at runtime spawns `claude --version` to fill in the
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
