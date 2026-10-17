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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    probe_installed_within(PROBE_TIMEOUT).await
}

/// [`probe_installed`] with the wait as a parameter, so a test driving a
/// real child can widen the budget rather than race machine load.
async fn probe_installed_within(budget: Duration) -> Option<String> {
    let result = tokio::time::timeout(
        budget,
        tokio::task::spawn_blocking(|| forge_sdk::transport::process::query_cli_version("claude")),
    )
    .await;
    match result {
        Ok(Ok(Ok(reported))) => reported_version_or_warn(&reported),
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

/// Normalise a zero-exit `claude --version` stdout to a bare version
/// token, WARN + `None` when it carries none - a silent `None` here is
/// indistinguishable from a probe that never ran.
fn reported_version_or_warn(reported: &str) -> Option<String> {
    let Some(token) = extract_semver_token(reported) else {
        tracing::warn!(
            target: crate::logging::targets::ENV_GIT,
            event_name = "cli_version_probe_unparseable",
            message = "claude --version output carried no version token",
            outcome = "failure",
            output = %reported,
        );
        return None;
    };
    Some(token.to_owned())
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
    match latest_version_or_warn(&body, direct) {
        Some(version) => ProbeOutcome::Version(version),
        None => ProbeOutcome::Unusable,
    }
}

/// Normalise an npm `/latest` response body to a version token, WARN +
/// `None` when it carries none - the same silent-collapse shape as the
/// installed probe's miss.
fn latest_version_or_warn(body: &serde_json::Value, direct: bool) -> Option<String> {
    let Some(version) = parse_latest_version(body) else {
        tracing::warn!(
            target: crate::logging::targets::ENV_GIT,
            event_name = "cli_release_version_unparseable",
            message = "npm registry response carried no version token",
            outcome = "failure",
            direct,
            version = ?body.get("version"),
        );
        return None;
    };
    Some(version)
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

    /// The npm side of the same defect: a `/latest` response carrying no
    /// parseable version token must warn rather than read as a registry
    /// that was never reached. The warn sits in the helper the HTTP path
    /// calls, because reaching the registry is that path's own concern.
    #[test]
    fn an_unusable_npm_latest_body_warns_and_yields_none() {
        let body = serde_json::json!({
            "name": "@anthropic-ai/claude-code",
            "version": "not-a-version",
        });
        let (result, captured) = capture_logs(|| latest_version_or_warn(&body, false));
        assert_eq!(result, None, "an unusable body yields None: {captured:?}");
        assert!(
            captured.contains("cli_release_version_unparseable"),
            "the miss fired no warn: {captured:?}",
        );
        assert!(
            captured.contains("not-a-version"),
            "the warn must carry the field's value - the diagnostic: {captured:?}",
        );
    }

    /// The converse half, or a warn-everything regression passes both
    /// miss tests: a readable output is not flagged.
    #[test]
    fn readable_outputs_are_not_flagged() {
        let (installed, captured) =
            capture_logs(|| reported_version_or_warn("2.1.116 (anthropic)"));
        assert_eq!(installed.as_deref(), Some("2.1.116"), "no token from a readable output");
        assert!(
            !captured.contains("cli_version_probe_unparseable"),
            "warned on a readable version: {captured:?}",
        );

        let body = serde_json::json!({ "version": "2.1.201" });
        let (latest, captured) = capture_logs(|| latest_version_or_warn(&body, false));
        assert_eq!(latest.as_deref(), Some("2.1.201"), "no token from a readable body");
        assert!(
            !captured.contains("cli_release_version_unparseable"),
            "warned on a readable body: {captured:?}",
        );
    }

    /// Buffer tracing output so an emitted record can be read back.
    #[derive(Clone, Default)]
    struct LogCapture(std::sync::Arc<parking_lot::Mutex<Vec<u8>>>);

    impl std::io::Write for LogCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn capture_logs<R>(f: impl FnOnce() -> R) -> (R, String) {
        let capture = LogCapture::default();
        let subscriber = tracing_subscriber::fmt().with_writer(capture.clone()).finish();
        let result = tracing::subscriber::with_default(subscriber, f);
        let text = String::from_utf8_lossy(&capture.0.lock().clone()).into_owned();
        (result, text)
    }

    /// The defect #1205 names, end to end: a zero-exit `claude --version`
    /// whose output carries no version token must warn rather than
    /// collapse to a silent `None`. PATH cannot be injected in-process
    /// (process-global, and `unsafe` to set under this edition), so the
    /// test re-execs this binary as a child whose whole PATH is a stub
    /// dir, and asserts the child's captured log. The child drives the
    /// probe with a widened budget so machine load cannot spend it before
    /// the stub answers.
    #[test]
    fn a_zero_exit_unreadable_version_warns() {
        const PROBE: &str = "FORGE_CLI_VERSION_PROBE";
        const PROBE_OK: &str = "FORGE_CLI_VERSION_PROBE_OK";
        const PROBE_TEST: &str = "env::cli_version::tests::a_zero_exit_unreadable_version_warns";

        let stub_dir = tempfile::tempdir().expect("tempdir");
        let stub = stub_dir.path().join("claude");
        std::fs::write(&stub, "#!/bin/sh\necho 'unknown build'\n").expect("write stub");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
                .expect("chmod stub");
        }

        if std::env::var(PROBE).is_err() {
            // Parent mode: re-run this test as a child whose PATH holds
            // only the stub dir, so `claude` resolves to the stub.
            let out = std::process::Command::new(std::env::current_exe().expect("current_exe"))
                .args(["--exact", PROBE_TEST, "--test-threads=1", "--nocapture"])
                .env(PROBE, "1")
                .env("PATH", stub_dir.path())
                .output()
                .expect("spawn probe");
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(
                out.status.success(),
                "the child probe failed: stdout={stdout}\nstderr={stderr}",
            );
            assert!(
                stdout.contains(PROBE_OK),
                "the probe never ran (filter matched nothing?): stdout={stdout}\nstderr={stderr}",
            );
            return;
        }

        // Child mode. `unknown build` appears only in the stub's stdout,
        // so a capture carrying it proves the probe ran the stub rather
        // than a real `claude`; `cli_version_probe_failed`'s absence
        // proves it reached the exit-0 arm rather than a spawn failure.
        let (installed, captured) = capture_logs(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime")
                .block_on(probe_installed_within(std::time::Duration::from_secs(60)))
        });
        assert_eq!(
            installed, None,
            "an unreadable `--version` output still yields None: {captured:?}",
        );
        assert!(
            captured.contains("cli_version_probe_unparseable"),
            "the miss fired no warn, which is the defect: {captured:?}",
        );
        assert!(
            captured.contains("unknown build"),
            "the warn must carry the raw output - the only diagnostic: {captured:?}",
        );
        assert!(
            !captured.contains("cli_version_probe_failed"),
            "the probe must have read the stub's output, not failed to spawn it: {captured:?}",
        );
        println!("{PROBE_OK}");
    }
}
