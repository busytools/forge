use crate::app::App;

pub(crate) enum RuntimeReloadRequestOutcome {
    Requested,
    Unavailable,
    Failed,
}

pub(crate) fn request_runtime_reload(app: &mut App) -> RuntimeReloadRequestOutcome {
    let Some(workspace) = app.workspace.as_ref() else {
        return RuntimeReloadRequestOutcome::Unavailable;
    };
    let Some(key) = app.active_session_key.as_ref() else {
        return RuntimeReloadRequestOutcome::Unavailable;
    };
    let Some(session_id) = app.session_id() else {
        return RuntimeReloadRequestOutcome::Unavailable;
    };
    let session_id = session_id.to_string();
    match workspace.reload_plugins(key) {
        Ok(()) => {
            tracing::debug!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "runtime_reload_requested",
                message = "session runtime plugin reload requested",
                outcome = "start",
                session_id = %session_id,
            );
            RuntimeReloadRequestOutcome::Requested
        }
        Err(error) => {
            tracing::warn!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "runtime_reload_request_failed",
                message = "failed to request session runtime plugin reload",
                outcome = "failure",
                session_id = %session_id,
                error_message = %error,
            );
            RuntimeReloadRequestOutcome::Failed
        }
    }
}

pub(crate) fn request_context_usage_refresh(app: &mut App) {
    if app.session_usage().context_usage_in_flight {
        app.session_usage_mut().context_usage_refresh_pending = true;
        return;
    }

    let Some(workspace) = app.workspace.clone() else {
        clear_context_usage_refresh_state(app);
        return;
    };
    let Some(key) = app.active_session_key.clone() else {
        clear_context_usage_refresh_state(app);
        return;
    };
    let Some(session_id) = app.session_id() else {
        clear_context_usage_refresh_state(app);
        return;
    };

    let session_id = session_id.to_string();
    {
        let usage = app.session_usage_mut();
        usage.context_usage_in_flight = true;
        usage.context_usage_refresh_pending = false;
    }
    match workspace.refresh_context_usage(&key) {
        Ok(()) => tracing::debug!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "context_usage_requested",
            message = "session context usage requested",
            outcome = "start",
            session_id = %session_id,
        ),
        Err(error) => {
            app.session_usage_mut().context_usage_in_flight = false;
            tracing::warn!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "context_usage_request_failed",
                message = "failed to request session context usage",
                outcome = "failure",
                session_id = %session_id,
                error_message = %error,
            );
        }
    }
}

pub(crate) fn request_status_snapshot_refresh(app: &mut App) {
    let Some(workspace) = app.workspace.as_ref() else { return };
    let Some(key) = app.active_session_key.as_ref() else { return };
    let Some(session_id) = app.session_id() else {
        return;
    };

    let session_id = session_id.to_string();
    match workspace.refresh_status_snapshot(key) {
        Ok(()) => tracing::debug!(
            target: crate::logging::targets::APP_AUTH,
            event_name = "status_snapshot_requested",
            message = "session status snapshot requested",
            outcome = "start",
            session_id = %session_id,
        ),
        Err(error) => tracing::warn!(
            target: crate::logging::targets::APP_AUTH,
            event_name = "status_snapshot_request_failed",
            message = "failed to request session status snapshot",
            outcome = "failure",
            session_id = %session_id,
            error_message = %error,
        ),
    }
}

pub(crate) fn request_oauth_credentials_snapshot_refresh(app: &mut App) {
    let Some(workspace) = app.workspace.as_ref() else { return };
    let Some(key) = app.active_session_key.as_ref() else { return };
    let Some(session_id) = app.session_id() else {
        return;
    };

    let session_id = session_id.to_string();
    match workspace.refresh_oauth_credentials_snapshot(key) {
        Ok(()) => tracing::debug!(
            target: crate::logging::targets::APP_AUTH,
            event_name = "oauth_credentials_snapshot_requested",
            message = "session oauth credentials snapshot requested",
            outcome = "start",
            session_id = %session_id,
        ),
        Err(error) => tracing::warn!(
            target: crate::logging::targets::APP_AUTH,
            event_name = "oauth_credentials_snapshot_request_failed",
            message = "failed to request session oauth credentials snapshot",
            outcome = "failure",
            session_id = %session_id,
            error_message = %error,
        ),
    }
}

pub(crate) fn apply_context_usage_snapshot(app: &mut App, percentage: Option<u8>) {
    let refresh_pending = {
        let usage = app.session_usage_mut();
        usage.context_usage_percent = percentage;
        usage.context_usage_in_flight = false;
        std::mem::take(&mut usage.context_usage_refresh_pending)
    };
    if refresh_pending {
        request_context_usage_refresh(app);
    }
}

fn clear_context_usage_refresh_state(app: &mut App) {
    let usage = app.session_usage_mut();
    usage.context_usage_in_flight = false;
    usage.context_usage_refresh_pending = false;
}

#[cfg(test)]
mod tests {
    use super::{
        RuntimeReloadRequestOutcome, apply_context_usage_snapshot, request_context_usage_refresh,
        request_runtime_reload, request_status_snapshot_refresh,
    };
    use crate::agent::model;

    use crate::app::App;

    fn app_with_connection()
    -> (App, tokio::sync::mpsc::UnboundedReceiver<forge_primitives::AgentCommand>) {
        let mut app = App::test_default();
        let rx = app.install_testing_stub();
        app.set_session_id(Some(model::SessionId::new("session-1")));
        (app, rx)
    }

    #[test]
    fn request_runtime_reload_sends_bridge_command() {
        let (mut app, mut rx) = app_with_connection();

        assert!(matches!(request_runtime_reload(&mut app), RuntimeReloadRequestOutcome::Requested));

        let envelope = rx.try_recv().expect("reload command");
        assert!(matches!(
            envelope,
            forge_primitives::AgentCommand::ReloadPlugins { session_id } if session_id == "session-1"
        ));
    }

    #[test]
    fn request_runtime_reload_reports_unavailable_without_session_connection() {
        let mut app = App::test_default();

        assert!(matches!(
            request_runtime_reload(&mut app),
            RuntimeReloadRequestOutcome::Unavailable
        ));
    }

    #[test]
    fn request_context_usage_refresh_coalesces_in_flight_requests() {
        let (mut app, mut rx) = app_with_connection();

        request_context_usage_refresh(&mut app);
        request_context_usage_refresh(&mut app);

        assert!(app.session_usage().context_usage_in_flight);
        assert!(app.session_usage().context_usage_refresh_pending);
        let envelope = rx.try_recv().expect("context usage command");
        assert!(matches!(
            envelope,
            forge_primitives::AgentCommand::GetContextUsage { session_id } if session_id == "session-1"
        ));
        assert!(rx.try_recv().is_err(), "coalesced refresh should not send twice");
    }

    #[test]
    fn apply_context_usage_snapshot_replays_pending_refresh() {
        let (mut app, mut rx) = app_with_connection();
        request_context_usage_refresh(&mut app);
        request_context_usage_refresh(&mut app);
        let _ = rx.try_recv().expect("initial context usage command");

        apply_context_usage_snapshot(&mut app, Some(62));

        assert_eq!(app.session_usage().context_usage_percent, Some(62));
        assert!(app.session_usage().context_usage_in_flight);
        assert!(!app.session_usage().context_usage_refresh_pending);
        let envelope = rx.try_recv().expect("replayed context usage command");
        assert!(matches!(
            envelope,
            forge_primitives::AgentCommand::GetContextUsage { session_id } if session_id == "session-1"
        ));
    }

    #[test]
    fn request_status_snapshot_refresh_sends_bridge_command() {
        let (mut app, mut rx) = app_with_connection();

        request_status_snapshot_refresh(&mut app);

        let envelope = rx.try_recv().expect("status snapshot command");
        assert!(matches!(
            envelope,
            forge_primitives::AgentCommand::GetStatusSnapshot { session_id } if session_id == "session-1"
        ));
    }
}
