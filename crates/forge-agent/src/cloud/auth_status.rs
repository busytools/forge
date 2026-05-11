//! `claude auth status` shell-out — the CLI's only source of truth
//! for the user's full account profile (email, org, subscription
//! tier).
//!
//! In claude 2.1.117 the `system/init` stream-json frame does **not**
//! include an `account` block. The frame carries `apiKeySource` at the
//! top level (e.g. `"none"`, `"oauth"`, `"user"`, …) and that's it —
//! everything else (email, organization, subscription) lives behind
//! the `claude auth status` subcommand. The JS-side SDK fetches the
//! same data via its own `query.accountInfo()` RPC; for forge the
//! cheapest equivalent is to spawn `claude auth status` and parse its
//! JSON output. Adds ~50ms latency for the first read.
//!
//! Lifted from forge-sdk in 2026-05-05. Shell-outs to `claude` are
//! agent-side concerns — the SDK now only owns the long-lived
//! stream-json subprocess. Mirrors the shape of
//! `userdata::plugins::cli`, which wraps `claude plugin` in the same
//! way.
//!
//! Returns a fully populated [`AccountInfo`] when the shell-out
//! succeeds, mapping `claude auth status`'s camelCase JSON into the
//! `snake_case` struct.

use std::path::Path;

use serde::Deserialize;

use forge_primitives::AccountInfo;

/// `claude auth status` JSON response. Captured from claude 2.1.117.
/// Field shape may evolve; we treat all fields as optional and ignore
/// unknown ones.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeAuthStatus {
    /// Whether the user is currently authenticated.
    #[serde(default)]
    logged_in: bool,
    /// e.g. `"claude.ai"` for OAuth, `"anthropic-api-key"` for API
    /// key auth, …
    auth_method: Option<String>,
    /// e.g. `"firstParty"`, `"bedrock"`, `"vertex"`, …
    api_provider: Option<String>,
    email: Option<String>,
    #[serde(rename = "orgId")]
    _org_id: Option<String>,
    org_name: Option<String>,
    subscription_type: Option<String>,
}

/// Map `auth_method` values from `claude auth status` to the legacy
/// `api_key_source` enum the TUI's status page renders. The TUI's
/// `login_method_label` distinguishes between `oauth`, `user`,
/// `project`, `org`, `temporary` — anything else falls through to
/// the raw string.
fn map_auth_method_to_api_key_source(auth_method: &str) -> &str {
    match auth_method {
        "claude.ai" => "oauth",
        "anthropic-api-key" => "user",
        other => other,
    }
}

/// Shell out to `claude auth status` and parse its JSON output into
/// [`AccountInfo`]. The `config_dir` is exported to the subprocess as
/// `CLAUDE_CONFIG_DIR` so the spawned `claude` reads the bound
/// account; the caller is the source of truth for which account this
/// is.
///
/// Returns `None` when:
///
/// - the `claude` binary is not on `$PATH`,
/// - the subprocess exits non-zero,
/// - the JSON is malformed,
/// - or `loggedIn` is `false`.
///
/// The fields are mapped:
/// - `email` → `email`
/// - `orgName` → `organization`
/// - `subscriptionType` → `subscription_type`
/// - `apiProvider` → `api_provider`
/// - `authMethod` → both `token_source` (raw) **and** `api_key_source`
///   (translated via the private `map_auth_method_to_api_key_source` helper)
///
/// Synchronous; runs the subprocess inline. ~50ms first call, faster
/// thereafter (claude warms up its keychain reads in-process).
#[must_use]
pub fn account_info_from_shell(config_dir: &Path) -> Option<AccountInfo> {
    let mut cmd = std::process::Command::new("claude");
    cmd.args(["auth", "status"]);
    cmd.env("CLAUDE_CONFIG_DIR", config_dir);
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    parse_auth_status(&output.stdout)
}

fn parse_auth_status(stdout: &[u8]) -> Option<AccountInfo> {
    let parsed: ClaudeAuthStatus = serde_json::from_slice(stdout).ok()?;
    if !parsed.logged_in {
        return None;
    }
    let api_key_source =
        parsed.auth_method.as_deref().map(|m| map_auth_method_to_api_key_source(m).to_owned());
    Some(AccountInfo {
        email: parsed.email,
        organization: parsed.org_name,
        subscription_type: parsed.subscription_type,
        token_source: parsed.auth_method,
        api_key_source,
        api_provider: parsed.api_provider,
    })
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn parses_full_oauth_status() {
        let stdout = br#"{
            "loggedIn": true,
            "authMethod": "claude.ai",
            "apiProvider": "firstParty",
            "email": "dev@example.com",
            "orgId": "f0a43d2e-a197-4a9f-94af-7ab882844c4d",
            "orgName": "Example Org",
            "subscriptionType": "team"
        }"#;
        let info = parse_auth_status(stdout).expect("parsed");
        assert_eq!(info.email.as_deref(), Some("dev@example.com"));
        assert_eq!(info.organization.as_deref(), Some("Example Org"));
        assert_eq!(info.subscription_type.as_deref(), Some("team"));
        assert_eq!(info.api_provider.as_deref(), Some("firstParty"));
        assert_eq!(info.api_key_source.as_deref(), Some("oauth"));
        assert_eq!(info.token_source.as_deref(), Some("claude.ai"));
    }

    #[test]
    fn parses_anthropic_api_key_status() {
        let stdout = br#"{
            "loggedIn": true,
            "authMethod": "anthropic-api-key",
            "apiProvider": "firstParty"
        }"#;
        let info = parse_auth_status(stdout).expect("parsed");
        assert_eq!(info.api_key_source.as_deref(), Some("user"));
        assert_eq!(info.token_source.as_deref(), Some("anthropic-api-key"));
        assert!(info.email.is_none());
    }

    #[test]
    fn returns_none_when_logged_out() {
        let stdout = br#"{"loggedIn": false}"#;
        assert!(parse_auth_status(stdout).is_none());
    }

    #[test]
    fn returns_none_for_malformed_json() {
        assert!(parse_auth_status(b"not json").is_none());
    }

    #[test]
    fn passes_through_unknown_auth_method() {
        let stdout = br#"{
            "loggedIn": true,
            "authMethod": "future-auth-3000"
        }"#;
        let info = parse_auth_status(stdout).expect("parsed");
        assert_eq!(info.api_key_source.as_deref(), Some("future-auth-3000"));
        assert_eq!(info.token_source.as_deref(), Some("future-auth-3000"));
    }
}
