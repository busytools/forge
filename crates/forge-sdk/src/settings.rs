//! Settings document accessors — read **and write** the three Claude
//! Code configuration files. Reads return raw `serde_json::Value`
//! documents (consumers own the merge / precedence semantics);
//! writes take a complete document for a given scope and persist it
//! atomically (temp + rename + fsync), creating parent directories
//! when needed.
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

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

use crate::error::Error;
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

/// Which settings document a write should target. Mirrors the
/// read-side resolution in [`settings_documents`] — `User` honours
/// `$CLAUDE_CONFIG_DIR`, `ProjectLocal` is project-relative, and
/// `Preferences` is `$HOME`-pinned.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SettingsTarget {
    /// `<config_dir>/settings.json`.
    User,
    /// `<cwd>/.claude/settings.local.json`.
    ProjectLocal {
        /// The project root the file is scoped to.
        cwd: PathBuf,
    },
    /// `$HOME/.claude.json`.
    Preferences,
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

/// Write `document` atomically to the file [`SettingsTarget`]
/// resolves to. Mirrors the existing TUI `store::save` semantics:
///
/// - Creates parent directories when missing.
/// - Normalises the document — non-object inputs are written as `{}`
///   so consumers parsing the file always see a JSON object.
/// - Writes to a unique temp file in the same directory, calls
///   `flush + sync_all`, then `rename` into place. The temp filename
///   is `.{target_filename}.{epoch_nanos}.tmp` to avoid collisions
///   when two writes race on the same file.
/// - Trailing newline appended after the pretty-printed JSON to
///   match the CLI's own write style.
///
/// # Errors
///
/// [`Error::Io`] for any underlying filesystem failure — open,
/// write, fsync, rename, or `create_dir_all`. JSON serialisation
/// failures are wrapped as `Io` with a descriptive message.
pub fn write_settings_document(
    target: &SettingsTarget,
    document: &Value,
) -> Result<(), Error> {
    let path = target_path(target);
    write_json_atomic(&path, document)
}

fn target_path(target: &SettingsTarget) -> PathBuf {
    match target {
        SettingsTarget::User => claude_config_dir().join("settings.json"),
        SettingsTarget::ProjectLocal { cwd } => {
            cwd.join(".claude").join("settings.local.json")
        }
        SettingsTarget::Preferences => home_dir().join(".claude.json"),
    }
}

fn write_json_atomic(path: &Path, document: &Value) -> Result<(), Error> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "settings path has no parent directory")
    })?;
    std::fs::create_dir_all(parent)?;

    let normalized = match document {
        Value::Object(_) => document.clone(),
        _ => Value::Object(Map::new()),
    };

    let temp_path = unique_temp_path(parent, path.file_name().and_then(|n| n.to_str()));
    let mut temp = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)?;
    serde_json::to_writer_pretty(&mut temp, &normalized)
        .map_err(|err| io::Error::other(format!("serialize settings: {err}")))?;
    temp.write_all(b"\n")?;
    temp.flush()?;
    temp.sync_all()?;
    drop(temp);
    std::fs::rename(&temp_path, path)?;
    Ok(())
}

fn unique_temp_path(parent: &Path, filename_hint: Option<&str>) -> PathBuf {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let filename = filename_hint.unwrap_or("settings.json");
    parent.join(format!(".{filename}.{stamp}.tmp"))
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

    // ---- write_settings_document / write_json_atomic ----

    #[test]
    fn target_path_user_uses_claude_config_dir() {
        // Just sanity-check the join shape — we don't pin the
        // env-derived prefix because that's racy across parallel tests.
        let path = target_path(&SettingsTarget::User);
        assert!(path.ends_with("settings.json"));
    }

    #[test]
    fn target_path_project_local_uses_cwd() {
        let cwd = PathBuf::from("/tmp/forge_sdk_test_proj");
        let path = target_path(&SettingsTarget::ProjectLocal { cwd: cwd.clone() });
        assert_eq!(path, cwd.join(".claude").join("settings.local.json"));
    }

    #[test]
    fn target_path_preferences_uses_home_root() {
        let path = target_path(&SettingsTarget::Preferences);
        assert!(path.ends_with(".claude.json"));
        // The leading dot makes it a hidden file at $HOME, not a
        // file under $HOME/.claude — sanity-check we didn't drift.
        assert_ne!(path.file_name().and_then(|n| n.to_str()), Some("settings.json"));
    }

    #[test]
    fn write_json_atomic_creates_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        let doc = serde_json::json!({"theme": "dark", "fastMode": true});
        write_json_atomic(&path, &doc).expect("write");
        let parsed = read_json_file(&path).expect("read");
        assert_eq!(parsed.get("theme"), Some(&serde_json::json!("dark")));
        assert_eq!(parsed.get("fastMode"), Some(&serde_json::json!(true)));
    }

    #[test]
    fn write_json_atomic_creates_parent_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(".claude").join("settings.json");
        // .claude/ doesn't exist yet
        assert!(!path.parent().unwrap().exists());
        write_json_atomic(&path, &serde_json::json!({"k": "v"})).expect("write");
        assert!(path.exists());
        assert!(path.parent().unwrap().is_dir());
    }

    #[test]
    fn write_json_atomic_replaces_existing_contents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        // Pre-populate with an old document
        write_json_atomic(&path, &serde_json::json!({"old": 1, "stale": "yes"})).expect("write1");
        // Overwrite with a different shape
        write_json_atomic(&path, &serde_json::json!({"new": 2})).expect("write2");
        let parsed = read_json_file(&path).expect("read");
        assert!(parsed.get("old").is_none(), "stale key from old doc must not survive");
        assert!(parsed.get("stale").is_none(), "stale key from old doc must not survive");
        assert_eq!(parsed.get("new"), Some(&serde_json::json!(2)));
    }

    #[test]
    fn write_json_atomic_normalises_non_object_to_empty_object() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        // Caller passes an array — pathologically wrong but shouldn't
        // produce a non-object on disk.
        write_json_atomic(&path, &serde_json::json!([1, 2, 3])).expect("write");
        let parsed = read_json_file(&path).expect("read");
        assert!(parsed.is_object(), "non-object input must be normalised to {{}}");
        assert!(parsed.as_object().unwrap().is_empty());
    }

    #[test]
    fn write_json_atomic_appends_trailing_newline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        write_json_atomic(&path, &serde_json::json!({"k": "v"})).expect("write");
        let raw = std::fs::read_to_string(&path).expect("read raw");
        assert!(raw.ends_with('\n'), "atomic write must end with newline");
    }

    #[test]
    fn write_settings_document_project_local_round_trips() {
        // ProjectLocal is the only scope safely testable end-to-end
        // because cwd is a function arg — User and Preferences would
        // race on $CLAUDE_CONFIG_DIR / $HOME across parallel tests.
        let dir = tempfile::tempdir().expect("tempdir");
        let target = SettingsTarget::ProjectLocal { cwd: dir.path().to_path_buf() };
        let doc = serde_json::json!({"outputStyle": "verbose"});
        write_settings_document(&target, &doc).expect("write");
        let docs = settings_documents(dir.path());
        let project_local = docs.project_local.expect("present");
        assert_eq!(project_local.get("outputStyle"), Some(&serde_json::json!("verbose")));
    }
}
