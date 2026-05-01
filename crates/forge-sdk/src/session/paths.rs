//! Shared path resolution for the `claude` CLI's on-disk state, plus
//! typed accessors for files inside `<config_dir>` that consumers need
//! a structured view of (currently OAuth credentials).
//!
//! Every accessor that reads a file under the user's config directory
//! goes through `claude_config_dir()` so `$CLAUDE_CONFIG_DIR` is
//! honoured in exactly one place. Empty-string env values are treated
//! as unset to match the CLI's own behaviour.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::public_types::OauthCredentials;

/// Resolve the Claude config directory. Honours `$CLAUDE_CONFIG_DIR`
/// (ignoring empty-string values), else falls back to
/// `$HOME/.claude`. Shared across `sessions`, `session_mutations`,
/// `client`, and any accessor that needs a typed view of an on-disk
/// CLI artefact.
pub(crate) fn claude_config_dir() -> PathBuf {
    let custom = std::env::var("CLAUDE_CONFIG_DIR").ok();
    let home = std::env::var("HOME").ok();
    claude_config_dir_from(custom.as_deref(), home.as_deref())
}

/// Pure variant of [`claude_config_dir`] that takes `CLAUDE_CONFIG_DIR`
/// and `HOME` as arguments instead of reading the process environment.
/// Used internally so the env-resolution branches are unit-testable
/// without mutating shared process state during parallel test runs.
fn claude_config_dir_from(custom: Option<&str>, home: Option<&str>) -> PathBuf {
    if let Some(value) = custom {
        let trimmed = value.trim_end_matches('/');
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    PathBuf::from(home.unwrap_or(".")).join(".claude")
}

/// Resolve the Claude projects directory. Honours `$CLAUDE_CONFIG_DIR`
/// (ignoring empty-string values), else falls back to
/// `$HOME/.claude/projects`. Shared across `sessions`,
/// `session_mutations`, and `client`.
pub(crate) fn projects_dir() -> PathBuf {
    claude_config_dir().join("projects")
}

/// Resolve the path to the OAuth credentials file:
/// `<config_dir>/.credentials.json`.
fn credentials_path() -> PathBuf {
    claude_config_dir().join(".credentials.json")
}

/// Read + parse the user's OAuth credentials.
///
/// Resolution order matches the `claude` CLI's behaviour as of
/// 2.1.117:
///
/// 1. `<config_dir>/.credentials.json` — the on-disk file.
/// 2. **macOS only:** the system keychain entry
///    `Claude Code-credentials-<sha256-prefix>` where
///    `<sha256-prefix>` is the first 8 hex characters of
///    `SHA256(<config_dir-as-string>)`. The keychain blob holds the
///    same `{ "claudeAiOauth": { ... } }` JSON the file would.
///
/// Returns `None` when neither source has a parseable, non-empty
/// `claudeAiOauth.accessToken`. `expires_at` is `None` when the
/// payload omits an expiry field; otherwise it is the parsed
/// [`SystemTime`] of the `expiresAt` numeric or stringified-numeric
/// epoch.
pub(crate) fn load_oauth_credentials() -> Option<OauthCredentials> {
    if let Some(creds) = load_oauth_credentials_at(&credentials_path()) {
        return Some(creds);
    }
    #[cfg(target_os = "macos")]
    {
        load_oauth_credentials_from_keychain(&claude_config_dir())
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Service name the macOS keychain stores Claude Code credentials
/// under. Encoded as `Claude Code-credentials-<sha256-prefix>` where
/// `<sha256-prefix>` is the first 8 lowercase hex characters of the
/// SHA-256 hash of the `<config_dir>` path *as a string*. Same scheme
/// the official `claude` CLI uses (verified empirically against
/// 2.1.117 — `~/.claude-{nf,gateway,stargate}` on the dev box all
/// hashed correctly).
#[cfg(target_os = "macos")]
fn keychain_service_name(config_dir: &std::path::Path) -> String {
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
fn load_oauth_credentials_from_keychain(
    config_dir: &std::path::Path,
) -> Option<OauthCredentials> {
    let service = keychain_service_name(config_dir);
    let output = std::process::Command::new("security")
        .args(["find-generic-password", "-s", service.as_str(), "-w"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let password = String::from_utf8(output.stdout).ok()?;
    let trimmed = password.trim_end_matches(['\r', '\n']);
    let json = serde_json::from_str::<Value>(trimmed).ok()?;
    parse_oauth_credentials(&json)
}

fn load_oauth_credentials_at(path: &std::path::Path) -> Option<OauthCredentials> {
    let contents = std::fs::read_to_string(path).ok()?;
    let json = serde_json::from_str::<Value>(&contents).ok()?;
    parse_oauth_credentials(&json)
}

fn parse_oauth_credentials(json: &Value) -> Option<OauthCredentials> {
    let oauth = json.get("claudeAiOauth")?;
    let access_token = oauth.get("accessToken")?.as_str()?.trim();
    if access_token.is_empty() {
        return None;
    }

    Some(OauthCredentials {
        access_token: access_token.to_owned(),
        expires_at: oauth.get("expiresAt").and_then(parse_timestamp_value),
    })
}

fn parse_timestamp_value(value: &Value) -> Option<SystemTime> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_u64().and_then(|raw| i64::try_from(raw).ok()))
            .and_then(system_time_from_epoch),
        Value::String(raw) => raw.trim().parse::<i64>().ok().and_then(system_time_from_epoch),
        _ => None,
    }
}

fn system_time_from_epoch(raw: i64) -> Option<SystemTime> {
    if raw < 0 {
        return None;
    }

    let raw = u64::try_from(raw).ok()?;
    if raw >= 1_000_000_000_000 {
        Some(UNIX_EPOCH + Duration::from_millis(raw))
    } else {
        Some(UNIX_EPOCH + Duration::from_secs(raw))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::io::Write;

    #[test]
    fn config_dir_honours_claude_config_dir_when_set() {
        let resolved = claude_config_dir_from(Some("/tmp/custom-config"), Some("/home/ignored"));
        assert_eq!(resolved, PathBuf::from("/tmp/custom-config"));
    }

    #[test]
    fn config_dir_strips_trailing_slash_from_claude_config_dir() {
        let resolved = claude_config_dir_from(Some("/tmp/custom/"), Some("/home/ignored"));
        assert_eq!(resolved, PathBuf::from("/tmp/custom"));
    }

    #[test]
    fn config_dir_falls_back_to_home_when_claude_config_dir_empty() {
        let resolved = claude_config_dir_from(Some(""), Some("/home/me"));
        assert_eq!(resolved, PathBuf::from("/home/me/.claude"));
    }

    #[test]
    fn config_dir_falls_back_to_home_when_claude_config_dir_unset() {
        let resolved = claude_config_dir_from(None, Some("/home/me"));
        assert_eq!(resolved, PathBuf::from("/home/me/.claude"));
    }

    #[test]
    fn config_dir_falls_back_to_dot_when_home_unset() {
        let resolved = claude_config_dir_from(None, None);
        assert_eq!(resolved, PathBuf::from("./.claude"));
    }

    #[test]
    fn returns_none_for_nonexistent_file() {
        let path = std::path::Path::new("/tmp/forge_sdk_test_nonexistent_credentials.json");
        assert!(load_oauth_credentials_at(path).is_none());
    }

    #[test]
    fn returns_none_for_empty_json() {
        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        write!(tmp, "{{}}").expect("write");
        assert!(load_oauth_credentials_at(tmp.path()).is_none());
    }

    #[test]
    fn returns_none_for_empty_access_token() {
        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        write!(tmp, r#"{{"claudeAiOauth":{{"accessToken":"","refreshToken":"tok"}}}}"#)
            .expect("write");
        assert!(load_oauth_credentials_at(tmp.path()).is_none());
    }

    #[test]
    fn returns_credentials_for_valid_oauth() {
        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        write!(
            tmp,
            r#"{{"claudeAiOauth":{{"accessToken":"sk-ant-oat01-test","refreshToken":"sk-ant-ort01-test","expiresAt":9999999999999}}}}"#
        )
        .expect("write");
        let credentials = load_oauth_credentials_at(tmp.path()).expect("credentials");
        assert_eq!(credentials.access_token, "sk-ant-oat01-test");
        assert!(credentials.expires_at.is_some());
    }

    #[test]
    fn parses_expiry_in_seconds() {
        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        write!(tmp, r#"{{"claudeAiOauth":{{"accessToken":"token","expiresAt":1}}}}"#)
            .expect("write");
        let credentials = load_oauth_credentials_at(tmp.path()).expect("credentials");
        assert_eq!(credentials.expires_at, Some(UNIX_EPOCH + Duration::from_secs(1)));
    }

    #[test]
    fn parses_expiry_in_milliseconds() {
        // Use a non-second-aligned ms epoch so the assertion can't be
        // satisfied by `Duration::from_secs(...)` — proves the
        // ms-branch actually fired.
        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        write!(
            tmp,
            r#"{{"claudeAiOauth":{{"accessToken":"token","expiresAt":1700000000001}}}}"#
        )
        .expect("write");
        let credentials = load_oauth_credentials_at(tmp.path()).expect("credentials");
        assert_eq!(
            credentials.expires_at,
            Some(UNIX_EPOCH + Duration::from_millis(1_700_000_000_001))
        );
    }

    #[test]
    fn parses_expiry_string_form() {
        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        write!(tmp, r#"{{"claudeAiOauth":{{"accessToken":"token","expiresAt":"42"}}}}"#)
            .expect("write");
        let credentials = load_oauth_credentials_at(tmp.path()).expect("credentials");
        assert_eq!(credentials.expires_at, Some(UNIX_EPOCH + Duration::from_secs(42)));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn keychain_service_name_uses_sha256_prefix_of_config_dir_string() {
        // Verified empirically against `claude` 2.1.117 keychain
        // entries on the development host: each `~/.claude-<flavour>`
        // produces a service-name whose suffix is the first 8 hex
        // chars of SHA-256(<absolute-path>).
        assert_eq!(
            keychain_service_name(std::path::Path::new("/Users/dev/.claude-profile4")),
            "Claude Code-credentials-7a8e7f2e"
        );
        assert_eq!(
            keychain_service_name(std::path::Path::new("/Users/dev/.claude-gateway")),
            "Claude Code-credentials-0ed1d9d0"
        );
        assert_eq!(
            keychain_service_name(std::path::Path::new("/Users/dev/.claude-stargate")),
            "Claude Code-credentials-afc8bc35"
        );
        assert_eq!(
            keychain_service_name(std::path::Path::new("/Users/dev/.claude")),
            "Claude Code-credentials-e531d3a4"
        );
    }

    #[test]
    fn returns_none_for_malformed_json() {
        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        write!(tmp, "not json at all").expect("write");
        assert!(load_oauth_credentials_at(tmp.path()).is_none());
    }
}
