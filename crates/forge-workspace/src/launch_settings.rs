//! The settings a session is spawned with.
//!
//! Read from the three Claude Code documents rather than kept as state, so a
//! session started from a client spawns with what the terminal would have
//! launched. A spawn that skipped this carries no language and no model, and
//! the CLI's own defaults for permissions, effort and output style.

use forge_agent::client::SessionLaunchSettings;
use forge_primitives::runtime::EffortLevel;
use serde_json::{Map, Value, json};

/// The three documents a launch reads, each as the CLI sees it.
pub struct LaunchSettingsDocuments<'a> {
    /// `<config_dir>/settings.json`.
    pub user: &'a Value,
    /// `<cwd>/.claude/settings.local.json`.
    pub local: &'a Value,
    /// `$HOME/.claude.json`.
    pub preferences: &'a Value,
}

/// The mode stored at `settings.permissions.defaultMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DefaultPermissionMode {
    #[default]
    Default,
    Auto,
    AcceptEdits,
    Plan,
    DontAsk,
    BypassPermissions,
}

impl DefaultPermissionMode {
    pub const fn as_stored(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Auto => "auto",
            Self::AcceptEdits => "acceptEdits",
            Self::Plan => "plan",
            Self::DontAsk => "dontAsk",
            Self::BypassPermissions => "bypassPermissions",
        }
    }

    pub fn from_stored(value: &str) -> Option<Self> {
        match value {
            "default" => Some(Self::Default),
            "auto" => Some(Self::Auto),
            "acceptEdits" => Some(Self::AcceptEdits),
            "plan" => Some(Self::Plan),
            "dontAsk" => Some(Self::DontAsk),
            "bypassPermissions" => Some(Self::BypassPermissions),
            _ => None,
        }
    }
}

/// The style stored at `settings.outputStyle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputStyle {
    #[default]
    Default,
    Explanatory,
    Learning,
}

impl OutputStyle {
    pub const fn as_stored(self) -> &'static str {
        match self {
            Self::Default => "Default",
            Self::Explanatory => "Explanatory",
            Self::Learning => "Learning",
        }
    }

    pub fn from_stored(value: &str) -> Option<Self> {
        match value {
            "Default" => Some(Self::Default),
            "Explanatory" => Some(Self::Explanatory),
            "Learning" => Some(Self::Learning),
            _ => None,
        }
    }
}

const LANGUAGE_MIN_CHARS: usize = 2;
const LANGUAGE_MAX_CHARS: usize = 30;

/// Validate a free-text language string before it reaches the launch
/// settings payload. Returns a message when the value is out of range,
/// otherwise `None` for "looks fine".
pub fn language_input_validation_message(value: &str) -> Option<&'static str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let length = trimmed.chars().count();
    if length < LANGUAGE_MIN_CHARS {
        Some("Language must be at least 2 characters.")
    } else if length > LANGUAGE_MAX_CHARS {
        Some("Language must be at most 30 characters.")
    } else {
        None
    }
}

/// The language a session is launched in, when the stored value is usable.
pub fn language(document: &Value) -> Option<String> {
    read_string(document, &["language"])
        .ok()
        .flatten()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .filter(|value| language_input_validation_message(value).is_none())
}

pub fn always_thinking_enabled(document: &Value) -> bool {
    read_bool(document, &["alwaysThinkingEnabled"]).ok().flatten().unwrap_or(false)
}

/// The model a session launches on. Forge defaults to `opus` when none is
/// stored; the CLI's own default is `sonnet`, so without this every fresh
/// forge session would launch on sonnet.
pub fn model(document: &Value) -> Option<String> {
    read_string(document, &["model"]).ok().flatten().or_else(|| Some("opus".to_owned()))
}

/// Forge defaults to `max` effort when none is stored.
pub fn thinking_effort(document: &Value) -> EffortLevel {
    read_string(document, &["effortLevel"])
        .ok()
        .flatten()
        .and_then(|value| EffortLevel::from_stored(&value))
        .unwrap_or(EffortLevel::Max)
}

/// Forge defaults to `Auto`, which the CLI itself calls `default`.
pub fn default_permission_mode(document: &Value) -> DefaultPermissionMode {
    read_string(document, &["permissions", "defaultMode"])
        .ok()
        .flatten()
        .and_then(|value| DefaultPermissionMode::from_stored(&value))
        .unwrap_or(DefaultPermissionMode::Auto)
}

pub fn output_style(document: &Value) -> OutputStyle {
    read_string(document, &["outputStyle"])
        .ok()
        .flatten()
        .and_then(|value| OutputStyle::from_stored(&value))
        .unwrap_or_default()
}

pub fn spinner_tips_enabled(document: &Value) -> bool {
    read_bool(document, &["spinnerTipsEnabled"]).ok().flatten().unwrap_or(true)
}

pub fn terminal_progress_bar_enabled(document: &Value) -> bool {
    read_bool(document, &["terminalProgressBarEnabled"]).ok().flatten().unwrap_or(true)
}

/// The settings a launch carries: the language, the settings object the CLI
/// merges at boot, and agent progress summaries.
pub fn session_launch_settings(documents: &LaunchSettingsDocuments<'_>) -> SessionLaunchSettings {
    SessionLaunchSettings {
        language: language(documents.user),
        settings: Some(build_session_settings_object(documents)),
        agent_progress_summaries: Some(true),
        charter: None,
        ..SessionLaunchSettings::default()
    }
}

fn build_session_settings_object(documents: &LaunchSettingsDocuments<'_>) -> Value {
    let mut settings = Map::new();

    settings.insert(
        "alwaysThinkingEnabled".to_owned(),
        Value::Bool(always_thinking_enabled(documents.user)),
    );

    if let Some(model) = model(documents.user) {
        settings.insert("model".to_owned(), Value::String(model));
    }

    settings.insert(
        "permissions".to_owned(),
        json!({ "defaultMode": default_permission_mode(documents.user).as_stored() }),
    );
    settings.insert(
        "effortLevel".to_owned(),
        Value::String(thinking_effort(documents.user).as_stored().to_owned()),
    );
    settings.insert(
        "outputStyle".to_owned(),
        Value::String(output_style(documents.local).as_stored().to_owned()),
    );
    settings.insert(
        "spinnerTipsEnabled".to_owned(),
        Value::Bool(spinner_tips_enabled(documents.local)),
    );
    settings.insert(
        "terminalProgressBarEnabled".to_owned(),
        Value::Bool(terminal_progress_bar_enabled(documents.preferences)),
    );
    if let Some(mut sandbox) = documents.user.get("sandbox").and_then(Value::as_object).cloned() {
        if sandbox.get("enabled").and_then(Value::as_bool) == Some(true)
            && !sandbox.contains_key("failIfUnavailable")
        {
            sandbox.insert("failIfUnavailable".to_owned(), Value::Bool(false));
        }
        settings.insert("sandbox".to_owned(), Value::Object(sandbox));
    }

    Value::Object(settings)
}

fn read_json_path<'a>(document: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut current = document;
    for key in path {
        current = current.get(key)?;
    }
    Some(current)
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
mod tests {
    use super::*;

    fn documents<'a>(
        user: &'a Value,
        local: &'a Value,
        preferences: &'a Value,
    ) -> LaunchSettingsDocuments<'a> {
        LaunchSettingsDocuments { user, local, preferences }
    }

    fn empty() -> Value {
        Value::Object(Map::new())
    }

    fn settings_of(launch: &SessionLaunchSettings) -> &Value {
        launch.settings.as_ref().expect("a launch always carries the settings object")
    }

    #[test]
    fn the_settings_carry_the_stored_model_permissions_and_effort() {
        let user = json!({
            "alwaysThinkingEnabled": true,
            "model": "haiku",
            "permissions": { "defaultMode": "plan" },
            "effortLevel": "high",
            "language": "German",
        });
        // Each key is pinned in the document it is read from, so a reader
        // that reaches for the wrong one answers differently rather than
        // matching by luck.
        let local = json!({ "outputStyle": "Learning", "spinnerTipsEnabled": false });
        // False, which is not the default: a reader that reached for the
        // wrong document would otherwise answer `true` and match by luck.
        let preferences = json!({ "terminalProgressBarEnabled": false });

        let launch = session_launch_settings(&documents(&user, &local, &preferences));

        assert_eq!(launch.language.as_deref(), Some("German"));
        assert_eq!(launch.agent_progress_summaries, Some(true));
        let settings = settings_of(&launch);
        assert_eq!(settings["alwaysThinkingEnabled"], json!(true));
        assert_eq!(settings["model"], json!("haiku"));
        assert_eq!(settings["permissions"]["defaultMode"], json!("plan"));
        assert_eq!(settings["effortLevel"], json!("high"));
        assert_eq!(settings["outputStyle"], json!("Learning"));
        assert_eq!(settings["spinnerTipsEnabled"], json!(false));
        assert_eq!(settings["terminalProgressBarEnabled"], json!(false));
    }

    /// Forge's own defaults, which are not the CLI's: a document that says
    /// nothing still launches on opus, `auto` and max effort.
    #[test]
    fn nothing_stored_still_carries_forges_defaults() {
        let (user, local, preferences) = (empty(), empty(), empty());

        let launch = session_launch_settings(&documents(&user, &local, &preferences));

        assert_eq!(launch.language, None);
        let settings = settings_of(&launch);
        assert_eq!(settings["model"], json!("opus"));
        assert_eq!(settings["permissions"]["defaultMode"], json!("auto"));
        assert_eq!(settings["effortLevel"], json!("max"));
    }

    /// A stored value the launch cannot use reads as the default rather
    /// than travelling to the CLI: a launch never fails on a bad document.
    #[test]
    fn an_unreadable_value_reads_as_the_default() {
        let user = json!({
            "model": true,
            "effortLevel": "brisk",
            "permissions": { "defaultMode": "not-a-mode" },
        });
        let local = json!({ "outputStyle": "Verbose" });

        let launch = session_launch_settings(&documents(&user, &local, &empty()));

        let settings = settings_of(&launch);
        assert_eq!(settings["model"], json!("opus"));
        assert_eq!(settings["effortLevel"], json!("max"));
        assert_eq!(settings["permissions"]["defaultMode"], json!("auto"));
        assert_eq!(settings["outputStyle"], json!("Default"));
    }

    /// The CLI's own sandbox fallback is opt-out: a stored sandbox that is
    /// enabled but says nothing about availability is made explicit, and one
    /// that already answers the question is left alone.
    #[test]
    fn a_stored_sandbox_keeps_its_own_fallback_answer() {
        let user = json!({ "sandbox": { "enabled": true, "allowUnsandboxedCommands": false } });
        let launch = session_launch_settings(&documents(&user, &empty(), &empty()));
        assert_eq!(
            settings_of(&launch)["sandbox"],
            json!({ "enabled": true, "allowUnsandboxedCommands": false, "failIfUnavailable": false }),
            "an enabled sandbox with no answer gets the explicit opt-out, and its other keys ride along",
        );

        let user = json!({ "sandbox": { "enabled": true, "failIfUnavailable": true } });
        let launch = session_launch_settings(&documents(&user, &empty(), &empty()));
        assert_eq!(
            settings_of(&launch)["sandbox"]["failIfUnavailable"],
            json!(true),
            "an explicit answer is never overwritten",
        );
    }

    /// A language the settings document carries is trimmed before it reaches
    /// the payload, and one too short or too long is dropped rather than
    /// launched with.
    #[test]
    fn an_unusable_language_never_reaches_the_payload() {
        let user = json!({ "language": "  German  " });
        let launch = session_launch_settings(&documents(&user, &empty(), &empty()));
        assert_eq!(launch.language.as_deref(), Some("German"), "trimmed");

        let user = json!({ "language": "E" });
        let launch = session_launch_settings(&documents(&user, &empty(), &empty()));
        assert_eq!(launch.language, None, "too short");

        let user = json!({ "language": "   " });
        let launch = session_launch_settings(&documents(&user, &empty(), &empty()));
        assert_eq!(launch.language, None, "blank");

        // One character longer than the ceiling, so the bound is the one
        // being read rather than a shorter string failing some other way.
        let too_long = "L".repeat(31);
        let user = json!({ "language": too_long });
        let launch = session_launch_settings(&documents(&user, &empty(), &empty()));
        assert_eq!(launch.language, None, "longer than the ceiling");

        let at_the_ceiling = "L".repeat(30);
        let user = json!({ "language": at_the_ceiling });
        let launch = session_launch_settings(&documents(&user, &empty(), &empty()));
        assert_eq!(launch.language.as_deref(), Some(&*at_the_ceiling), "the ceiling itself passes");
    }
}
