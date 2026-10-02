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
