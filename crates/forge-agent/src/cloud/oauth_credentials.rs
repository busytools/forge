//! OAuth bearer-credential reader.
//!
//! Reads the access token the `claude` CLI persisted for an account.
//! On macOS the source is the system keychain entry
//! `Claude Code-credentials-<sha256-prefix>` where `<sha256-prefix>`
//! is the first 8 lowercase hex chars of `SHA-256(<config_dir-as-string>)`.
//! The file path (`<config_dir>/.credentials.json`) is deliberately
//! NOT read: claude 2.1.117+ writes to keychain and the file is
//! stale-by-construction for accounts that have been refreshed at
//! least once, which made forge surface stale tokens that the upstream
//! API immediately rejected as `Unauthorized`. On non-macOS targets
//! the loader returns `None` unconditionally (forge is macOS-only in
//! practice; the cfg gate keeps the crate compiling on other targets
//! without dragging in a Linux-keyring shim).
//!
//! Two readers turn them into a Bearer header: the forge-providers
//! Anthropic and Codex backends, via
//! [`crate::cloud::provider_host::AgentHost`]. The codex backend skips
//! this loader entirely, building its credential from
//! `ANTHROPIC_AUTH_TOKEN` in `[accounts.env]` instead. The others:
//! [`refresh_via_cli_spawn`] below, before and after its spawn;
//! `ForgeSdkBridge` (through [`session_oauth_credentials`], which
//! routes token-mode accounts to their env token); and
//! forge-workspace's usage-poll 401 gate.
//!
//! When the keychain token is past its `expires_at` and the live probe
//! returns 401, callers can fire [`refresh_via_cli_spawn`] to nudge the
//! claude CLI into rotating the keychain entry on the user's behalf.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use parking_lot::Mutex;
#[cfg(target_os = "macos")]
use serde_json::Value;

use crate::cloud::auth_status;
#[cfg(target_os = "macos")]
use forge_providers::helpers::parse_timestamp_value;

pub use forge_primitives::cloud::oauth_credentials::OauthCredentials;

/// Read + parse the user's OAuth credentials against an explicit
/// `config_dir`. The caller (typically a `ForgeSdkBridge`) is the
/// source of truth for which account's credentials to read; there is
/// no fallback to a process-env-derived path.
///
/// On macOS the source is the system keychain entry
/// `Claude Code-credentials-<sha256-prefix>`; on non-macOS targets
/// this returns `None`.
///
/// Returns `None` when the keychain entry is missing, unreadable, or
/// lacks a parseable, non-empty `claudeAiOauth.accessToken`.
/// `expires_at` is `None` when the payload omits the field; otherwise
/// it is the parsed [`std::time::SystemTime`] of the `expiresAt`
/// numeric or stringified-numeric epoch.
pub fn load_oauth_credentials(config_dir: &Path) -> Option<OauthCredentials> {
    #[cfg(target_os = "macos")]
    {
        load_oauth_credentials_from_keychain(config_dir)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = config_dir;
        None
    }
}

/// `[accounts.env]` key carrying a per-account setup token (minted by
/// `claude setup-token`). Its presence makes the account token-mode:
/// this token is the credential, never the keychain entry for the
/// account's config dir.
const CLAUDE_CODE_OAUTH_TOKEN_ENV: &str = "CLAUDE_CODE_OAUTH_TOKEN";

/// The setup token `env` carries, trimmed and non-empty - the same
/// read the usage probe makes, so the snapshot reports the credential
/// the session actually authenticates with.
fn env_setup_token<S: std::hash::BuildHasher>(env: &HashMap<String, String, S>) -> Option<&str> {
    env.get(CLAUDE_CODE_OAUTH_TOKEN_ENV)
        .map(String::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
}

/// The credential a session's OAuth snapshot reports. A token-mode
/// account's credential is the setup token in its env - the keychain
/// entry for its (shared) config dir belongs to whichever sibling
/// logged in interactively - so the env token IS the snapshot, with no
/// locally-known expiry. Every other account reads its config dir's
/// keychain.
pub fn session_oauth_credentials<S: std::hash::BuildHasher>(
    config_dir: &Path,
    env: &HashMap<String, String, S>,
) -> Option<OauthCredentials> {
    match env_setup_token(env) {
        Some(token) => Some(OauthCredentials { access_token: token.to_owned(), expires_at: None }),
        None => load_oauth_credentials(config_dir),
    }
}

/// Hard timeout for the `claude -p "hi"` refresh spawn. The CLI's
/// own login + keychain-write path is sub-second when the OAuth
/// refresh token is still valid; 10 s absorbs slow network or cold
/// keychain access without keeping the bottom-panel bar stale for a
/// noticeable stretch.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(10);

/// Outcomes from a refresh attempt. The workspace callers treat any
/// error as "fall through to the existing `Unauthorized`
/// surface" - the cache-invalidation pathway from #237-A picks up
/// after 3 consecutive strikes and the bottom panel flips to the
/// "`⚠ unauthorized - /login`" label.
#[derive(Debug, thiserror::Error)]
pub enum RefreshError {
    /// `claude auth status` reports `loggedIn=false` (or fails to
    /// run / parse). The account has no live refresh token; user
    /// must `/login` interactively. Short-circuits before mutex
    /// acquire and spawn so we don't waste a billed API call.
    #[error("claude auth status reports the account is not logged in")]
    NotLoggedIn,
    /// `tokio::process::Command::output()` failed (binary missing,
    /// permission denied, IO error). The error string is the
    /// underlying `io::Error` rendered.
    #[error("spawning `claude -p hi` failed: {0}")]
    SpawnFailed(String),
    /// The spawn ran past the internal refresh timeout (10 s).
    #[error("`claude -p hi` did not return within {}s", REFRESH_TIMEOUT.as_secs())]
    Timeout,
    /// `claude -p hi` exited with a non-zero status. Carries the
    /// status code when one is available.
    #[error("`claude -p hi` exited with status {0:?}")]
    ExitNonZero(Option<i32>),
    /// The refresh ran successfully but the re-read keychain still
    /// reports an expired (or absent) token. Suggests the CLI's
    /// own refresh path is broken; falls through to the upstream
    /// `Unauthorized` surface.
    #[error("keychain still reports an expired token after refresh")]
    KeychainStillExpiredAfterRefresh,
}

/// Stable per-account hex prefix used to name the refresh-spawn tmp
/// directory + the claude-CLI-sanitised project dir. Mirrors the
/// `keychain_service_name` pattern (SHA-256 first 8 hex chars) so the
/// two derivations stay in lockstep - any future probe-name change
/// can update both helpers together.
fn account_id_for_tmp(config_dir: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(config_dir.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    hex::encode(&digest[..4])
}

/// Fixed tmp directory the refresh spawn runs in. One per account so
/// concurrent refreshes for different accounts don't share state, but
/// a single account's repeated refreshes all reuse the same path -
/// the pre-spawn sweep cleans leftover JSONLs from the previous run
/// so the directory's footprint stays bounded over time.
fn tmp_path_for(account_id: &str) -> PathBuf {
    PathBuf::from(format!("/tmp/forge-refresh-{account_id}"))
}

/// Where claude writes the spawn's session JSONL: it sanitises the
/// cwd into a directory name under `<config_dir>/projects/` by
/// replacing slashes with dashes. The tmp path `/tmp/forge-refresh-X`
/// sanitises to `-tmp-forge-refresh-X` (leading slash + interior
/// slashes both become dashes; hex chars in the account id need no
/// escaping).
fn sanitized_project_dir(config_dir: &Path, account_id: &str) -> PathBuf {
    config_dir.join("projects").join(format!("-tmp-forge-refresh-{account_id}"))
}

/// Per-account inner mutex registry. The outer `parking_lot::Mutex`
/// guards entry creation in the map; the inner `tokio::sync::Mutex<()>`
/// is what callers actually hold while spawning + re-reading the
/// keychain. Two callers refreshing the same account run sequentially
/// (the second observes the first's fresh-write via the double-check
/// and returns without a second spawn); callers refreshing different
/// accounts run independently.
fn per_account_locks() -> &'static Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn per_account_mutex(config_dir: &Path) -> Arc<tokio::sync::Mutex<()>> {
    let mut map = per_account_locks().lock();
    map.entry(config_dir.to_path_buf())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// Best-effort sweep: remove any `.jsonl` files in `dir`. Keeps the
/// directory itself so the next refresh runs against a clean slate
/// without `mkdir`-ing. Silent on missing dir / unreadable entries -
/// the next spawn will recreate state if claude needs it.
fn sweep_stale_jsonls(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.path().extension().is_some_and(|ext| ext == "jsonl") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Spawn a brief `claude -p "hi"` invocation against `config_dir` to
/// nudge the claude CLI into refreshing its OAuth token. Returns the
/// re-read [`OauthCredentials`] on success, or a [`RefreshError`]
/// classifying the failure.
///
/// Trigger contract: the workspace refresh callers invoke this
/// only when the live `/api/oauth/usage` probe returned 401 AND the
/// cached `credentials.expires_at` is in the past - i.e. the local
/// view of the token agrees with the server's verdict that the token
/// is dead. Other 401 causes (revoked token, scope mismatch, network
/// proxy interception) fall through to the existing Unauthorized
/// surface; #237-A's cache-invalidation handles the renderer side.
///
/// Concurrency: per-account `tokio::sync::Mutex` serialises refresh
/// attempts; concurrent callers for the same account wait on the
/// inner lock, then observe the first caller's fresh keychain write
/// via a double-check and return without spawning a second CLI.
///
/// Side effects:
/// - Creates `/tmp/forge-refresh-<account_id>/` (idempotent).
/// - Removes any leftover `.jsonl` files in
///   `<config_dir>/projects/-tmp-forge-refresh-<account_id>/` from
///   prior refreshes. Bounded state - one account's repeated
///   refreshes don't grow the project dir over time.
/// - Spawns one `claude -p "hi"` per refresh attempt. This is a real
///   billed API call (one short turn). Lead has accepted the cost.
pub async fn refresh_via_cli_spawn(config_dir: &Path) -> Result<OauthCredentials, RefreshError> {
    // Pre-gate. `account_info_from_shell` returns None when
    // loggedIn=false, the binary is missing, or the JSON fails to
    // parse. Any of those means we can't refresh - short-circuit
    // before mutex acquire so a logged-out account doesn't burn the
    // ~50 ms shell-out plus the spawn-attempt cost.
    if auth_status::account_info_from_shell(config_dir).is_none() {
        return Err(RefreshError::NotLoggedIn);
    }

    let lock = per_account_mutex(config_dir);
    let _guard = lock.lock().await;

    // Double-check inside the critical section: another caller may
    // have refreshed between our pre-gate and lock acquire. Returning
    // the already-fresh creds skips a redundant spawn.
    if let Some(creds) = load_oauth_credentials(config_dir)
        && creds.expires_at.is_some_and(|t| t > std::time::SystemTime::now())
    {
        return Ok(creds);
    }

    let account_id = account_id_for_tmp(config_dir);
    let tmp_dir = tmp_path_for(&account_id);
    let project_dir = sanitized_project_dir(config_dir, &account_id);

    std::fs::create_dir_all(&tmp_dir)
        .map_err(|e| RefreshError::SpawnFailed(format!("mkdir {}: {e}", tmp_dir.display())))?;
    sweep_stale_jsonls(&project_dir);

    let mut cmd = tokio::process::Command::new("claude");
    cmd.args(["-p", "hi"]);
    cmd.env("CLAUDE_CONFIG_DIR", config_dir);
    cmd.current_dir(&tmp_dir);
    // kill_on_drop = true so dropping the future on a timeout actually
    // terminates the child. Without it, `tokio::time::timeout` drops
    // the future but the child keeps running in the background: a
    // mid-keychain-write at the 10 s mark would land async after we
    // already surfaced Unauthorized (user sees the error despite a
    // fresh entry arriving seconds later), and two concurrent timed-
    // out refreshes could leave racing CLI processes both writing the
    // same keychain entry.
    cmd.kill_on_drop(true);
    let started = std::time::Instant::now();
    let output = if let Ok(result) = tokio::time::timeout(REFRESH_TIMEOUT, cmd.output()).await {
        result.map_err(|e| RefreshError::SpawnFailed(e.to_string()))?
    } else {
        tracing::warn!(
            target: crate::logging::targets::OAUTH_CREDENTIALS,
            event_name = "oauth_refresh_timeout",
            config_dir = %config_dir.display(),
            elapsed_secs = started.elapsed().as_secs_f64(),
            timeout_secs = REFRESH_TIMEOUT.as_secs(),
            "refresh spawn killed after timeout",
        );
        return Err(RefreshError::Timeout);
    };
    if !output.status.success() {
        return Err(RefreshError::ExitNonZero(output.status.code()));
    }

    let new_creds =
        load_oauth_credentials(config_dir).ok_or(RefreshError::KeychainStillExpiredAfterRefresh)?;
    if new_creds.expires_at.is_some_and(|t| t <= std::time::SystemTime::now()) {
        return Err(RefreshError::KeychainStillExpiredAfterRefresh);
    }
    Ok(new_creds)
}

/// Service name the macOS keychain stores Claude Code credentials
/// under. Encoded as `Claude Code-credentials-<sha256-prefix>` where
/// `<sha256-prefix>` is the first 8 lowercase hex characters of the
/// SHA-256 hash of the `<config_dir>` path *as a string*. Same scheme
/// the official `claude` CLI uses: verified against the entries CLI
/// 2.1.117 had actually written, for several `~/.claude-<profile>`
/// dirs at once.
#[cfg(target_os = "macos")]
fn keychain_service_name(config_dir: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(config_dir.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let prefix = hex::encode(&digest[..4]);
    format!("Claude Code-credentials-{prefix}")
}

/// Read + parse OAuth credentials from the macOS keychain. Shells out
/// to `security find-generic-password -s <service> -w`, parses the
/// returned password as the same `claudeAiOauth` JSON shape the
/// on-disk credentials file uses.
#[cfg(target_os = "macos")]
fn load_oauth_credentials_from_keychain(config_dir: &Path) -> Option<OauthCredentials> {
    let service = keychain_service_name(config_dir);
    let output = match std::process::Command::new("security")
        .args(["find-generic-password", "-s", service.as_str(), "-w"])
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            tracing::debug!(
                target: crate::logging::targets::OAUTH_CREDENTIALS,
                error = %e,
                service = %service,
                "keychain shell-out failed (security CLI missing?)",
            );
            return None;
        }
    };
    if !output.status.success() {
        // Common: keychain entry doesn't exist for this service. Not
        // a bug, but logged at debug (not trace) so a fresh-install
        // user filing "credentials lookup failed" can see the
        // breadcrumb without flipping env-filter to trace.
        tracing::debug!(
            target: crate::logging::targets::OAUTH_CREDENTIALS,
            exit = ?output.status.code(),
            service = %service,
            "keychain entry missing (typical on first login)",
        );
        return None;
    }
    let password = match String::from_utf8(output.stdout) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(
                target: crate::logging::targets::OAUTH_CREDENTIALS,
                error = %e,
                "keychain payload was not valid UTF-8",
            );
            return None;
        }
    };
    let trimmed = password.trim_end_matches(['\r', '\n']);
    let json = match serde_json::from_str::<Value>(trimmed) {
        Ok(j) => j,
        Err(e) => {
            tracing::debug!(
                target: crate::logging::targets::OAUTH_CREDENTIALS,
                error = %e,
                "keychain payload was not valid JSON (corrupt entry?)",
            );
            return None;
        }
    };
    parse_oauth_credentials(&json)
}

#[cfg(target_os = "macos")]
fn parse_oauth_credentials(json: &Value) -> Option<OauthCredentials> {
    let Some(oauth) = json.get("claudeAiOauth") else {
        tracing::debug!(
            target: crate::logging::targets::OAUTH_CREDENTIALS,
            "credentials JSON has no `claudeAiOauth` key (schema mismatch / wrong shape)",
        );
        return None;
    };
    let Some(access_token_value) = oauth.get("accessToken") else {
        tracing::debug!(
            target: crate::logging::targets::OAUTH_CREDENTIALS,
            "credentials.claudeAiOauth has no `accessToken` field",
        );
        return None;
    };
    let Some(access_token) = access_token_value.as_str() else {
        // Log the variant name only - `Display` on `serde_json::Value`
        // would emit the entire JSON content, which could leak
        // sensitive data if a future schema put a non-string token
        // here.
        let kind = if access_token_value.is_number() {
            "number"
        } else if access_token_value.is_array() {
            "array"
        } else if access_token_value.is_object() {
            "object"
        } else if access_token_value.is_boolean() {
            "boolean"
        } else if access_token_value.is_null() {
            "null"
        } else {
            "unknown"
        };
        tracing::debug!(
            target: crate::logging::targets::OAUTH_CREDENTIALS,
            kind,
            "credentials.claudeAiOauth.accessToken is not a string",
        );
        return None;
    };
    let access_token = access_token.trim();
    if access_token.is_empty() {
        tracing::debug!(
            target: crate::logging::targets::OAUTH_CREDENTIALS,
            "credentials.claudeAiOauth.accessToken is empty after trim",
        );
        return None;
    }

    let expires_at = oauth.get("expiresAt").and_then(parse_timestamp_value);
    Some(OauthCredentials { access_token: access_token.to_owned(), expires_at })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parser-surface tests are gated on macOS because the
    /// `parse_oauth_credentials` helper itself is only compiled on
    /// macOS - it has no caller on non-macos targets (the file source
    /// is gone; the keychain source doesn't exist outside macOS), so
    /// the function is `#[cfg(target_os = "macos")]` to satisfy
    /// `-D dead_code`. Tests gated the same way keep the symbol's
    /// availability symmetric with its definition.
    #[cfg(target_os = "macos")]
    mod parser {
        use super::*;
        use std::time::{Duration, UNIX_EPOCH};

        fn parse_str(json: &str) -> Option<OauthCredentials> {
            let value: Value = serde_json::from_str(json).ok()?;
            parse_oauth_credentials(&value)
        }

        #[test]
        fn returns_none_for_empty_json() {
            assert!(parse_str("{}").is_none());
        }

        #[test]
        fn returns_none_for_missing_oauth_block() {
            assert!(parse_str(r#"{"somethingElse":true}"#).is_none());
        }

        #[test]
        fn returns_none_for_empty_access_token() {
            let json = r#"{"claudeAiOauth":{"accessToken":"","refreshToken":"tok"}}"#;
            assert!(parse_str(json).is_none());
        }

        #[test]
        fn returns_none_for_non_string_access_token() {
            let json = r#"{"claudeAiOauth":{"accessToken":42}}"#;
            assert!(parse_str(json).is_none());
        }

        #[test]
        fn returns_credentials_for_valid_oauth() {
            let json = r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-test","refreshToken":"sk-ant-ort01-test","expiresAt":9999999999999}}"#;
            let credentials = parse_str(json).expect("credentials");
            assert_eq!(credentials.access_token, "sk-ant-oat01-test");
            assert!(credentials.expires_at.is_some());
        }

        #[test]
        fn parses_expiry_in_seconds() {
            let json = r#"{"claudeAiOauth":{"accessToken":"token","expiresAt":1}}"#;
            let credentials = parse_str(json).expect("credentials");
            assert_eq!(credentials.expires_at, Some(UNIX_EPOCH + Duration::from_secs(1)));
        }

        #[test]
        fn parses_expiry_in_milliseconds() {
            let json = r#"{"claudeAiOauth":{"accessToken":"token","expiresAt":1700000000001}}"#;
            let credentials = parse_str(json).expect("credentials");
            assert_eq!(
                credentials.expires_at,
                Some(UNIX_EPOCH + Duration::from_millis(1_700_000_000_001))
            );
        }

        #[test]
        fn parses_expiry_string_form() {
            let json = r#"{"claudeAiOauth":{"accessToken":"token","expiresAt":"42"}}"#;
            let credentials = parse_str(json).expect("credentials");
            assert_eq!(credentials.expires_at, Some(UNIX_EPOCH + Duration::from_secs(42)));
        }

        #[test]
        fn returns_none_for_malformed_json() {
            assert!(parse_str("not json at all").is_none());
        }
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn keychain_service_name_uses_sha256_prefix_of_config_dir_string() {
        // The scheme itself was confirmed against real CLI 2.1.117
        // keychain entries; these pins are recomputed for paths that are
        // safe to commit, so they hold the derivation still rather than
        // re-proving the match.
        assert_eq!(
            keychain_service_name(Path::new("/Users/developer/.claude-one")),
            "Claude Code-credentials-4a7e8760"
        );
        assert_eq!(
            keychain_service_name(Path::new("/Users/developer/.claude-two")),
            "Claude Code-credentials-2edc62e9"
        );
        assert_eq!(
            keychain_service_name(Path::new("/Users/developer/.claude-three")),
            "Claude Code-credentials-c465c265"
        );
        assert_eq!(
            keychain_service_name(Path::new("/Users/developer/.claude")),
            "Claude Code-credentials-6f5e8f91"
        );
    }

    /// `account_id_for_tmp` shares the SHA-256-prefix derivation with
    /// `keychain_service_name` so the keychain entry and the tmp/spawn
    /// state share an identifier. If a future change re-bases either
    /// derivation the other must move in lockstep; this test pins the
    /// equality.
    #[test]
    fn account_id_for_tmp_matches_keychain_service_name_suffix() {
        let dirs = [
            "/Users/developer/.claude-one",
            "/Users/developer/.claude-two",
            "/Users/developer/.claude-three",
            "/Users/developer/.claude",
        ];
        for dir in dirs {
            let path = Path::new(dir);
            let id = account_id_for_tmp(path);
            assert_eq!(id.chars().count(), 8, "hex prefix is 8 chars: {id}");
            #[cfg(target_os = "macos")]
            assert!(
                keychain_service_name(path).ends_with(&id),
                "keychain service name must end with account_id_for_tmp output (dir={dir} id={id})",
            );
        }
    }

    #[test]
    fn tmp_path_for_uses_fixed_per_account_path() {
        let p1 = tmp_path_for("abcd1234");
        let p2 = tmp_path_for("abcd1234");
        assert_eq!(p1, p2, "same account_id => same fixed path");
        assert_eq!(p1, PathBuf::from("/tmp/forge-refresh-abcd1234"));
    }

    #[test]
    fn sanitized_project_dir_uses_dashes_for_slash() {
        let dir = sanitized_project_dir(Path::new("/Users/me/.claude"), "deadbeef");
        assert_eq!(dir, PathBuf::from("/Users/me/.claude/projects/-tmp-forge-refresh-deadbeef"),);
    }

    #[test]
    fn sweep_stale_jsonls_removes_jsonl_keeps_other_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let jsonl = tmp.path().join("session-a.jsonl");
        let txt = tmp.path().join("readme.txt");
        std::fs::write(&jsonl, b"old session").expect("write jsonl");
        std::fs::write(&txt, b"untouched").expect("write txt");

        sweep_stale_jsonls(tmp.path());

        assert!(!jsonl.exists(), ".jsonl swept");
        assert!(txt.exists(), "other extensions preserved");
        assert!(tmp.path().exists(), "directory itself preserved");
    }

    #[test]
    fn sweep_stale_jsonls_silent_on_missing_dir() {
        // Must not panic when the dir doesn't exist - the post-spawn
        // sweep can run before the first refresh has created the dir.
        let missing = PathBuf::from("/tmp/forge-refresh-test-nonexistent-dir-zzzz");
        // Just verifying no panic.
        sweep_stale_jsonls(&missing);
    }

    /// The stub dir has no keychain entry, so `None` from this test
    /// would mean the keychain was read - which is exactly what a
    /// token-mode session must not do: the shared entry belongs to
    /// whichever sibling logged in interactively.
    #[test]
    fn a_token_session_snapshots_its_env_token_not_the_shared_keychain() {
        let mut env = HashMap::new();
        env.insert("CLAUDE_CODE_OAUTH_TOKEN".to_owned(), "  setup-token  ".to_owned());
        let credentials = session_oauth_credentials(Path::new("/tmp/forge-testing-stub"), &env);
        assert_eq!(
            credentials,
            Some(OauthCredentials { access_token: "setup-token".to_owned(), expires_at: None }),
            "the env token IS the session's credential, with no locally-known expiry",
        );
    }

    #[test]
    fn a_keychain_session_snapshots_the_config_dir_keychain() {
        let credentials =
            session_oauth_credentials(Path::new("/tmp/forge-testing-stub"), &HashMap::new());
        assert_eq!(credentials, None, "the stub dir has no keychain entry");
    }
}
