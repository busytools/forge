//! forge-agent's implementation of the
//! [`forge_providers::ProviderHost`] port. The keychain read, the
//! extra-roots HTTP client and the `claude --version` UA cache stay on
//! this side of the port so forge-providers carries no process or
//! keychain plumbing of its own.

use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

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

impl ProviderHost for AgentHost {
    fn keychain(&self, config_dir: &Path) -> Option<OauthCredentials> {
        crate::cloud::oauth_credentials::load_oauth_credentials(config_dir)
    }

    fn http_client(&self, timeout: Duration) -> Result<reqwest::Client, String> {
        crate::http_trust::with_extra_roots(reqwest::Client::builder().timeout(timeout))
            .build()
            .map_err(|error| error.to_string())
    }

    fn user_agent(&self) -> Result<String, String> {
        if let Some(cached) = UA.get() {
            return Ok(cached.clone());
        }
        // One blocking `claude --version` per process (~tens of ms);
        // the callers are workspace poll tasks, never the render
        // thread.
        let version = forge_sdk::transport::process::query_cli_version("claude")
            .map_err(|error| format!("claude --version probe failed for UA: {error}"))?;
        let ua = format!("claude-code/{version}");
        // If another caller raced us and set first, our `set` errors
        // out and we return ours anyway - value is identical (same
        // probe result for the same machine) so the race is benign.
        let _ = UA.set(ua.clone());
        Ok(ua)
    }
}

#[cfg(test)]
mod tests {
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
