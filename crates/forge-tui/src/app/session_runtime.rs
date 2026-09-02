use std::time::{Duration, Instant};

use crate::app::App;

/// Minimum spacing between actual `get_context_usage` sends per
/// session. Rapid pane switches coalesce into the first send instead
/// of re-asking the CLI to recompute over the whole transcript.
const CONTEXT_USAGE_MIN_SEND_INTERVAL: Duration = Duration::from_secs(60);

/// Retained history bytes at or above which the auto refresh is
/// skipped: the CLI answers `get_context_usage` inline over the full
/// transcript, and past this size that computation alone can exceed
/// the hook timeout (#827).
const CONTEXT_USAGE_LARGE_TRANSCRIPT_BYTES: usize = 8 * 1024 * 1024;

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
    request_context_usage_refresh_at(app, Instant::now());
}

fn request_context_usage_refresh_at(app: &mut App, now: Instant) {
    if app.session_usage().context_usage_in_flight {
        app.session_usage_mut().context_usage_refresh_pending = true;
        return;
    }

    if app.retained_history_bytes() >= CONTEXT_USAGE_LARGE_TRANSCRIPT_BYTES {
        tracing::debug!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "context_usage_refresh_skipped",
            message = "auto context usage refresh skipped on large transcript",
            retained_bytes = app.retained_history_bytes(),
        );
        return;
    }

    if let Some(last) = app.session_usage().context_usage_last_sent
        && now.duration_since(last) < CONTEXT_USAGE_MIN_SEND_INTERVAL
    {
        tracing::debug!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "context_usage_refresh_debounced",
            message = "auto context usage refresh inside min send interval",
        );
        return;
    }

    send_context_usage_request(app, now);
}

/// Unconditional refresh: no size gate, no debounce. The manual-class
/// path - today the post-/compact refresh, where the displayed
/// percentage is guaranteed stale.
pub(crate) fn request_context_usage_refresh_forced(app: &mut App) {
    if app.session_usage().context_usage_in_flight {
        app.session_usage_mut().context_usage_refresh_pending = true;
        return;
    }
    send_context_usage_request(app, Instant::now());
}

fn send_context_usage_request(app: &mut App, now: Instant) {
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
        usage.context_usage_last_sent = Some(now);
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

pub(crate) fn apply_context_usage_snapshot(
    app: &mut App,
    percentage: Option<u8>,
    max_tokens: Option<u64>,
) {
    let refresh_pending = {
        let usage = app.session_usage_mut();
        usage.context_usage_percent = percentage;
        usage.context_max_tokens = max_tokens;
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
        request_context_usage_refresh_at, request_context_usage_refresh_forced,
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
    fn request_context_usage_refresh_debounces_to_one_send_per_interval() {
        let (mut app, mut rx) = app_with_connection();
        let t0 = std::time::Instant::now();

        request_context_usage_refresh_at(&mut app, t0);
        let _ = rx.try_recv().expect("first refresh sends");
        apply_context_usage_snapshot(&mut app, Some(62), Some(200_000));

        request_context_usage_refresh_at(&mut app, t0 + std::time::Duration::from_secs(30));
        assert!(rx.try_recv().is_err(), "a request inside the min interval must not send again");

        request_context_usage_refresh_at(&mut app, t0 + std::time::Duration::from_secs(61));
        let envelope = rx.try_recv().expect("past the interval the next request sends");
        assert!(matches!(
            envelope,
            forge_primitives::AgentCommand::GetContextUsage { session_id } if session_id == "session-1"
        ));
    }

    #[test]
    fn request_context_usage_refresh_skips_auto_refresh_on_large_transcripts() {
        let (mut app, mut rx) = app_with_connection();

        *app.retained_history_bytes_mut() = super::CONTEXT_USAGE_LARGE_TRANSCRIPT_BYTES;
        request_context_usage_refresh(&mut app);
        assert!(
            rx.try_recv().is_err(),
            "at or above the transcript threshold the auto refresh is skipped"
        );
        assert!(!app.session_usage().context_usage_in_flight);

        *app.retained_history_bytes_mut() = super::CONTEXT_USAGE_LARGE_TRANSCRIPT_BYTES - 1;
        request_context_usage_refresh(&mut app);
        let envelope = rx.try_recv().expect("below the threshold the refresh still sends");
        assert!(matches!(
            envelope,
            forge_primitives::AgentCommand::GetContextUsage { session_id } if session_id == "session-1"
        ));
    }

    #[test]
    fn request_context_usage_refresh_forced_bypasses_both_gates() {
        let (mut app, mut rx) = app_with_connection();

        *app.retained_history_bytes_mut() = super::CONTEXT_USAGE_LARGE_TRANSCRIPT_BYTES;
        app.session_usage_mut().context_usage_last_sent = Some(std::time::Instant::now());

        request_context_usage_refresh_forced(&mut app);

        let envelope = rx.try_recv().expect("forced refresh sends despite both gates");
        assert!(matches!(
            envelope,
            forge_primitives::AgentCommand::GetContextUsage { session_id } if session_id == "session-1"
        ));
    }

    #[test]
    fn apply_context_usage_snapshot_leaves_pending_refresh_to_the_debounce() {
        let (mut app, mut rx) = app_with_connection();
        request_context_usage_refresh(&mut app);
        request_context_usage_refresh(&mut app);
        let _ = rx.try_recv().expect("initial context usage command");

        apply_context_usage_snapshot(&mut app, Some(62), Some(200_000));

        assert_eq!(app.session_usage().context_usage_percent, Some(62));
        assert!(!app.session_usage().context_usage_in_flight);
        assert!(!app.session_usage().context_usage_refresh_pending);
        assert!(
            rx.try_recv().is_err(),
            "the pending refresh inside the min interval is served by the fresh snapshot, not re-sent"
        );
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
