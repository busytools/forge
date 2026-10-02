use crate::app::App;
use forge_workspace::SessionLaunchSettings;
use forge_workspace::launch_settings::{LaunchSettingsDocuments, session_launch_settings};

pub(crate) fn session_launch_settings_for_reason(app: &App) -> SessionLaunchSettings {
    session_launch_settings(&LaunchSettingsDocuments {
        user: &app.config.committed_settings_document,
        local: &app.config.committed_local_settings_document,
        preferences: &app.config.committed_preferences_document,
    })
}

fn log_session_request(app: &App, launch_settings: &SessionLaunchSettings, session_id: &str) {
    let has_language = launch_settings.language.is_some();
    let has_settings = launch_settings.settings.is_some();
    let agent_progress_summaries_enabled =
        launch_settings.agent_progress_summaries.unwrap_or(false);
    tracing::info!(
        target: crate::logging::targets::APP_SESSION,
        event_name = "session_resume_requested",
        message = "session request queued",
        outcome = "start",
        reason = "resume",
        session_id = %session_id,
        cwd = %app.cwd_raw().unwrap_or_default(),
        has_language,
        has_settings,
        agent_progress_summaries_enabled,
    );
}

pub(crate) fn resume_session(app: &App, session_id: String) -> anyhow::Result<()> {
    let launch_settings = session_launch_settings_for_reason(app);
    log_session_request(app, &launch_settings, &session_id);
    let cwd = app.cwd_raw().unwrap_or_default();
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
    if let Some(slot) = app.resuming_session_id_mut() {
        *slot = Some(session_id.clone());
    }
    resume_session(app, session_id)
}
