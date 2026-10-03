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

use std::io::Write as _;
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
/// the agent's reported cwd, never `std::env::current_dir()`. It is
/// `None` when no project root resolves, which reads no project-local
/// document rather than joining an empty root into a relative path.
pub fn settings_documents(config_dir: &Path, cwd: Option<&Path>) -> SettingsDocuments {
    SettingsDocuments {
        user: read_json_file(&config_dir.join("settings.json")),
        project_local: cwd
            .and_then(|cwd| read_json_file(&cwd.join(".claude").join("settings.local.json"))),
        preferences: user_preferences(),
    }
}

/// Write one settings document, leaving the file it replaces whole.
///
/// A symlink at `path` is followed rather than replaced: the rename onto
/// a link would break a profile setup that points one config dir's
/// `settings.json` at another's.
pub fn save_document(path: &Path, document: &Value) -> Result<(), String> {
    let resolved = resolve_symlink(path)
        .map_err(|err| format!("Failed to resolve settings symlink: {err}"))?;
    let path = resolved.as_path();
    let parent = path.parent().ok_or_else(|| "Settings path has no parent directory".to_owned())?;
    if !parent.is_dir() {
        // A link whose target directory is gone. Writing still repairs
        // the canonical file, but building a tree for a stale link
        // should not happen silently.
        tracing::warn!(
            target: "forge_agent::userdata::settings",
            path = %path.display(),
            "settings symlink resolved to a path whose parent does not exist; creating it"
        );
    }
    std::fs::create_dir_all(parent)
        .map_err(|err| format!("Failed to create settings directory: {err}"))?;

    let normalized = match document {
        Value::Object(object) => Value::Object(object.clone()),
        _ => Value::Object(serde_json::Map::new()),
    };
    let temp_path = unique_temp_path(parent, path.file_name().and_then(std::ffi::OsStr::to_str));
    let result = write_then_rename(&temp_path, path, &normalized);
    if result.is_err() {
        // Best-effort: a failed rename would otherwise leave
        // `.settings.json.<nanos>.tmp` in the config dir forever.
        // Propagate the original error, not the cleanup's.
        if let Err(cleanup) = std::fs::remove_file(&temp_path) {
            tracing::debug!(
                target: "forge_agent::userdata::settings",
                error = %cleanup,
                "failed to clean up settings temp file; original error follows"
            );
        }
    }
    result
}

fn unique_temp_path(parent: &Path, filename_hint: Option<&str>) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let filename = filename_hint.unwrap_or("settings.json");
    parent.join(format!(".{filename}.{stamp}.tmp"))
}

/// Walk a symlink chain to the file it ultimately names, resolving each
/// relative target against its own link's parent. Not `canonicalize`,
/// which fails on a dangling link: a link whose target is missing should
/// still resolve, so the write recreates the canonical file rather than
/// clobbering the link.
fn resolve_symlink(path: &Path) -> std::io::Result<PathBuf> {
    // Chains are one hop in practice; the cap is only a cycle guard.
    const MAX_HOPS: usize = 32;

    let mut current = path.to_path_buf();
    for _ in 0..MAX_HOPS {
        match std::fs::symlink_metadata(&current) {
            Ok(md) if md.file_type().is_symlink() => {
                let link = std::fs::read_link(&current)?;
                current = if link.is_absolute() {
                    link
                } else {
                    current.parent().map_or_else(|| link.clone(), |parent| parent.join(&link))
                };
            }
            _ => return Ok(current),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("settings symlink chain exceeded {MAX_HOPS} hops: {}", path.display()),
    ))
}

fn write_then_rename(temp_path: &Path, path: &Path, document: &Value) -> Result<(), String> {
    let mut temp = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp_path)
        .map_err(|err| format!("Failed to create settings temp file: {err}"))?;
    serde_json::to_writer_pretty(&mut temp, document)
        .map_err(|err| format!("Failed to serialize settings: {err}"))?;
    temp.write_all(b"\n").map_err(|err| format!("Failed to finalize settings file: {err}"))?;
    temp.flush().map_err(|err| format!("Failed to flush settings file: {err}"))?;
    temp.sync_all().map_err(|err| format!("Failed to sync settings file: {err}"))?;
    drop(temp);
    // Carry the existing file's mode over. settings.json is 0600 for a
    // reason and a fresh temp file would otherwise widen it to 0644.
    if let Ok(existing) = std::fs::metadata(path) {
        std::fs::set_permissions(temp_path, existing.permissions())
            .map_err(|err| format!("Failed to apply settings file mode: {err}"))?;
    }
    std::fs::rename(temp_path, path)
        .map_err(|err| format!("Failed to move settings file into place: {err}"))
}

/// The per-user preferences document alone, for a reader that wants
/// nothing else out of it. `None` when the file is absent or unreadable.
pub fn user_preferences() -> Option<Value> {
    home_dir().and_then(|home| read_json_file(&home.join(".claude.json")))
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

    /// A `None` root yields no project-local document, and the config
    /// dir is not consulted for one. Neither half is pinned by this
    /// test: the rule-14 half could only be caught by a fixture at the
    /// cwd-relative path, which is a write into the source tree, and
    /// the config-dir half lost the fixture that caught it when that
    /// one went. The producer side pins the rule-14 half, in
    /// `app::config`.
    #[test]
    fn settings_documents_reads_no_project_local_without_a_root() {
        let dir = tempfile::tempdir().expect("tempdir");

        let docs = settings_documents(dir.path(), None);

        assert!(docs.project_local.is_none(), "no root, no project-local document");
    }

    /// Regression: a symlink at the write target must be preserved.
    /// `std::fs::rename(temp, symlink_path)` replaces the symlink
    /// itself, clobbering profile setups such as
    /// `/tmp/forge-test-stargate/settings.json -> ~/.claude/settings.json`.
    #[test]
    fn save_document_preserves_a_symlink_at_the_write_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let canonical_dir = dir.path().join("canonical");
        let profile_dir = dir.path().join("profile");
        std::fs::create_dir_all(&canonical_dir).expect("mkdir canonical");
        std::fs::create_dir_all(&profile_dir).expect("mkdir profile");

        let canonical = canonical_dir.join("settings.json");
        let profile = profile_dir.join("settings.json");
        std::fs::write(&canonical, b"{}\n").expect("seed canonical");
        std::os::unix::fs::symlink(&canonical, &profile).expect("symlink");

        save_document(&profile, &serde_json::json!({ "effortLevel": "max" })).expect("save");

        let md = std::fs::symlink_metadata(&profile).expect("symlink_metadata");
        assert!(md.file_type().is_symlink(), "profile path got clobbered into a real file");

        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&canonical).expect("read canonical"))
                .expect("parse");
        assert_eq!(written.get("effortLevel"), Some(&serde_json::json!("max")));
    }

    #[test]
    fn save_document_resolves_a_relative_symlink_against_its_own_parent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let canonical = dir.path().join("settings.json");
        let profile_dir = dir.path().join("profile");
        std::fs::create_dir_all(&profile_dir).expect("mkdir profile");
        let profile = profile_dir.join("settings.json");
        std::fs::write(&canonical, b"{}\n").expect("seed canonical");
        std::os::unix::fs::symlink(std::path::Path::new("..").join("settings.json"), &profile)
            .expect("symlink");

        save_document(&profile, &serde_json::json!({ "model": "opus" })).expect("save");

        let md = std::fs::symlink_metadata(&profile).expect("symlink_metadata");
        assert!(md.file_type().is_symlink(), "profile path got clobbered into a real file");

        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&canonical).expect("read canonical"))
                .expect("parse");
        assert_eq!(written.get("model"), Some(&serde_json::json!("opus")));
    }

    /// A profile link can point at another link. Resolving only one hop
    /// writes the intermediate and leaves the canonical file stale.
    #[test]
    fn save_document_walks_a_symlink_chain_to_the_canonical_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let canonical = dir.path().join("real.json");
        let mid = dir.path().join("mid.json");
        let top = dir.path().join("top.json");
        std::fs::write(&canonical, b"{}\n").expect("seed canonical");
        std::os::unix::fs::symlink(&canonical, &mid).expect("symlink mid");
        std::os::unix::fs::symlink(&mid, &top).expect("symlink top");

        save_document(&top, &serde_json::json!({ "model": "opus" })).expect("save");

        for (label, path) in [("top", &top), ("mid", &mid)] {
            let md = std::fs::symlink_metadata(path).expect("symlink_metadata");
            assert!(md.file_type().is_symlink(), "{label} got clobbered into a real file");
        }
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&canonical).expect("read canonical"))
                .expect("parse");
        assert_eq!(written.get("model"), Some(&serde_json::json!("opus")));
    }

    #[test]
    fn save_document_preserves_the_existing_file_mode() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        std::fs::write(&path, b"{}\n").expect("seed");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");

        save_document(&path, &serde_json::json!({ "model": "opus" })).expect("save");

        let mode = std::fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a restricted settings file must not become world-readable");
    }

    #[test]
    fn save_document_leaves_no_temp_file_behind_when_the_rename_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A directory at the target path makes rename fail after the temp
        // has been written and synced.
        let path = dir.path().join("settings.json");
        std::fs::create_dir(&path).expect("mkdir at target");

        assert!(save_document(&path, &serde_json::json!({ "model": "opus" })).is_err());

        let strays: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| std::path::Path::new(name).extension().is_some_and(|ext| ext == "tmp"))
            .collect();
        assert!(strays.is_empty(), "temp files left behind: {strays:?}");
    }
}
