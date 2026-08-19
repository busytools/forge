use serde_json::{Map, Value};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::{DefaultPermissionMode, OutputStyle, PreferredNotifChannel};
use crate::agent::model::EffortLevel;

const SETTINGS_FILENAME: &str = "settings.json";
const LOCAL_SETTINGS_FILENAME: &str = "settings.local.json";
const PREFERENCES_FILENAME: &str = ".claude.json";
const CLAUDE_DIR: &str = ".claude";
const ANTHROPIC_DEFAULT_OPUS_MODEL_ENV: &str = "ANTHROPIC_DEFAULT_OPUS_MODEL";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsPaths {
    pub settings: PathBuf,
    pub local_settings: PathBuf,
    pub preferences: PathBuf,
}

pub struct LoadedSettingsDocuments {
    pub paths: SettingsPaths,
    pub settings_document: Value,
    pub local_settings_document: Value,
    pub preferences_document: Value,
}

/// Workspace-backed entry point into the bridge's settings reader.
/// Holds a borrowed `&Workspace` plus the active session's
/// `&SessionKey` so `load` / `resolve_paths` can ask the workspace
/// for the bridge's documents + config_dir without TUI ever holding
/// an `AgentHandle` directly.
#[derive(Clone, Copy)]
pub struct WorkspaceBridge<'a> {
    pub workspace: &'a forge_workspace::Workspace,
    pub key: &'a forge_workspace::SessionKey,
}

pub fn load(
    home_override: Option<&Path>,
    project_root: &Path,
    bridge: Option<WorkspaceBridge<'_>>,
) -> Result<LoadedSettingsDocuments, String> {
    let paths = resolve_paths(home_override, project_root, bridge)?;

    // Production path delegates to the workspace facade so the same
    // `$CLAUDE_CONFIG_DIR`-respecting reader is used everywhere.
    // Test fixtures pass `home_override` and bypass the workspace -
    // env vars are process-global and would race across parallel
    // test runs.
    let (settings_document, local_settings_document, preferences_document) = match bridge {
        Some(bridge) if home_override.is_none() => {
            let docs = bridge
                .workspace
                .settings_documents(bridge.key, project_root)
                .ok_or_else(|| "no agent registered for session".to_owned())?;
            (
                docs.user.unwrap_or_else(empty_object),
                docs.project_local.unwrap_or_else(empty_object),
                docs.preferences.unwrap_or_else(empty_object),
            )
        }
        _ => (
            read_json_or_empty(&paths.settings),
            read_json_or_empty(&paths.local_settings),
            read_json_or_empty(&paths.preferences),
        ),
    };

    Ok(LoadedSettingsDocuments {
        paths,
        settings_document,
        local_settings_document,
        preferences_document,
    })
}

pub fn save(path: &Path, document: &Value) -> Result<(), String> {
    let resolved = resolve_symlink(path)
        .map_err(|err| format!("Failed to resolve settings symlink: {err}"))?;
    let path = resolved.as_path();
    let parent = path.parent().ok_or_else(|| "Settings path has no parent directory".to_owned())?;
    if !parent.is_dir() {
        // A link whose target directory is gone. Writing still repairs
        // the canonical file, but building a tree for a stale link
        // should not happen silently.
        tracing::warn!(
            target: "forge_tui::config",
            path = %path.display(),
            "settings symlink resolved to a path whose parent does not exist; creating it"
        );
    }
    std::fs::create_dir_all(parent)
        .map_err(|err| format!("Failed to create settings directory: {err}"))?;

    let normalized = normalized_root(document);
    let temp_path = unique_temp_path(parent, path.file_name().and_then(std::ffi::OsStr::to_str));
    let result = write_then_rename(&temp_path, path, &normalized);
    if result.is_err() {
        // Best-effort: a failed rename would otherwise leave
        // `.settings.json.<nanos>.tmp` in the config dir forever.
        // Propagate the original error, not the cleanup's.
        if let Err(cleanup) = std::fs::remove_file(&temp_path) {
            tracing::debug!(
                target: "forge_tui::config",
                error = %cleanup,
                "failed to clean up settings temp file; original error follows"
            );
        }
    }
    result
}

fn write_then_rename(temp_path: &Path, path: &Path, document: &Value) -> Result<(), String> {
    let mut temp = OpenOptions::new()
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

fn read_bool(document: &Value, path: &[&str]) -> Result<Option<bool>, ()> {
    match read_json_path(document, path) {
        None => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(()),
    }
}

fn read_string(document: &Value, path: &[&str]) -> Result<Option<String>, ()> {
    match read_json_path(document, path) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(()),
    }
}

#[cfg(test)]
fn write_bool(document: &mut Value, path: &[&str], enabled: bool) {
    set_json_path(document, path, Value::Bool(enabled));
}

fn write_string(document: &mut Value, path: &[&str], value: &str) {
    set_json_path(document, path, Value::String(value.to_owned()));
}

#[cfg(test)]
fn write_missing(document: &mut Value, path: &[&str]) {
    remove_json_path(document, path);
}

pub fn fast_mode(document: &Value) -> Result<bool, ()> {
    Ok(read_bool(document, &["fastMode"])?.unwrap_or(false))
}

pub fn always_thinking_enabled(document: &Value) -> Result<bool, ()> {
    Ok(read_bool(document, &["alwaysThinkingEnabled"])?.unwrap_or(false))
}

pub fn thinking_effort_level(document: &Value) -> Result<EffortLevel, ()> {
    match read_string(document, &["effortLevel"])? {
        // Forge defaults to `max` effort when unset.
        None => Ok(EffortLevel::Max),
        Some(value) => EffortLevel::from_stored(&value).ok_or(()),
    }
}

pub fn set_thinking_effort_level(document: &mut Value, level: EffortLevel) {
    write_string(document, &["effortLevel"], level.as_stored());
}

pub fn prefers_reduced_motion(document: &Value) -> Result<bool, ()> {
    Ok(read_bool(document, &["prefersReducedMotion"])?.unwrap_or(false))
}

#[cfg(test)]
pub fn set_prefers_reduced_motion(document: &mut Value, enabled: bool) {
    write_bool(document, &["prefersReducedMotion"], enabled);
}

pub fn output_style(document: &Value) -> Result<OutputStyle, ()> {
    match read_string(document, &["outputStyle"])? {
        None => Ok(OutputStyle::Default),
        Some(value) => OutputStyle::from_stored(&value).ok_or(()),
    }
}

#[cfg(test)]
pub fn set_model(document: &mut Value, model: Option<&str>) {
    match model {
        Some(value) => write_string(document, &["model"], value),
        None => write_missing(document, &["model"]),
    }
}

pub fn model(document: &Value) -> Result<Option<String>, ()> {
    read_string(document, &["model"])
}

pub fn default_permission_mode(document: &Value) -> Result<DefaultPermissionMode, ()> {
    match read_string(document, &["permissions", "defaultMode"])? {
        // Forge defaults to `Auto` permission mode when unset (the
        // CLI itself defaults to `default`). The override lives
        // here so launch_settings / picker render all pick up the
        // same forge-flavoured default.
        None => Ok(DefaultPermissionMode::Auto),
        Some(value) => DefaultPermissionMode::from_stored(&value).ok_or(()),
    }
}

#[cfg(test)]
pub fn set_respect_gitignore(document: &mut Value, enabled: bool) {
    write_bool(document, &["respectGitignore"], enabled);
}

#[cfg(test)]
pub fn set_default_permission_mode(document: &mut Value, mode: DefaultPermissionMode) {
    write_string(document, &["permissions", "defaultMode"], mode.as_stored());
}

#[cfg(test)]
pub fn set_language(document: &mut Value, value: Option<&str>) {
    match value.map(str::trim).filter(|text| !text.is_empty()) {
        Some(text) => write_string(document, &["language"], text),
        None => write_missing(document, &["language"]),
    }
}

#[cfg(test)]
pub fn set_always_thinking_enabled(document: &mut Value, enabled: bool) {
    write_bool(document, &["alwaysThinkingEnabled"], enabled);
}

#[cfg(test)]
pub fn set_fast_mode(document: &mut Value, enabled: bool) {
    write_bool(document, &["fastMode"], enabled);
}

#[cfg(test)]
pub fn set_output_style(document: &mut Value, style: OutputStyle) {
    write_string(document, &["outputStyle"], style.as_stored());
}

#[cfg(test)]
pub fn set_spinner_tips_enabled(document: &mut Value, enabled: bool) {
    write_bool(document, &["spinnerTipsEnabled"], enabled);
}

#[cfg(test)]
pub fn set_terminal_progress_bar_enabled(document: &mut Value, enabled: bool) {
    write_bool(document, &["terminalProgressBarEnabled"], enabled);
}

pub fn opus_version_pin(document: &Value) -> Result<Option<String>, ()> {
    read_string(document, &["env", ANTHROPIC_DEFAULT_OPUS_MODEL_ENV])
}

pub fn respect_gitignore(document: &Value) -> Result<bool, ()> {
    Ok(read_bool(document, &["respectGitignore"])?.unwrap_or(true))
}

pub fn preferred_notification_channel(document: &Value) -> Result<PreferredNotifChannel, ()> {
    match read_string(document, &["preferredNotifChannel"])? {
        None => Ok(PreferredNotifChannel::default()),
        Some(value) => PreferredNotifChannel::from_stored(&value).ok_or(()),
    }
}

pub fn language(document: &Value) -> Result<Option<String>, ()> {
    read_string(document, &["language"])
}

pub fn spinner_tips_enabled(document: &Value) -> Result<bool, ()> {
    Ok(read_bool(document, &["spinnerTipsEnabled"])?.unwrap_or(true))
}

pub fn terminal_progress_bar_enabled(document: &Value) -> Result<bool, ()> {
    Ok(read_bool(document, &["terminalProgressBarEnabled"])?.unwrap_or(true))
}

fn resolve_paths(
    home_override: Option<&Path>,
    project_root: &Path,
    bridge: Option<WorkspaceBridge<'_>>,
) -> Result<SettingsPaths, String> {
    let home = if let Some(path) = home_override {
        path.to_path_buf()
    } else {
        dirs::home_dir().ok_or_else(|| "Failed to resolve home directory".to_owned())?
    };
    let project_root = project_root.to_path_buf();

    // User settings live under <config_dir>, which honours
    // $CLAUDE_CONFIG_DIR - delegate to the workspace facade so the
    // env var is resolved in exactly one place. The home_override
    // case (used by tests) and the no-bridge case (early init /
    // disconnected) both bypass the workspace.
    let settings = match (home_override, bridge) {
        (None, Some(bridge)) => bridge.workspace.config_dir_for(bridge.key).map_or_else(
            || home.join(CLAUDE_DIR).join(SETTINGS_FILENAME),
            |dir| dir.join(SETTINGS_FILENAME),
        ),
        (Some(_), _) | (None, None) => home.join(CLAUDE_DIR).join(SETTINGS_FILENAME),
    };

    Ok(SettingsPaths {
        settings,
        local_settings: project_root.join(CLAUDE_DIR).join(LOCAL_SETTINGS_FILENAME),
        preferences: home.join(PREFERENCES_FILENAME),
    })
}

fn empty_object() -> Value {
    Value::Object(Map::new())
}

fn read_json_or_empty(path: &Path) -> Value {
    // NotFound is the normal case for fresh user/project settings -
    // return empty silently. Other I/O errors (perm denied, broken
    // FS) and JSON parse errors are surfaced as warn so the user
    // gets a triage signal instead of an empty-config mystery.
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return empty_object(),
        Err(e) => {
            tracing::warn!(
                target: "forge_tui::config",
                path = %path.display(),
                error = %e,
                "failed to read settings/preferences file"
            );
            return empty_object();
        }
    };
    match serde_json::from_str::<Value>(&raw) {
        Ok(v) if v.is_object() => v,
        Ok(_) => {
            tracing::warn!(
                target: "forge_tui::config",
                path = %path.display(),
                "settings/preferences file is not a JSON object; ignoring"
            );
            empty_object()
        }
        Err(e) => {
            tracing::warn!(
                target: "forge_tui::config",
                path = %path.display(),
                error = %e,
                "failed to parse settings/preferences file as JSON"
            );
            empty_object()
        }
    }
}

fn unique_temp_path(parent: &Path, filename_hint: Option<&str>) -> PathBuf {
    let stamp =
        SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |duration| duration.as_nanos());
    let filename = filename_hint.unwrap_or(SETTINGS_FILENAME);
    parent.join(format!(".{filename}.{stamp}.tmp"))
}

/// Walk a symlink chain to the file it ultimately names, resolving
/// each relative target against its own link's parent. Renaming onto
/// a symlink replaces the link itself, which would break profile
/// setups that point `~/.claude-<profile>/settings.json` at the
/// canonical `~/.claude/settings.json`.
///
/// Not `canonicalize`, which fails on a dangling link - here a link
/// whose target is missing should still resolve, so the write
/// recreates the canonical file rather than clobbering the link.
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

fn read_json_path<'a>(document: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut current = document;
    for key in path {
        current = current.as_object()?.get(*key)?;
    }
    Some(current)
}

fn set_json_path(document: &mut Value, path: &[&str], value: Value) {
    let Some((last_key, parents)) = path.split_last() else {
        return;
    };

    let mut current = ensure_object_mut(document);
    for key in parents {
        let child = current.entry((*key).to_owned()).or_insert_with(|| Value::Object(Map::new()));
        if !child.is_object() {
            *child = Value::Object(Map::new());
        }
        current = match child {
            Value::Object(object) => object,
            _ => unreachable!("child must be an object after normalization"),
        };
    }

    current.insert((*last_key).to_owned(), value);
}

#[cfg(test)]
fn remove_json_path(document: &mut Value, path: &[&str]) {
    if let Value::Object(object) = document {
        remove_from_object_path(object, path);
    }
}

#[cfg(test)]
fn remove_from_object_path(object: &mut Map<String, Value>, path: &[&str]) -> bool {
    let Some((head, tail)) = path.split_first() else {
        return object.is_empty();
    };

    if tail.is_empty() {
        object.remove(*head);
        return object.is_empty();
    }

    let should_remove_child = if let Some(child) = object.get_mut(*head) {
        match child {
            Value::Object(child_object) => remove_from_object_path(child_object, tail),
            _ => true,
        }
    } else {
        false
    };

    if should_remove_child {
        object.remove(*head);
    }

    object.is_empty()
}

fn normalized_root(document: &Value) -> Value {
    match document {
        Value::Object(object) => Value::Object(object.clone()),
        _ => Value::Object(Map::new()),
    }
}

fn ensure_object_mut(document: &mut Value) -> &mut Map<String, Value> {
    if !document.is_object() {
        *document = Value::Object(Map::new());
    }

    match document {
        Value::Object(object) => object,
        _ => unreachable!("document must be an object after normalization"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_missing_files_returns_empty_objects() {
        let dir = tempfile::tempdir().expect("tempdir");

        let loaded = load(Some(dir.path()), dir.path(), None).expect("load");

        assert_eq!(loaded.settings_document, Value::Object(Map::new()));
        assert_eq!(loaded.local_settings_document, Value::Object(Map::new()));
        assert_eq!(loaded.preferences_document, Value::Object(Map::new()));
        assert_eq!(loaded.paths.settings, dir.path().join(".claude").join("settings.json"));
        assert_eq!(
            loaded.paths.local_settings,
            dir.path().join(".claude").join("settings.local.json")
        );
        assert_eq!(loaded.paths.preferences, dir.path().join(".claude.json"));
    }

    #[test]
    fn load_malformed_files_returns_empty_objects_silently() {
        let dir = tempfile::tempdir().expect("tempdir");
        let settings_path = dir.path().join(".claude").join("settings.json");
        let preferences_path = dir.path().join(".claude.json");
        std::fs::create_dir_all(settings_path.parent().expect("settings parent"))
            .expect("create settings dir");
        std::fs::write(&settings_path, r#"{"fastMode":true}"#).expect("write settings");
        std::fs::write(&preferences_path, "{ not-json").expect("write malformed");

        let loaded = load(Some(dir.path()), dir.path(), None).expect("load");

        assert_eq!(fast_mode(&loaded.settings_document), Ok(true));
        assert_eq!(loaded.preferences_document, Value::Object(Map::new()));
    }

    #[test]
    fn persisted_setting_readers_apply_defaults() {
        let document = Value::Object(Map::new());

        // Forge defaults `defaultMode` to `Auto` when missing.
        assert_eq!(default_permission_mode(&document), Ok(DefaultPermissionMode::Auto));
        assert_eq!(respect_gitignore(&document), Ok(true));
        assert_eq!(output_style(&document), Ok(OutputStyle::Default));
        assert_eq!(model(&document), Ok(None));
        assert_eq!(preferred_notification_channel(&document), Ok(PreferredNotifChannel::Iterm2));
    }

    #[test]
    fn persisted_setting_readers_reject_invalid_values() {
        let invalid_notification = serde_json::json!({ "preferredNotifChannel": "not-a-channel" });
        let invalid_output_style = serde_json::json!({ "outputStyle": "Verbose" });
        let invalid_gitignore = serde_json::json!({ "respectGitignore": "yes" });
        let invalid_model = serde_json::json!({ "model": true });
        let invalid_permission_mode = serde_json::json!({
            "permissions": { "defaultMode": "not-a-mode" }
        });

        assert_eq!(preferred_notification_channel(&invalid_notification), Err(()));
        assert_eq!(output_style(&invalid_output_style), Err(()));
        assert_eq!(respect_gitignore(&invalid_gitignore), Err(()));
        assert_eq!(model(&invalid_model), Err(()));
        assert_eq!(default_permission_mode(&invalid_permission_mode), Err(()));
    }

    #[test]
    fn opus_version_pin_returns_none_when_unset() {
        let document = Value::Object(Map::new());

        assert_eq!(opus_version_pin(&document), Ok(None));
    }

    #[test]
    fn opus_version_pin_returns_string_when_set() {
        let document = serde_json::json!({
            "env": { "ANTHROPIC_DEFAULT_OPUS_MODEL": "claude-opus-4-7" }
        });

        assert_eq!(opus_version_pin(&document), Ok(Some("claude-opus-4-7".to_owned())));
    }

    #[test]
    fn opus_version_pin_errors_on_non_string_value() {
        let document = serde_json::json!({
            "env": { "ANTHROPIC_DEFAULT_OPUS_MODEL": true }
        });

        assert_eq!(opus_version_pin(&document), Err(()));
    }

    #[test]
    fn set_thinking_effort_level_writes_string_value() {
        let mut document = Value::Object(Map::new());
        set_thinking_effort_level(&mut document, EffortLevel::High);
        assert_eq!(thinking_effort_level(&document), Ok(EffortLevel::High));
    }

    #[test]
    fn set_model_writes_or_removes_value() {
        let mut document = serde_json::json!({ "model": "sonnet" });
        set_model(&mut document, Some("opus"));
        assert_eq!(model(&document), Ok(Some("opus".to_owned())));
        set_model(&mut document, None);
        assert_eq!(model(&document), Ok(None));
    }

    /// Regression: a symlink at the write target must be preserved.
    /// `std::fs::rename(temp, symlink_path)` replaces the symlink
    /// itself, clobbering profile setups such as
    /// `~/.claude-stargate/settings.json -> ~/.claude/settings.json`.
    #[test]
    fn save_preserves_a_symlink_at_the_write_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let canonical_dir = dir.path().join("canonical");
        let profile_dir = dir.path().join("profile");
        std::fs::create_dir_all(&canonical_dir).expect("mkdir canonical");
        std::fs::create_dir_all(&profile_dir).expect("mkdir profile");

        let canonical = canonical_dir.join(SETTINGS_FILENAME);
        let profile = profile_dir.join(SETTINGS_FILENAME);
        std::fs::write(&canonical, b"{}\n").expect("seed canonical");
        std::os::unix::fs::symlink(&canonical, &profile).expect("symlink");

        save(&profile, &serde_json::json!({ "effortLevel": "max" })).expect("save");

        let md = std::fs::symlink_metadata(&profile).expect("symlink_metadata");
        assert!(md.file_type().is_symlink(), "profile path got clobbered into a real file");

        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&canonical).expect("read canonical"))
                .expect("parse");
        assert_eq!(written.get("effortLevel"), Some(&serde_json::json!("max")));
    }

    #[test]
    fn save_resolves_a_relative_symlink_against_its_own_parent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let canonical = dir.path().join(SETTINGS_FILENAME);
        let profile_dir = dir.path().join("profile");
        std::fs::create_dir_all(&profile_dir).expect("mkdir profile");
        let profile = profile_dir.join(SETTINGS_FILENAME);
        std::fs::write(&canonical, b"{}\n").expect("seed canonical");
        std::os::unix::fs::symlink(Path::new("..").join(SETTINGS_FILENAME), &profile)
            .expect("symlink");

        save(&profile, &serde_json::json!({ "model": "opus" })).expect("save");

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
    fn save_walks_a_symlink_chain_to_the_canonical_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let canonical = dir.path().join("real.json");
        let mid = dir.path().join("mid.json");
        let top = dir.path().join("top.json");
        std::fs::write(&canonical, b"{}\n").expect("seed canonical");
        std::os::unix::fs::symlink(&canonical, &mid).expect("symlink mid");
        std::os::unix::fs::symlink(&mid, &top).expect("symlink top");

        save(&top, &serde_json::json!({ "model": "opus" })).expect("save");

        for (label, p) in [("top", &top), ("mid", &mid)] {
            let md = std::fs::symlink_metadata(p).expect("symlink_metadata");
            assert!(md.file_type().is_symlink(), "{label} got clobbered into a real file");
        }
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&canonical).expect("read canonical"))
                .expect("parse");
        assert_eq!(written.get("model"), Some(&serde_json::json!("opus")));
    }

    #[test]
    fn save_preserves_the_existing_file_mode() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(SETTINGS_FILENAME);
        std::fs::write(&path, b"{}\n").expect("seed");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");

        save(&path, &serde_json::json!({ "model": "opus" })).expect("save");

        let mode = std::fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a restricted settings file must not become world-readable");
    }

    #[test]
    fn save_leaves_no_temp_file_behind_when_the_rename_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A directory at the target path makes rename fail after the temp
        // has been written and synced.
        let path = dir.path().join(SETTINGS_FILENAME);
        std::fs::create_dir(&path).expect("mkdir at target");

        assert!(save(&path, &serde_json::json!({ "model": "opus" })).is_err());

        let strays: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| Path::new(n).extension().is_some_and(|ext| ext == "tmp"))
            .collect();
        assert!(strays.is_empty(), "temp files left behind: {strays:?}");
    }
}
