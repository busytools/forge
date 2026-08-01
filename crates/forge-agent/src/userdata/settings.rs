//! Settings document accessors - read the three Claude Code
//! configuration files. Reads return raw `serde_json::Value`
//! documents; consumers own the merge / precedence semantics.
//!
//! Resolution rules match the `claude` CLI as of 2.1.117:
//!
//! - **User settings** at `<config_dir>/settings.json`. `<config_dir>`
//!   is the path the caller passes - typically the per-spawn account
//!   binding stored on the `ForgeSdkBridge`.
//! - **Project-local settings** at
//!   `<cwd>/.claude/settings.local.json`. Tied to the project's
//!   working directory; `<config_dir>` does not affect this path.
//! - **User preferences** at `$HOME/.claude.json` (note the leading
//!   dot - this is a *file* at the home root, not a directory under
//!   it). Per-user preferences (notification channel, gitignore
//!   respect, terminal-progress-bar, etc.); `<config_dir>` does not
//!   affect this either.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// Three raw settings documents, each `None` when the underlying
/// file is absent or unreadable. Malformed JSON also yields `None` -
/// consumers that care about distinguishing missing vs. corrupt
/// should re-read directly.
#[derive(Debug, Clone, Default)]
pub struct SettingsDocuments {
    /// `<config_dir>/settings.json` - user-scope settings.
    pub user: Option<Value>,
    /// `<cwd>/.claude/settings.local.json` - project-local overrides.
    pub project_local: Option<Value>,
    /// `$HOME/.claude.json` - per-user preferences.
    pub preferences: Option<Value>,
}

/// Read the three Claude Code settings documents from disk.
///
/// `config_dir` is the user-scope config directory the caller has
/// bound this read to (typically the per-spawn account binding).
/// `cwd` is the project root used to locate
/// `<cwd>/.claude/settings.local.json` - sourced from `forge.toml` or
/// the agent's reported cwd, never `std::env::current_dir()`.
pub fn settings_documents(config_dir: &Path, cwd: &Path) -> SettingsDocuments {
    SettingsDocuments {
        user: read_json_file(&config_dir.join("settings.json")),
        project_local: read_json_file(&cwd.join(".claude").join("settings.local.json")),
        preferences: home_dir().and_then(|h| read_json_file(&h.join(".claude.json"))),
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|s| !s.is_empty()).map(PathBuf::from)
}

fn read_json_file(path: &Path) -> Option<Value> {
    let contents = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(
                target: "forge_agent::userdata::settings",
                path = %path.display(),
                error = %e,
                "failed to read settings file"
            );
            return None;
        }
    };
    match serde_json::from_str(&contents) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(
                target: "forge_agent::userdata::settings",
                path = %path.display(),
                error = %e,
                "failed to parse settings file as JSON"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {

    use std::io::Write;

    use super::*;

    #[test]
    fn missing_file_returns_none() {
        let path = std::path::Path::new("/tmp/forge_sdk_test_nonexistent_settings.json");
        assert!(read_json_file(path).is_none());
    }

    #[test]
    fn malformed_json_returns_none() {
        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        write!(tmp, "{{ not valid").expect("write");
        assert!(read_json_file(tmp.path()).is_none());
    }

    #[test]
    fn parses_object() {
        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        write!(tmp, r#"{{"editorMode":"vim","userID":"abc"}}"#).expect("write");
        let value = read_json_file(tmp.path()).expect("parsed");
        assert_eq!(value.get("editorMode"), Some(&serde_json::json!("vim")));
        assert_eq!(value.get("userID"), Some(&serde_json::json!("abc")));
    }

    #[test]
    fn settings_documents_has_default() {
        // Smoke-check the Default impl works - useful when callers
        // want a "no settings yet" placeholder.
        let docs = SettingsDocuments::default();
        assert!(docs.user.is_none());
        assert!(docs.project_local.is_none());
        assert!(docs.preferences.is_none());
    }
}
