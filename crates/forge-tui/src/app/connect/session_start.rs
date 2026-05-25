use crate::app::App;
use crate::app::config::{language_input_validation_message, store};
use forge_workspace::SessionLaunchSettings;
use serde_json::{Map, Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionStartReason {
    Startup,
    NewSession,
    Resume,
}

impl SessionStartReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::NewSession => "new_session",
            Self::Resume => "resume",
        }
    }

    fn event_name(self) -> &'static str {
        match self {
            Self::Startup => "session_start_requested",
            Self::Resume => "session_resume_requested",
            Self::NewSession => "session_restart_requested",
        }
    }
}

pub(crate) fn session_launch_settings_for_reason(
    app: &App,
    _reason: SessionStartReason,
) -> SessionLaunchSettings {
    let language = store::language(&app.config.committed_settings_document)
        .ok()
        .flatten()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .filter(|value| language_input_validation_message(value).is_none());
    SessionLaunchSettings {
        language,
        settings: Some(build_session_settings_object(app)),
        agent_progress_summaries: Some(true),
        charter: None,
        ..SessionLaunchSettings::default()
    }
}

fn build_session_settings_object(app: &App) -> Value {
    let mut settings = Map::new();

    settings.insert(
        "alwaysThinkingEnabled".to_owned(),
        Value::Bool(app.config.always_thinking_effective()),
    );

    if let Some(model) = app.config.model_effective() {
        settings.insert("model".to_owned(), Value::String(model));
    }

    settings.insert(
        "permissions".to_owned(),
        json!({
            "defaultMode": app.config.default_permission_mode_effective().as_stored()
        }),
    );
    settings.insert("fastMode".to_owned(), Value::Bool(app.config.fast_mode_effective()));
    settings.insert(
        "effortLevel".to_owned(),
        Value::String(app.config.thinking_effort_effective().as_stored().to_owned()),
    );
    settings.insert(
        "outputStyle".to_owned(),
        Value::String(app.config.output_style_effective().as_stored().to_owned()),
    );
    settings.insert(
        "spinnerTipsEnabled".to_owned(),
        Value::Bool(
            store::spinner_tips_enabled(&app.config.committed_local_settings_document)
                .unwrap_or(true),
        ),
    );
    settings.insert(
        "terminalProgressBarEnabled".to_owned(),
        Value::Bool(
            store::terminal_progress_bar_enabled(&app.config.committed_preferences_document)
                .unwrap_or(true),
        ),
    );
    if let Some(mut sandbox) =
        app.config.committed_settings_document.get("sandbox").and_then(Value::as_object).cloned()
    {
        if sandbox.get("enabled").and_then(Value::as_bool) == Some(true)
            && !sandbox.contains_key("failIfUnavailable")
        {
            sandbox.insert("failIfUnavailable".to_owned(), Value::Bool(false));
        }
        settings.insert("sandbox".to_owned(), Value::Object(sandbox));
    }

    Value::Object(settings)
}

fn log_session_request(
    app: &App,
    reason: SessionStartReason,
    launch_settings: &SessionLaunchSettings,
    session_id: Option<&str>,
) {
    let has_language = launch_settings.language.is_some();
    let has_settings = launch_settings.settings.is_some();
    let agent_progress_summaries_enabled =
        launch_settings.agent_progress_summaries.unwrap_or(false);
    if let Some(session_id) = session_id {
        tracing::info!(
            target: crate::logging::targets::APP_SESSION,
            event_name = reason.event_name(),
            message = "session request queued",
            outcome = "start",
            reason = reason.as_str(),
            session_id = %session_id,
            cwd = %app.cwd_raw(),
            has_language,
            has_settings,
            agent_progress_summaries_enabled,
        );
    } else {
        tracing::info!(
            target: crate::logging::targets::APP_SESSION,
            event_name = reason.event_name(),
            message = "session request queued",
            outcome = "start",
            reason = reason.as_str(),
            cwd = %app.cwd_raw(),
            has_language,
            has_settings,
            agent_progress_summaries_enabled,
        );
    }
}

pub(crate) fn start_new_session(app: &App, reason: SessionStartReason) -> anyhow::Result<()> {
    let launch_settings = session_launch_settings_for_reason(app, reason);
    log_session_request(app, reason, &launch_settings, None);
    let cwd = app.cwd_raw();
    app.dispatch_command(|key| forge_workspace::Command::NewSession { key, cwd, launch_settings })
        .map_err(|err| anyhow::anyhow!("workspace dispatch failed: {err}"))
}

pub(crate) fn resume_session(app: &App, session_id: String) -> anyhow::Result<()> {
    let launch_settings = session_launch_settings_for_reason(app, SessionStartReason::Resume);
    log_session_request(app, SessionStartReason::Resume, &launch_settings, Some(&session_id));
    let cwd = app.cwd_raw();
    // `claude --resume` keys sessions off the subprocess's working
    // directory; pass the current bucket's cwd so claude looks in the
    // right project subdir. Empty cwd would inherit forge's `$PWD`,
    // which for an in-session resume usually does not match the
    // target project.
    app.dispatch_command(|key| forge_workspace::Command::ResumeSession {
        key,
        session_id,
        cwd,
        launch_settings,
    })
    .map_err(|err| anyhow::anyhow!("workspace dispatch failed: {err}"))
}

/// Begin a session resume by marking the target session and sending the command.
///
/// Caller owns UI concerns such as entering `CommandPending` and surfacing
/// synchronous errors.
pub(crate) fn begin_resume_session(app: &mut App, session_id: String) -> anyhow::Result<()> {
    *app.resuming_session_id_mut() = Some(session_id.clone());
    resume_session(app, session_id)
}

#[cfg(test)]
mod tests {
    use super::{SessionStartReason, session_launch_settings_for_reason};
    use crate::agent::model::EffortLevel;
    use crate::app::App;
    use crate::app::config::{DefaultPermissionMode, store};
    use forge_workspace::SessionLaunchSettings;
    use serde_json::{Map, Value};

    #[test]
    fn persisted_launch_settings_include_model_and_permission_mode() {
        let mut app = App::test_default();
        store::set_model(&mut app.config.committed_settings_document, Some("haiku"));
        store::set_default_permission_mode(
            &mut app.config.committed_settings_document,
            DefaultPermissionMode::Plan,
        );
        store::set_language(&mut app.config.committed_settings_document, Some("German"));
        store::set_always_thinking_enabled(&mut app.config.committed_settings_document, true);
        store::set_thinking_effort_level(
            &mut app.config.committed_settings_document,
            EffortLevel::High,
        );

        let launch_settings = session_launch_settings_for_reason(&app, SessionStartReason::Startup);

        assert_eq!(launch_settings.language.as_deref(), Some("German"));
        assert_setting_value(&launch_settings, "alwaysThinkingEnabled", &Value::Bool(true));
        assert_setting_value(&launch_settings, "model", &Value::String("haiku".to_owned()));
        assert_permission_mode(&launch_settings, "plan");
        assert_setting_value(&launch_settings, "fastMode", &Value::Bool(false));
        assert_setting_value(&launch_settings, "effortLevel", &Value::String("high".to_owned()));
        assert_setting_value(&launch_settings, "outputStyle", &Value::String("Default".to_owned()));
        assert_setting_value(&launch_settings, "spinnerTipsEnabled", &Value::Bool(true));
        assert_setting_value(&launch_settings, "terminalProgressBarEnabled", &Value::Bool(true));
        assert_eq!(launch_settings.agent_progress_summaries, Some(true));
    }

    #[test]
    fn persisted_launch_settings_include_auto_permission_mode() {
        let mut app = App::test_default();
        store::set_default_permission_mode(
            &mut app.config.committed_settings_document,
            DefaultPermissionMode::Auto,
        );

        let launch_settings = session_launch_settings_for_reason(&app, SessionStartReason::Startup);

        assert_permission_mode(&launch_settings, "auto");
    }

    #[test]
    fn persisted_launch_settings_preserve_sandbox_settings_and_make_fallback_explicit() {
        let mut app = App::test_default();
        app.config.committed_settings_document = serde_json::json!({
            "sandbox": {
                "enabled": true,
                "allowUnsandboxedCommands": false
            }
        });

        let launch_settings = session_launch_settings_for_reason(&app, SessionStartReason::Startup);

        assert_setting_value(
            &launch_settings,
            "sandbox",
            &serde_json::json!({
                "enabled": true,
                "allowUnsandboxedCommands": false,
                "failIfUnavailable": false
            }),
        );
    }

    #[test]
    fn persisted_launch_settings_preserve_explicit_sandbox_fail_if_unavailable() {
        let mut app = App::test_default();
        app.config.committed_settings_document = serde_json::json!({
            "sandbox": {
                "enabled": true,
                "failIfUnavailable": true
            }
        });

        let launch_settings = session_launch_settings_for_reason(&app, SessionStartReason::Startup);

        assert_setting_value(
            &launch_settings,
            "sandbox",
            &serde_json::json!({
                "enabled": true,
                "failIfUnavailable": true
            }),
        );
    }

    #[test]
    fn persisted_launch_settings_trim_language_value() {
        let mut app = App::test_default();
        app.config.committed_settings_document = serde_json::json!({ "language": "  German  " });

        let launch_settings = session_launch_settings_for_reason(&app, SessionStartReason::Startup);

        assert_eq!(launch_settings.language.as_deref(), Some("German"));
    }

    #[test]
    fn persisted_launch_settings_default_permission_mode_when_missing() {
        let app = App::test_default();

        let launch_settings =
            session_launch_settings_for_reason(&app, SessionStartReason::NewSession);

        assert_eq!(launch_settings.language, None);
        assert_setting_value(&launch_settings, "model", &Value::String("opus".to_owned()));
        assert_setting_value(&launch_settings, "alwaysThinkingEnabled", &Value::Bool(false));
        // Forge defaults to `auto` permission mode when the user has
        // not stored an explicit value (mirrors `effortLevel = "max"`).
        assert_permission_mode(&launch_settings, "auto");
        assert_setting_value(&launch_settings, "fastMode", &Value::Bool(false));
        assert_setting_value(&launch_settings, "effortLevel", &Value::String("max".to_owned()));
        assert_setting_value(&launch_settings, "outputStyle", &Value::String("Default".to_owned()));
        assert_setting_value(&launch_settings, "spinnerTipsEnabled", &Value::Bool(true));
        assert_setting_value(&launch_settings, "terminalProgressBarEnabled", &Value::Bool(true));
        assert_eq!(launch_settings.agent_progress_summaries, Some(true));
    }

    #[test]
    fn persisted_launch_settings_include_supported_settings_json_with_explicit_opus_when_unset() {
        let mut app = App::test_default();
        store::set_always_thinking_enabled(&mut app.config.committed_settings_document, true);
        store::set_thinking_effort_level(
            &mut app.config.committed_settings_document,
            EffortLevel::High,
        );
        store::set_fast_mode(&mut app.config.committed_settings_document, true);
        store::set_output_style(
            &mut app.config.committed_local_settings_document,
            crate::app::config::OutputStyle::Learning,
        );
        store::set_spinner_tips_enabled(&mut app.config.committed_local_settings_document, false);
        store::set_terminal_progress_bar_enabled(
            &mut app.config.committed_preferences_document,
            false,
        );

        let launch_settings = session_launch_settings_for_reason(&app, SessionStartReason::Startup);

        assert_eq!(launch_settings.language, None);
        assert_setting_value(&launch_settings, "model", &Value::String("opus".to_owned()));
        assert_setting_value(&launch_settings, "alwaysThinkingEnabled", &Value::Bool(true));
        // Forge defaults to `auto` permission mode when unset.
        assert_permission_mode(&launch_settings, "auto");
        assert_setting_value(&launch_settings, "fastMode", &Value::Bool(true));
        assert_setting_value(&launch_settings, "effortLevel", &Value::String("high".to_owned()));
        assert_setting_value(
            &launch_settings,
            "outputStyle",
            &Value::String("Learning".to_owned()),
        );
        assert_setting_value(&launch_settings, "spinnerTipsEnabled", &Value::Bool(false));
        assert_setting_value(&launch_settings, "terminalProgressBarEnabled", &Value::Bool(false));
        assert_eq!(launch_settings.agent_progress_summaries, Some(true));
    }

    #[test]
    fn persisted_launch_settings_omit_invalid_language_value() {
        let mut app = App::test_default();
        app.config.committed_settings_document = serde_json::json!({ "language": "E" });

        let launch_settings = session_launch_settings_for_reason(&app, SessionStartReason::Startup);

        assert_eq!(launch_settings.language, None);
    }

    #[test]
    fn persisted_launch_settings_omit_whitespace_only_language_value() {
        let mut app = App::test_default();
        app.config.committed_settings_document = serde_json::json!({ "language": "   " });

        let launch_settings = session_launch_settings_for_reason(&app, SessionStartReason::Startup);

        assert_eq!(launch_settings.language, None);
    }

    fn settings_object(launch_settings: &SessionLaunchSettings) -> &Map<String, Value> {
        launch_settings.settings.as_ref().and_then(Value::as_object).expect("settings object")
    }

    fn assert_setting_value(launch_settings: &SessionLaunchSettings, key: &str, expected: &Value) {
        assert_eq!(settings_object(launch_settings).get(key), Some(expected));
    }

    fn assert_permission_mode(launch_settings: &SessionLaunchSettings, expected: &str) {
        let permissions = settings_object(launch_settings)
            .get("permissions")
            .and_then(Value::as_object)
            .expect("permissions object");
        assert_eq!(permissions.get("defaultMode"), Some(&Value::String(expected.to_owned())));
    }
}
