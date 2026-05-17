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
//!   is the path the caller passes — typically the per-spawn account
//!   binding stored on the `ForgeSdkBridge`.
//! - **Project-local settings** at
//!   `<cwd>/.claude/settings.local.json`. Tied to the project's
//!   working directory; `<config_dir>` does not affect this path.
//! - **User preferences** at `$HOME/.claude.json` (note the leading
//!   dot — this is a *file* at the home root, not a directory under
//!   it). Per-user preferences (notification channel, gitignore
//!   respect, terminal-progress-bar, etc.); `<config_dir>` does not
//!   affect this either.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

use forge_sdk::Error;

/// Three raw settings documents, each `None` when the underlying
/// file is absent or unreadable. Malformed JSON also yields `None` —
/// consumers that care about distinguishing missing vs. corrupt
/// should re-read directly.
#[derive(Debug, Clone, Default, PartialEq)]
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
/// the `config_dir` argument, `ProjectLocal` is project-relative,
/// and `Preferences` is `$HOME`-pinned.
#[derive(Debug, Clone, PartialEq, Eq)]
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
/// `config_dir` is the user-scope config directory the caller has
/// bound this read to (typically the per-spawn account binding).
/// `cwd` is the project root used to locate
/// `<cwd>/.claude/settings.local.json` — sourced from `forge.toml` or
/// the agent's reported cwd, never `std::env::current_dir()`.
pub fn settings_documents(config_dir: &Path, cwd: &Path) -> SettingsDocuments {
    SettingsDocuments {
        user: read_json_file(&config_dir.join("settings.json")),
        project_local: read_json_file(&cwd.join(".claude").join("settings.local.json")),
        preferences: home_dir().and_then(|h| read_json_file(&h.join(".claude.json"))),
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
/// `config_dir` is consulted only for [`SettingsTarget::User`]; the
/// other variants ignore it.
///
/// # Errors
///
/// [`Error::Io`] for any underlying filesystem failure — open,
/// write, fsync, rename, or `create_dir_all`. JSON serialisation
/// failures are wrapped as `Io` with a descriptive message.
pub fn write_settings_document(
    config_dir: &Path,
    target: &SettingsTarget,
    document: &Value,
) -> Result<(), Error> {
    let path = target_path(config_dir, target)?;
    write_json_atomic(&path, document)
}

fn target_path(config_dir: &Path, target: &SettingsTarget) -> Result<PathBuf, Error> {
    match target {
        SettingsTarget::User => Ok(config_dir.join("settings.json")),
        SettingsTarget::ProjectLocal { cwd } => Ok(cwd.join(".claude").join("settings.local.json")),
        SettingsTarget::Preferences => {
            home_dir().map(|h| h.join(".claude.json")).ok_or_else(|| {
                Error::Io(io::Error::other("$HOME unset; cannot resolve preferences path"))
            })
        }
    }
}

fn write_json_atomic(path: &Path, document: &Value) -> Result<(), Error> {
    // If `path` is a symlink (e.g. `~/.claude-subspace/settings.json`
    // pointing at the canonical `~/.claude/settings.json` for shared
    // behaviour across Claude Code profiles), follow the symlink and
    // write to its target. `std::fs::rename(temp, symlink_path)`
    // replaces the symlink itself with the temp file's content; that
    // silently clobbers the user's profile setup. Resolve to the
    // target before rename so the symlink stays intact and the
    // canonical file gets the new content.
    let resolved = resolve_symlink(path)?;
    let path = resolved.as_path();

    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "settings path has no parent directory")
    })?;
    std::fs::create_dir_all(parent)?;

    let normalized = match document {
        Value::Object(_) => document.clone(),
        _ => Value::Object(Map::new()),
    };

    let temp_path = unique_temp_path(parent, path.file_name().and_then(|n| n.to_str()));
    // On any open/serialize/sync/rename failure, remove the temp file
    // before propagating the error so transient failures don't leak
    // `.settings.json.{nanos}.tmp` files into the config dir.
    let result = (|| -> io::Result<()> {
        let mut temp = OpenOptions::new().write(true).create_new(true).open(&temp_path)?;
        serde_json::to_writer_pretty(&mut temp, &normalized)
            .map_err(|err| io::Error::other(format!("serialize settings: {err}")))?;
        temp.write_all(b"\n")?;
        temp.flush()?;
        temp.sync_all()?;
        drop(temp);
        std::fs::rename(&temp_path, path)?;
        Ok(())
    })();
    if result.is_err() {
        // Best-effort cleanup; log a debug breadcrumb if the cleanup
        // itself fails (would mean accumulation continues silently),
        // but propagate the ORIGINAL error rather than the cleanup
        // failure.
        if let Err(cleanup_err) = std::fs::remove_file(&temp_path) {
            // Log only the basename of the temp path so the breadcrumb
            // doesn't expose the user's full config-dir path if the log
            // is shared in a bug report.
            let temp_basename = temp_path
                .file_name()
                .map_or_else(|| "<no-basename>".to_owned(), |n| n.to_string_lossy().into_owned());
            tracing::debug!(
                target: crate::logging::targets::SETTINGS,
                error = %cleanup_err,
                temp_basename,
                "best-effort temp cleanup failed; original error follows",
            );
        }
    }
    Ok(result?)
}

/// Follow a symlink one level. Returns the symlink's target path
/// (resolved against the symlink's parent for relative targets) when
/// `path` is a symlink; otherwise returns `path` unchanged. Errors
/// propagate from `read_link` only when the symlink is broken
/// mid-walk; non-symlink paths return without an error.
fn resolve_symlink(path: &Path) -> io::Result<PathBuf> {
    match std::fs::symlink_metadata(path) {
        Ok(md) if md.file_type().is_symlink() => {
            let link = std::fs::read_link(path)?;
            Ok(if link.is_absolute() {
                link
            } else {
                path.parent().map_or(link.clone(), |p| p.join(&link))
            })
        }
        // Path is a regular file, doesn't exist yet, or some other
        // non-symlink kind — write directly to it.
        _ => Ok(path.to_path_buf()),
    }
}

fn unique_temp_path(parent: &Path, filename_hint: Option<&str>) -> PathBuf {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let filename = filename_hint.unwrap_or("settings.json");
    parent.join(format!(".{filename}.{stamp}.tmp"))
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

    /// Regression: a symlink at the write target must be preserved.
    /// `std::fs::rename(temp, symlink_path)` replaces the symlink
    /// itself, clobbering profile setups (e.g.
    /// `~/.claude-subspace/settings.json -> ~/.claude/settings.json`).
    /// `write_json_atomic` resolves symlinks before rename so the
    /// link stays intact.
    #[test]
    fn symlink_at_write_target_is_preserved() {
        let dir = tempfile::tempdir().expect("tempdir");
        let canonical_dir = dir.path().join("canonical");
        let profile_dir = dir.path().join("profile");
        std::fs::create_dir_all(&canonical_dir).expect("mkdir canonical");
        std::fs::create_dir_all(&profile_dir).expect("mkdir profile");

        let canonical = canonical_dir.join("settings.json");
        let profile = profile_dir.join("settings.json");
        // Seed canonical with a stub so the symlink target exists.
        std::fs::write(&canonical, b"{}\n").expect("seed canonical");
        std::os::unix::fs::symlink(&canonical, &profile).expect("symlink");

        // Write through the symlink path.
        let new_doc = serde_json::json!({"effortLevel": "max"});
        write_json_atomic(&profile, &new_doc).expect("write");

        // 1. Profile path is still a symlink.
        let md = std::fs::symlink_metadata(&profile).expect("symlink_metadata");
        assert!(md.file_type().is_symlink(), "profile path got clobbered into a real file");

        // 2. Canonical (the symlink target) carries the new content.
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&canonical).expect("read canonical"))
                .expect("parse");
        assert_eq!(written.get("effortLevel"), Some(&serde_json::json!("max")));
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
    fn target_path_user_uses_supplied_config_dir() {
        let config_dir = PathBuf::from("/tmp/forge_test_user_settings_dir");
        let path = target_path(&config_dir, &SettingsTarget::User).expect("path");
        assert_eq!(path, config_dir.join("settings.json"));
    }

    #[test]
    fn target_path_project_local_uses_cwd() {
        let config_dir = PathBuf::from("/tmp/ignored");
        let cwd = PathBuf::from("/tmp/forge_sdk_test_proj");
        let path = target_path(&config_dir, &SettingsTarget::ProjectLocal { cwd: cwd.clone() })
            .expect("path");
        assert_eq!(path, cwd.join(".claude").join("settings.local.json"));
    }

    #[test]
    fn target_path_preferences_uses_home_root() {
        // $HOME must be set for this test to assert a path; skip otherwise
        // (matches the rule-15 fail-loud behaviour).
        if std::env::var_os("HOME").is_none_or(|s| s.is_empty()) {
            return;
        }
        let config_dir = PathBuf::from("/tmp/ignored");
        let path = target_path(&config_dir, &SettingsTarget::Preferences).expect("path");
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
        // ProjectLocal is the safest scope to test end-to-end because
        // its path is fully derived from the `cwd` arg and ignores
        // both `config_dir` and `$HOME`.
        let dir = tempfile::tempdir().expect("tempdir");
        let config_dir = PathBuf::from("/tmp/ignored");
        let target = SettingsTarget::ProjectLocal { cwd: dir.path().to_path_buf() };
        let doc = serde_json::json!({"outputStyle": "verbose"});
        write_settings_document(&config_dir, &target, &doc).expect("write");
        let docs = settings_documents(&config_dir, dir.path());
        let project_local = docs.project_local.expect("present");
        assert_eq!(project_local.get("outputStyle"), Some(&serde_json::json!("verbose")));
    }

    #[test]
    fn write_settings_document_user_writes_to_supplied_config_dir() {
        // Verify the User scope honours the explicit config_dir.
        let dir = tempfile::tempdir().expect("tempdir");
        let target = SettingsTarget::User;
        let doc = serde_json::json!({"theme": "neon"});
        write_settings_document(dir.path(), &target, &doc).expect("write");
        let written = read_json_file(&dir.path().join("settings.json")).expect("read");
        assert_eq!(written.get("theme"), Some(&serde_json::json!("neon")));
    }
}
