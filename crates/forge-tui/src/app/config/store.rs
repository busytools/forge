use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const SETTINGS_FILENAME: &str = "settings.json";
const LOCAL_SETTINGS_FILENAME: &str = "settings.local.json";
const PREFERENCES_FILENAME: &str = ".claude.json";
const CLAUDE_DIR: &str = ".claude";
const ANTHROPIC_DEFAULT_OPUS_MODEL_ENV: &str = "ANTHROPIC_DEFAULT_OPUS_MODEL";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsPaths {
    /// `None` when no session is bound. The file lives in the
    /// workspace's shared config dir, and nothing can name that before
    /// a spawn. There is deliberately no fallback: this path is also
    /// what a saved setting is written back to.
    pub settings: Option<PathBuf>,
    /// `None` when no project root resolved, which is the launchpad
    /// boot. There is deliberately no fallback path: a cwd-derived one
    /// would make the launch directory shape forge's settings.
    pub local_settings: Option<PathBuf>,
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
/// `&SessionSlot` so `load` / `resolve_paths` can ask the workspace
/// for the bridge's documents + config_dir without TUI ever holding
/// an `AgentHandle` directly.
#[derive(Clone, Copy)]
pub struct WorkspaceBridge<'a> {
    pub workspace: &'a Arc<forge_workspace::Workspace>,
    pub key: &'a forge_workspace::SessionSlot,
}

pub fn load(
    home_override: Option<&Path>,
    project_root: Option<&Path>,
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
            // No root means no project-local document, the same rule the
            // non-bridge arm follows; an empty root would join into a
            // relative path read against the process working directory.
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
            // The user settings document has no path without a session,
            // and with one the agent reader above is its only reader -
            // so nothing is read from disk here either way.
            empty_object(),
            paths.local_settings.as_deref().map_or_else(empty_object, read_json_or_empty),
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

#[cfg(test)]
fn write_string(document: &mut Value, path: &[&str], value: &str) {
    set_json_path(document, path, Value::String(value.to_owned()));
}

#[cfg(test)]
fn write_missing(document: &mut Value, path: &[&str]) {
    remove_json_path(document, path);
}

pub fn prefers_reduced_motion(document: &Value) -> Result<bool, ()> {
    Ok(read_bool(document, &["prefersReducedMotion"])?.unwrap_or(false))
}

#[cfg(test)]
pub fn set_prefers_reduced_motion(document: &mut Value, enabled: bool) {
    write_bool(document, &["prefersReducedMotion"], enabled);
}

#[cfg(test)]
pub fn set_model(document: &mut Value, model: Option<&str>) {
    match model {
        Some(value) => write_string(document, &["model"], value),
        None => write_missing(document, &["model"]),
    }
}

#[cfg(test)]
pub fn set_respect_gitignore(document: &mut Value, enabled: bool) {
    write_bool(document, &["respectGitignore"], enabled);
}

pub fn opus_version_pin(document: &Value) -> Result<Option<String>, ()> {
    read_string(document, &["env", ANTHROPIC_DEFAULT_OPUS_MODEL_ENV])
}

fn resolve_paths(
    home_override: Option<&Path>,
    project_root: Option<&Path>,
    bridge: Option<WorkspaceBridge<'_>>,
) -> Result<SettingsPaths, String> {
    let home = if let Some(path) = home_override {
        path.to_path_buf()
    } else {
        dirs::home_dir().ok_or_else(|| "Failed to resolve home directory".to_owned())?
    };

    // User settings live under <config_dir>, which honours
    // $CLAUDE_CONFIG_DIR - delegate to the workspace facade so the
    // env var is resolved in exactly one place. The home_override
    // case (used by tests) and the no-bridge case (early init /
    // disconnected) both bypass the workspace, and neither can name
    // the session's config dir, so neither produces a path.
    let settings = match (home_override, bridge) {
        (None, Some(bridge)) => {
            forge_server::surface::ViewSurface::new(Arc::clone(bridge.workspace))
                .roster()
                .config_dir(bridge.key)
                .map(|dir| dir.join(SETTINGS_FILENAME))
        }
        (Some(_), _) | (None, None) => None,
    };

    Ok(SettingsPaths {
        settings,
        local_settings: project_root
            .map(|root| root.join(CLAUDE_DIR).join(LOCAL_SETTINGS_FILENAME)),
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

fn read_json_path<'a>(document: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut current = document;
    for key in path {
        current = current.as_object()?.get(*key)?;
    }
    Some(current)
}

#[cfg(test)]
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

#[cfg(test)]
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

        let loaded = load(Some(dir.path()), Some(dir.path()), None).expect("load");

        assert_eq!(loaded.settings_document, Value::Object(Map::new()));
        assert_eq!(loaded.local_settings_document, Value::Object(Map::new()));
        assert_eq!(loaded.preferences_document, Value::Object(Map::new()));
        assert_eq!(loaded.paths.settings, None, "no session to name a config dir");
        assert_eq!(
            loaded.paths.local_settings,
            Some(dir.path().join(".claude").join("settings.local.json"))
        );
        assert_eq!(loaded.paths.preferences, dir.path().join(".claude.json"));
    }

    /// The bridge arm - the one a live session takes - reads the root the
    /// caller passed and not one of its own. No other test reaches this
    /// arm; without this, dropping or replacing that argument leaves the
    /// suite green.
    ///
    /// The second half asserts the intended no-root reading, but it is
    /// not what catches a substituted relative path: that would be
    /// relative to this test's process cwd, so only a fixture written
    /// into the source tree could catch it. The producer side pins
    /// that, in `app::config`.
    #[test]
    fn the_bridge_arm_reads_the_root_it_is_given() {
        let (workspace, _updates) = forge_workspace::Workspace::testing_stub();
        let key = forge_workspace::SessionSlot::from_str_for_test("bridge-arm-key");
        let _rx = workspace.install_testing_stub(&key);
        let dir = tempfile::tempdir().expect("tempdir");
        let local = dir.path().join(".claude");
        std::fs::create_dir_all(&local).expect("mkdir");
        std::fs::write(local.join("settings.local.json"), r#"{"prefersReducedMotion":true}"#)
            .expect("write");

        let with_root = load(
            None,
            Some(dir.path()),
            Some(WorkspaceBridge { workspace: &workspace, key: &key }),
        )
        .expect("load with a root");
        assert_eq!(
            prefers_reduced_motion(&with_root.local_settings_document),
            Ok(true),
            "the root the caller passed is the one read",
        );

        let without_root =
            load(None, None, Some(WorkspaceBridge { workspace: &workspace, key: &key }))
                .expect("load without a root");
        assert_eq!(
            without_root.local_settings_document,
            Value::Object(Map::new()),
            "no root reads no project-local document",
        );
    }

    /// The documents the loader reads from disk itself: a valid
    /// project-local document parses, and a malformed preferences file
    /// becomes an empty one rather than an error.
    #[test]
    fn load_malformed_files_returns_empty_objects_silently() {
        let dir = tempfile::tempdir().expect("tempdir");
        let local_path = dir.path().join(".claude").join("settings.local.json");
        std::fs::create_dir_all(local_path.parent().expect("local parent")).expect("create dir");
        std::fs::write(&local_path, r#"{"prefersReducedMotion":true}"#).expect("write local");
        std::fs::write(dir.path().join(".claude.json"), "{ not-json").expect("write malformed");

        let loaded = load(Some(dir.path()), Some(dir.path()), None).expect("load");

        assert_eq!(prefers_reduced_motion(&loaded.local_settings_document), Ok(true));
        assert_eq!(loaded.preferences_document, Value::Object(Map::new()));
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
    fn set_model_writes_or_removes_value() {
        let mut document = serde_json::json!({ "model": "sonnet" });
        set_model(&mut document, Some("opus"));
        assert_eq!(document["model"], serde_json::json!("opus"));
        set_model(&mut document, None);
        assert!(document.get("model").is_none(), "a cleared model is removed, not blanked");
    }

    /// With no project root no project-local path is resolved, and the
    /// document read follows the path: `None` means the loader has
    /// nothing to open. That is half of the rule-14 property - the
    /// other half is `project_root` never handing back an empty root,
    /// pinned in `app::config`, so nothing can join an empty path into
    /// a relative `.claude/settings.local.json`.
    #[test]
    fn load_without_a_project_root_resolves_no_local_path() {
        let dir = tempfile::tempdir().expect("tempdir");

        let loaded = load(Some(dir.path()), None, None).expect("load");

        assert!(
            loaded.paths.local_settings.is_none(),
            "an empty root must not become a relative local-settings path",
        );
        assert_eq!(loaded.local_settings_document, Value::Object(Map::new()));
    }

    /// `settings.json` lives in the session's config dir, which is
    /// per-account. Nothing can name it before a spawn, so a boot with
    /// no session applies no user settings document rather than reading
    /// the default config dir's, which belongs to another account. The
    /// path goes with it: nothing may write that file back either.
    #[test]
    fn load_without_a_session_applies_no_user_settings_document() {
        let dir = tempfile::tempdir().expect("tempdir");
        let settings = dir.path().join(".claude").join("settings.json");
        std::fs::create_dir_all(settings.parent().expect("parent")).expect("mkdir");
        std::fs::write(&settings, r#"{"alwaysThinkingEnabled":true}"#).expect("write");

        let loaded = load(Some(dir.path()), None, None).expect("load");

        assert_eq!(
            loaded.settings_document,
            Value::Object(Map::new()),
            "no session, no user settings document",
        );
        assert!(loaded.paths.settings.is_none(), "and no path to write one back to");
    }
}
