//! Settings document accessors — read the three Claude Code
//! configuration files from disk and return raw
//! `serde_json::Value` documents. Consumers own the merge /
//! precedence semantics — this module deliberately doesn't merge or
//! validate, so callers can apply whatever scope-tagging or
//! conflict-resolution logic suits them.
//!
//! Resolution rules match the `claude` CLI as of 2.1.117:
//!
//! - **User settings** at `<config_dir>/settings.json`. `<config_dir>`
//!   is `$CLAUDE_CONFIG_DIR` (when set + non-empty) else
//!   `$HOME/.claude` — see [`session::paths`](crate::session) for the
//!   shared resolver.
//! - **Project-local settings** at
//!   `<cwd>/.claude/settings.local.json`. Tied to the project's
//!   working directory; `$CLAUDE_CONFIG_DIR` does not affect this
//!   path.
//! - **User preferences** at `$HOME/.claude.json` (note the leading
//!   dot — this is a *file* at the home root, not a directory under
//!   it). Per-user preferences (notification channel, gitignore
//!   respect, terminal-progress-bar, etc.); `$CLAUDE_CONFIG_DIR`
//!   does not affect this either.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::session::paths::claude_config_dir;

/// Three raw settings documents, each `None` when the underlying
/// file is absent or unreadable. Malformed JSON also yields `None` —
/// consumers that care about distinguishing missing vs. corrupt
/// should re-read directly.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct SettingsDocuments {
    /// `<config_dir>/settings.json` — user-scope settings.
    pub user: Option<Value>,
    /// `<cwd>/.claude/settings.local.json` — project-local overrides.
    pub project_local: Option<Value>,
    /// `$HOME/.claude.json` — per-user preferences.
    pub preferences: Option<Value>,
}

/// Read the three Claude Code settings documents from disk.
///
/// `cwd` is the project root used to locate
/// `<cwd>/.claude/settings.local.json`. Pass `std::env::current_dir()`
/// to match the CLI's default behaviour.
#[must_use]
pub fn settings_documents(cwd: &Path) -> SettingsDocuments {
    SettingsDocuments {
        user: read_json_file(&claude_config_dir().join("settings.json")),
        project_local: read_json_file(&cwd.join(".claude").join("settings.local.json")),
        preferences: read_json_file(&home_dir().join(".claude.json")),
    }
}

fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

fn read_json_file(path: &Path) -> Option<Value> {
    let contents = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&contents).ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::io::Write;

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
        // Smoke-check the Default impl works — useful when callers
        // want a "no settings yet" placeholder.
        let docs = SettingsDocuments::default();
        assert!(docs.user.is_none());
        assert!(docs.project_local.is_none());
        assert!(docs.preferences.is_none());
    }
}
