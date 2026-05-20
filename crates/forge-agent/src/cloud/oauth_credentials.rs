//! OAuth bearer-credential reader. Resolves the access token the
//! `claude` CLI persisted at `<config_dir>/.credentials.json` (file
//! path on every platform, plus a macOS keychain fallback for fresh
//! installs that haven't seeded the file yet).
//!
//!
//! The returned [`OauthCredentials`] feeds the
//! `cloud::oauth_usage` HTTP client (Bearer header) — no other
//! consumer reads them today.

use std::path::Path;

use crate::cloud::time::parse_timestamp_value;

use serde_json::Value;

pub use forge_primitives::cloud::oauth_credentials::OauthCredentials;

/// Read + parse the user's OAuth credentials against an explicit
/// `config_dir`. The caller (typically a `ForgeSdkBridge`) is the
/// source of truth for which account's credentials to read; there is
/// no fallback to a process-env-derived path.
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
/// [`std::time::SystemTime`] of the `expiresAt` numeric or stringified-numeric
/// epoch.
pub fn load_oauth_credentials(config_dir: &Path) -> Option<OauthCredentials> {
    let path = config_dir.join(".credentials.json");
    if let Some(creds) = load_oauth_credentials_at(&path) {
        return Some(creds);
    }
    #[cfg(target_os = "macos")]
    {
        load_oauth_credentials_from_keychain(config_dir)
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

fn load_oauth_credentials_at(path: &Path) -> Option<OauthCredentials> {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::debug!(
                target: crate::logging::targets::OAUTH_CREDENTIALS,
                error = %e,
                path = %path.display(),
                "credentials file present but read failed (permissions? lock?)",
            );
            return None;
        }
    };
    let json = match serde_json::from_str::<Value>(&contents) {
        Ok(j) => j,
        Err(e) => {
            tracing::debug!(
                target: crate::logging::targets::OAUTH_CREDENTIALS,
                error = %e,
                path = %path.display(),
                "credentials file present but JSON parse failed (corrupt? partial write?)",
            );
            return None;
        }
    };
    parse_oauth_credentials(&json)
}

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
        // Log the variant name only — `Display` on `serde_json::Value`
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
    use std::io::Write;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn returns_none_for_nonexistent_file() {
        let path = Path::new("/tmp/forge_agent_test_nonexistent_credentials.json");
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
        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        write!(tmp, r#"{{"claudeAiOauth":{{"accessToken":"token","expiresAt":1700000000001}}}}"#)
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
            keychain_service_name(Path::new("/Users/dev/.claude-profile4")),
            "Claude Code-credentials-7a8e7f2e"
        );
        assert_eq!(
            keychain_service_name(Path::new("/Users/dev/.claude-gateway")),
            "Claude Code-credentials-0ed1d9d0"
        );
        assert_eq!(
            keychain_service_name(Path::new("/Users/dev/.claude-stargate")),
            "Claude Code-credentials-afc8bc35"
        );
        assert_eq!(
            keychain_service_name(Path::new("/Users/dev/.claude")),
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
