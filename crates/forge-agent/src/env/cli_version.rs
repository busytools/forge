//! Detect the installed `claude` CLI version and (when reachable)
//! the latest published version on npm.
//!
//! The local probe shells out via [`forge_sdk::transport::process::query_cli_version`]
//! and normalises the result to a bare `MAJOR.MINOR.PATCH` token -
//! `claude --version` prints lines like `2.1.116 (anthropic)` or
//! `claude 2.1.116` depending on the build channel, so callers want
//! the version token alone for display.
//!
//! `CliVersionInfo` is the cross-crate shape returned by the
//! workspace mediator; both `installed` and `latest` are `Option`
//! because either probe can fail independently (`claude` not on
//! PATH, no network, npm rate-limited). The renderer treats `None`
//! as a ` - ` placeholder so the panel's row count stays constant.

use std::time::Duration;

/// Per-probe timeout (each of the two probes runs in parallel via
/// `tokio::join!` inside [`crate::env::cli_version::fetch_info`]).
/// 5 s is generous for a local `--version` invocation and still
/// well below the user-visible "app feels stuck" threshold for the
/// startup latency hit.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// npm registry endpoint for the published `@anthropic-ai/claude-code`
/// package. The `/latest` dist-tag returns the same JSON shape as the
/// full package document but trimmed to the current latest version,
/// which is all we need.
const NPM_LATEST_URL: &str = "https://registry.npmjs.org/@anthropic-ai/claude-code/latest";

/// Snapshot of the installed-vs-latest claude CLI versions. Either
/// side can be `None` independently; the renderer falls back to a
/// dim ` - ` placeholder for the missing side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliVersionInfo {
    /// Version reported by `claude --version` on the local machine,
    /// normalised to `MAJOR.MINOR.PATCH`. `None` when the binary
    /// isn't on PATH or the probe failed.
    pub installed: Option<String>,
    /// Latest version published on npm under the `latest` dist-tag.
    /// `None` when the registry probe failed (offline, rate-limit,
    /// timeout, …).
    pub latest: Option<String>,
}

impl CliVersionInfo {
    /// `true` when both probes succeeded AND the published version
    /// is strictly newer than the installed one. Comparison uses
    /// numeric-tuple semver: a non-parseable token on either side
    /// collapses to `false` (no update banner shown rather than a
    /// spurious one).
    pub fn has_update(&self) -> bool {
        match (self.installed.as_deref(), self.latest.as_deref()) {
            (Some(installed), Some(latest)) => is_strictly_newer(latest, installed),
            _ => false,
        }
    }
}

/// Run both probes in parallel and return the merged snapshot.
/// Always succeeds - failures collapse to `None` on the affected
/// field with a WARN log; the workspace caller never has to handle
/// a `Result`.
pub async fn fetch_info() -> CliVersionInfo {
    let (installed, latest) = tokio::join!(probe_installed(), probe_latest());
    CliVersionInfo { installed, latest }
}

/// Spawn `claude --version` on a blocking thread (the underlying
/// helper uses `std::process::Command`), bound the wait to
/// [`PROBE_TIMEOUT`], and normalise the output to a bare
/// `MAJOR.MINOR.PATCH` token. WARN log + `None` on any failure.
async fn probe_installed() -> Option<String> {
    let result = tokio::time::timeout(
        PROBE_TIMEOUT,
        tokio::task::spawn_blocking(|| forge_sdk::transport::process::query_cli_version("claude")),
    )
    .await;
    match result {
        Ok(Ok(Ok(reported))) => extract_semver_token(&reported).map(str::to_owned),
        Ok(Ok(Err(err))) => {
            tracing::warn!(
                target: crate::logging::targets::ENV_GIT,
                event_name = "cli_version_probe_failed",
                message = "claude --version probe failed",
                outcome = "failure",
                error = %err,
            );
            None
        }
        Ok(Err(join_err)) => {
            tracing::warn!(
                target: crate::logging::targets::ENV_GIT,
                event_name = "cli_version_probe_join_failed",
                message = "claude --version probe task panicked",
                outcome = "failure",
                error = %join_err,
            );
            None
        }
        Err(_) => {
            tracing::warn!(
                target: crate::logging::targets::ENV_GIT,
                event_name = "cli_version_probe_timeout",
                message = "claude --version probe timed out",
                outcome = "timeout",
            );
            None
        }
    }
}

/// Result of one npm-registry attempt. The type exists for the retry
/// decision: `Unreachable` never touched the server (retry direct),
/// `Unusable` reached it but the response was no good (retry won't
/// help).
enum ProbeOutcome {
    /// Parsed a version token from the response.
    Version(String),
    /// Connect / send error - the registry was never reached.
    Unreachable,
    /// Reached the registry, but the response was non-success, bad
    /// JSON, or missing the `version` field.
    Unusable,
}

/// GET npm's `/latest` dist-tag and return the newest published
/// `@anthropic-ai/claude-code` version as `MAJOR.MINOR.PATCH`.
///
/// The first attempt honours the ambient proxy env; if it can't reach
/// the registry (e.g. an exported `HTTPS_PROXY` pointing at a stopped
/// mitmproxy) it retries once via `.no_proxy()` so the non-sensitive
/// version check still lands over a direct connection. WARN-and-`None`
/// on any terminal failure.
async fn probe_latest() -> Option<String> {
    let client = build_probe_client(false)?;
    match attempt_latest(&client, false).await {
        ProbeOutcome::Version(v) => return Some(v),
        ProbeOutcome::Unusable => return None,
        ProbeOutcome::Unreachable => {}
    }
    let direct = build_probe_client(true)?;
    match attempt_latest(&direct, true).await {
        ProbeOutcome::Version(v) => Some(v),
        _ => None,
    }
}

/// Build the reqwest client for a single npm probe. `direct` adds
/// `.no_proxy()` so the request bypasses any ambient proxy env and
/// connects straight to the registry; both variants keep
/// `with_extra_roots` so a corporate / mitmproxy CA still validates.
fn build_probe_client(direct: bool) -> Option<reqwest::Client> {
    let mut builder =
        crate::http_trust::with_extra_roots(reqwest::Client::builder().timeout(PROBE_TIMEOUT));
    if direct {
        builder = builder.no_proxy();
    }
    match builder.build() {
        Ok(client) => Some(client),
        Err(err) => {
            tracing::warn!(
                target: crate::logging::targets::ENV_GIT,
                event_name = "cli_release_client_build_failed",
                message = "reqwest client build failed",
                outcome = "failure",
                direct,
                error = %err,
            );
            None
        }
    }
}

/// Run one GET against the npm registry with the given client,
/// classifying the result so the caller can decide whether a direct
/// retry is worthwhile.
async fn attempt_latest(client: &reqwest::Client, direct: bool) -> ProbeOutcome {
    let resp = match client.get(NPM_LATEST_URL).send().await {
        Ok(resp) => resp,
        Err(err) => {
            tracing::warn!(
                target: crate::logging::targets::ENV_GIT,
                event_name = "cli_release_fetch_failed",
                message = "npm registry GET failed",
                outcome = "failure",
                direct,
                error = %err,
            );
            return ProbeOutcome::Unreachable;
        }
    };
    if !resp.status().is_success() {
        tracing::warn!(
            target: crate::logging::targets::ENV_GIT,
            event_name = "cli_release_non_success",
            message = "npm registry returned non-success",
            outcome = "failure",
            direct,
            status = %resp.status(),
        );
        return ProbeOutcome::Unusable;
    }
    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(
                target: crate::logging::targets::ENV_GIT,
                event_name = "cli_release_parse_failed",
                message = "npm registry JSON parse failed",
                outcome = "failure",
                direct,
                error = %err,
            );
            return ProbeOutcome::Unusable;
        }
    };
    match parse_latest_version(&body) {
        Some(version) => ProbeOutcome::Version(version),
        None => ProbeOutcome::Unusable,
    }
}

/// Pull the `MAJOR.MINOR.PATCH` token from an npm `/latest` dist-tag
/// response body's `version` field.
fn parse_latest_version(body: &serde_json::Value) -> Option<String> {
    let version = body.get("version").and_then(|v| v.as_str())?;
    extract_semver_token(version).map(str::to_owned)
}

/// Extract the first whitespace-separated token that starts with a
/// digit - handles `2.1.116 (anthropic)`, `claude 2.1.116`, and
/// bare `2.1.116` shapes the CLI / npm produce.
fn extract_semver_token(reported: &str) -> Option<&str> {
    reported.split_whitespace().find(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()))
}

/// `true` when `lhs` parses as a strictly-greater semver triple than
/// `rhs`. A parse failure on either side collapses to `false` so the
/// update banner only fires when the comparison is unambiguous.
fn is_strictly_newer(lhs: &str, rhs: &str) -> bool {
    let Some(lhs_tuple) = parse_semver_triple(lhs) else { return false };
    let Some(rhs_tuple) = parse_semver_triple(rhs) else { return false };
    lhs_tuple > rhs_tuple
}

/// Parse `MAJOR.MINOR.PATCH` (ignoring any `-pre.1` / `+build` suffix
/// after the patch number) into a `(u32, u32, u32)` tuple suitable
/// for direct tuple comparison.
fn parse_semver_triple(s: &str) -> Option<(u32, u32, u32)> {
    let mut parts = s.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    // The patch component may carry a `-pre.1` / `+build` suffix -
    // strip everything from the first non-digit char.
    let patch_token = parts.next()?;
    let digits: String = patch_token.chars().take_while(char::is_ascii_digit).collect();
    let patch: u32 = digits.parse().ok()?;
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_token_handles_anthropic_suffix() {
        assert_eq!(extract_semver_token("2.1.116 (anthropic)"), Some("2.1.116"));
    }

    #[test]
    fn extract_token_handles_claude_prefix() {
        assert_eq!(extract_semver_token("claude 2.1.116"), Some("2.1.116"));
    }

    #[test]
    fn extract_token_handles_bare_version() {
        assert_eq!(extract_semver_token("2.1.116"), Some("2.1.116"));
    }

    #[test]
    fn extract_token_returns_none_on_no_digit_token() {
        assert!(extract_semver_token("not a version").is_none());
    }

    #[test]
    fn parse_semver_triple_basic() {
        assert_eq!(parse_semver_triple("2.1.116"), Some((2, 1, 116)));
    }

    #[test]
    fn parse_semver_triple_strips_pre_suffix() {
        assert_eq!(parse_semver_triple("2.1.116-pre.1"), Some((2, 1, 116)));
    }

    #[test]
    fn parse_semver_triple_strips_build_suffix() {
        assert_eq!(parse_semver_triple("2.1.116+abc"), Some((2, 1, 116)));
    }

    #[test]
    fn parse_semver_triple_rejects_too_short() {
        assert!(parse_semver_triple("2.1").is_none());
    }

    #[test]
    fn parse_semver_triple_rejects_non_numeric() {
        assert!(parse_semver_triple("v2.1.116").is_none());
    }

    #[test]
    fn has_update_when_latest_is_strictly_newer() {
        let info =
            CliVersionInfo { installed: Some("2.0.45".into()), latest: Some("2.0.50".into()) };
        assert!(info.has_update());
    }

    #[test]
    fn has_update_when_minor_bumps() {
        let info =
            CliVersionInfo { installed: Some("2.0.99".into()), latest: Some("2.1.0".into()) };
        assert!(info.has_update());
    }

    #[test]
    fn no_update_when_versions_match() {
        let info =
            CliVersionInfo { installed: Some("2.0.50".into()), latest: Some("2.0.50".into()) };
        assert!(!info.has_update());
    }

    #[test]
    fn no_update_when_installed_is_newer_dev_build() {
        let info =
            CliVersionInfo { installed: Some("2.1.0".into()), latest: Some("2.0.50".into()) };
        assert!(!info.has_update());
    }

    #[test]
    fn no_update_when_either_side_missing() {
        let info = CliVersionInfo { installed: None, latest: Some("2.0.50".into()) };
        assert!(!info.has_update());
        let info = CliVersionInfo { installed: Some("2.0.45".into()), latest: None };
        assert!(!info.has_update());
    }

    #[test]
    fn parse_latest_version_reads_npm_version_field() {
        let body = serde_json::json!({
            "name": "@anthropic-ai/claude-code",
            "version": "2.1.201",
        });
        assert_eq!(parse_latest_version(&body), Some("2.1.201".to_owned()));
    }

    #[test]
    fn parse_latest_version_none_when_field_missing() {
        let body = serde_json::json!({ "name": "@anthropic-ai/claude-code" });
        assert!(parse_latest_version(&body).is_none());
    }
}
