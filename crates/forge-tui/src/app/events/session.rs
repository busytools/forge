use super::super::connect::take_connection_slot;
use super::super::connect::{SessionStartReason, start_new_session};
use super::super::state::RecentSessionInfo;
use super::super::view::{self, ActiveView};
use super::super::{
    App, AppStatus, ChatMessage, LoginHint, MessageBlock, MessageRole, SystemSeverity, TextBlock,
};
use super::push_system_message_with_severity;
use super::session_reset::{load_resume_history, reset_for_new_session};
use crate::agent::events::ServiceStatusSeverity;
use crate::agent::model;
use crate::error::AppError;
use forge_workspace::SessionKey;
use std::sync::Arc;

const TURN_ERROR_INPUT_LOCK_HINT: &str =
    "Input disabled after an error. Press Ctrl+Q to quit and try again.";

/// Returns `true` when `key` is a synthetic placeholder (sentinel
/// pattern `__<name>__`) rather than a real claude-issued session
/// UUID. The Connected handler uses this to find synthetic-keyed
/// buckets that it should migrate onto the real session key.
///
/// Today's sentinels: `__conn_pending__` (pre-Connect bucket from
/// startup) and `__spawn_<project>__` (sleeping-project click in
/// the Projects pane). Real claude session ids are UUIDs, which
/// never start or end with `__`.
fn is_synthetic_key(key: &SessionKey) -> bool {
    let s = key.as_str();
    s.len() >= 4 && s.starts_with("__") && s.ends_with("__")
}

#[allow(clippy::too_many_arguments)]
pub(super) fn handle_connected_client_event(
    app: &mut App,
    session_id: model::SessionId,
    cwd: String,
    current_model: model::CurrentModel,
    available_models: Vec<model::AvailableModel>,
    mode: Option<super::super::ModeState>,
    history_messages: &[forge_primitives::Message],
    pre_connect_key: Option<SessionKey>,
) {
    let session_id_for_log = session_id.to_string();
    let history_message_count = history_messages.len();
    let available_model_count = available_models.len();
    let prev_session_id = app.session_id().map(ToString::to_string);
    // Phase 2a foundation: register this session in the multi-session
    // map. Bucket-migration commits move per-session fields off App
    // into this entry; if a synthetic-keyed bucket is the active key
    // when Connected fires, migrate it onto the real session key so
    // the user-visible state (welcome message, cwd, viewport) survives
    // the Connect transition.
    //
    // The synthetic-key sentinel pattern is `__<name>__`. Two
    // currently-used variants:
    //
    // - `__conn_pending__` — the pre-Connect bucket seeded by
    //   `create_app`. Latched onto the active key from the moment
    //   forge-tui boots until the very first Connected event.
    // - `__spawn_<project>__` — the spawning bucket seeded when the
    //   user clicks a sleeping project header in the Projects pane
    //   (Phase 2b-α). Latched onto the active key while
    //   `workspace.get_agent_handle(SessionTarget::Named(...))` runs
    //   in the background.
    //
    // Migrate FIRST, then activate, then write conn into the target
    // bucket directly. This ordering guards against the multi-bridge
    // case (Phase 2b): if `set_conn` were called before activation,
    // the conn would land in the previous active bucket — a silent
    // routing bug. Resolving the target bucket first ensures the
    // conn slot lands in the correct bucket regardless of which
    // session was active before this event fired.
    let session_key = forge_workspace::SessionKey::from_session_id(session_id.to_string());
    // Determine which synthetic bucket this Connected event should
    // migrate. Preferred path: the connection task carried the
    // pre_connect_key it was seeded with — migrate THAT specific
    // bucket. Without this, rapid clicks on different sleeping
    // projects can race: A's Connected might pick up B's synthetic
    // bucket from the active key. Fallback chain (for paths that
    // don't yet thread pre_connect_key through, e.g. tests that
    // construct ClientEvent::Connected directly): the active
    // synthetic key, then the legacy `__conn_pending__` sentinel.
    let synthetic_to_migrate = pre_connect_key
        .filter(is_synthetic_key)
        .filter(|k| app.sessions.contains_key(k))
        .or_else(|| app.active_session_key.as_ref().filter(|k| is_synthetic_key(k)).cloned())
        .or_else(|| {
            let pre = forge_workspace::SessionKey::from_session_id(App::PRE_CONNECT_KEY);
            if app.sessions.contains_key(&pre) { Some(pre) } else { None }
        });
    // Decide whether this Connected event corresponds to a session
    // change the user is watching, or a background spawn that
    // completed while the user was elsewhere.
    //
    // - Synthetic-migration case: was_active is true iff the
    //   synthetic bucket is the active key. If the user switched
    //   away during the spawn window, the synthetic exists but is
    //   no longer active — the migration still happens but we
    //   don't yank `active_session_key` away from the user's
    //   deliberate pick.
    // - No-synthetic case (legacy single-session flow / second
    //   Connected as session reset): default to active-path
    //   behaviour. The original handler always ran the apply
    //   chain in this case; preserve that so `Connected` retains
    //   its long-standing "session reset" semantics on the
    //   single-session path.
    let was_active = match synthetic_to_migrate.as_ref() {
        Some(_) => synthetic_to_migrate.as_ref() == app.active_session_key.as_ref(),
        None => true,
    };
    if let Some(synth_key) = synthetic_to_migrate.as_ref() {
        if let Some(mut existing) = app.sessions.remove(synth_key) {
            if app.sessions.contains_key(&session_key) {
                tracing::warn!(
                    target: crate::logging::targets::APP_SESSION,
                    event_name = "connected_synthetic_dropped",
                    message = "synthetic bucket dropped because the real-key bucket already existed",
                    outcome = "dropped",
                    session_id = %session_id_for_log,
                    synthetic_key = %synth_key.as_str(),
                    reason = "real_bucket_present",
                );
                let _ = existing;
            } else {
                existing.key = Some(session_key.clone());
                // Lifecycle: the bucket that was just spawned is now
                // connected and idle. Future Running/Attention
                // transitions land on turn-start / permission-pending;
                // Idle on Connect is the right minimum.
                existing.lifecycle_state = crate::app::session::SessionLifecycleState::Idle;
                app.sessions.insert(session_key.clone(), existing);
            }
        }
    } else {
        app.sessions.entry(session_key.clone()).or_insert_with(|| {
            let mut bucket = crate::app::session::Session::new(session_key.clone());
            bucket.lifecycle_state = crate::app::session::SessionLifecycleState::Idle;
            bucket
        });
    }
    if was_active {
        // Active path: the user has been watching the spawning
        // bucket OR this is the startup connect (where the
        // pre-Connect synthetic bucket is the active key by
        // construction). Run the full active-session apply chain
        // below so welcome / file-index / runtime-tabs all sync.
        app.active_session_key = Some(session_key.clone());
        if let Some(slot) = take_connection_slot()
            && let Some(bucket) = app.sessions.get_mut(&session_key)
        {
            bucket.conn = Some(slot.conn);
        }
        apply_session_cwd(app, cwd);
        reset_for_new_session(app, session_id, current_model, mode, true);
        refresh_session_git_watcher(app, prev_session_id);
        *app.available_models_mut() = available_models;
        app.sync_welcome_snapshot();
        if !history_messages.is_empty() {
            load_resume_history(app, history_messages);
        }
        clear_pending_command(app);
        app.resuming_session_id = None;
        crate::app::file_index::restart(app);
        app.rebuild_chat_focus_from_state();
        crate::app::config::refresh_runtime_tabs_for_session_change(app);
        maybe_open_startup_session_picker(app);
    } else {
        // Background path: the user switched to a different session
        // while this one was spawning. Park the connection in the
        // newly-migrated bucket but don't yank the active session.
        if let Some(slot) = take_connection_slot()
            && let Some(bucket) = app.sessions.get_mut(&session_key)
        {
            bucket.conn = Some(slot.conn);
        }
        // Apply per-bucket cwd and identity directly rather than
        // running the full active-session apply chain (which would
        // touch app-global UI). The bucket already has cwd from its
        // Spawning state seed; apply the Connected-supplied value
        // without rewriting App-level fields.
        if let Some(bucket) = app.sessions.get_mut(&session_key) {
            let display = shorten_cwd_display(&cwd);
            bucket.cwd_raw = cwd;
            bucket.cwd = display;
            bucket.session_id = Some(session_id);
            bucket.current_model = Some(current_model);
            bucket.mode = mode;
            bucket.available_models = available_models;
            // Best-effort load of resume history into the bucket's
            // own message buffer — keep the bucket internally
            // consistent for a future switch.
            if !history_messages.is_empty() {
                bucket.messages.clear();
                bucket.message_retained_bytes.clear();
            }
        }
    }
    tracing::info!(
        target: crate::logging::targets::APP_SESSION,
        event_name = "session_connected",
        message = "session connected and applied",
        outcome = "success",
        session_id = %session_id_for_log,
        cwd = %app.cwd_raw(),
        current_model = ?app.current_model().map(|model| model.resolved_id.clone()),
        history_message_count,
        available_model_count,
        was_active,
    );
}

pub(super) fn handle_sessions_listed_event(
    app: &mut App,
    sessions: Vec<forge_primitives::SessionListEntry>,
) {
    let session_count = sessions.len();
    let pending_title_change = app.config.pending_session_title_change.take();
    let selected_session_id = app
        .recent_sessions
        .get(app.session_picker.selected)
        .map(|session| session.session_id.clone());
    let had_pending_title_change = pending_title_change.is_some();
    app.recent_sessions = sessions
        .into_iter()
        .map(|entry| RecentSessionInfo {
            session_id: entry.session_id,
            summary: entry.summary,
            last_modified_ms: entry.last_modified_ms,
            file_size_bytes: entry.file_size_bytes,
            cwd: entry.cwd,
            git_branch: entry.git_branch,
            custom_title: entry.custom_title,
            first_prompt: entry.first_prompt,
        })
        .collect();
    let mut pending_title_change_resolved = false;
    if let Some(pending_title_change) = pending_title_change {
        let renamed_session_present = app
            .recent_sessions
            .iter()
            .any(|session| session.session_id == pending_title_change.session_id);
        pending_title_change_resolved = renamed_session_present;
        if renamed_session_present {
            app.config.last_error = None;
            app.config.status_message = Some(match pending_title_change.kind {
                crate::app::config::PendingSessionTitleChangeKind::Rename { requested_title } => {
                    match requested_title {
                        Some(title) => format!("Renamed session to {title}"),
                        None => "Cleared session name".to_owned(),
                    }
                }
                crate::app::config::PendingSessionTitleChangeKind::Generate => {
                    "Generated session title".to_owned()
                }
            });
        }
    }
    app.startup_recent_sessions_loaded = true;
    reconcile_session_picker_selection(app, selected_session_id.as_deref());
    maybe_open_startup_session_picker(app);
    tracing::info!(
        target: crate::logging::targets::APP_SESSION,
        event_name = "sessions_list_updated",
        message = "sessions list applied",
        outcome = "success",
        session_count,
        had_pending_title_change,
        pending_title_change_resolved,
    );
}

pub(super) fn handle_auth_required_event(
    app: &mut App,
    session_key: &SessionKey,
    method_name: String,
    method_description: String,
) {
    if app.active_session_key.as_ref() != Some(session_key) {
        let Some(session) = app.session_mut(session_key) else {
            tracing::warn!(
                target: crate::logging::targets::APP_AUTH,
                event_name = "auth_required_dropped",
                message = "auth required dropped for an unknown session",
                outcome = "dropped",
                session_key = %session_key.as_str(),
                reason = "unknown_session",
            );
            return;
        };
        // Background-session auth-required: clear the bucket's
        // session identity / auth state. Don't touch App-global UI
        // (login_hint, status_message, pending_command). The bucket
        // becomes "needs auth"; if/when the user switches to it we
        // surface the hint then.
        session.session_id = None;
        session.key = None;
        session.current_model = None;
        session.mode = None;
        session.fast_mode_state = model::FastModeState::Off;
        session.session_usage = crate::app::state::SessionUsageState::default();
        session.last_rate_limit_update = None;
        session.cancelled_turn_pending_hint = false;
        session.pending_cancel_origin = None;
        session.account_info = None;
        session.mcp = super::super::McpState::default();
        super::turn::finalize_background_tool_calls(session, model::ToolCallStatus::Failed);
        session.active_turn_assistant_message_idx = None;
        session.turn_notice_refs.clear();
        session.session_scope_epoch = session.session_scope_epoch.saturating_add(1);
        tracing::warn!(
            target: crate::logging::targets::APP_AUTH,
            event_name = "auth_required_background",
            message = "auth required cleared background session state",
            outcome = "blocked",
            session_key = %session_key.as_str(),
            method_name = %method_name,
        );
        return;
    }
    let method_name_for_log = method_name.clone();
    clear_pending_command(app);
    app.resuming_session_id = None;
    app.login_hint = Some(LoginHint { method_name, method_description });
    app.bump_session_scope_epoch();
    app.clear_session_runtime_identity();
    super::clear_compaction_state(app, false);
    app.set_last_rate_limit_update(None);
    app.set_cancelled_turn_pending_hint(false);
    app.set_pending_cancel_origin(None);
    app.pending_auto_submit_after_cancel = false;
    app.set_account_info(None);
    *app.mcp_mut() = super::super::McpState::default();
    app.config.pending_session_title_change = None;
    crate::app::usage::reset_for_session_change(app);
    app.finalize_turn_runtime_artifacts(model::ToolCallStatus::Failed);
    app.clear_active_turn_assistant();
    super::notices::clear_turn_notice_tracking(app);
    tracing::warn!(
        target: crate::logging::targets::APP_AUTH,
        event_name = "auth_required_detected",
        message = "auth required cleared active session state",
        outcome = "blocked",
        method_name = %method_name_for_log,
    );
}

pub(super) fn handle_connection_failed_event(app: &mut App, session_key: &SessionKey, msg: &str) {
    if app.active_session_key.as_ref() != Some(session_key) {
        let Some(session) = app.session_mut(session_key) else {
            tracing::warn!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "connection_failed_dropped",
                message = "connection failure dropped for an unknown session",
                outcome = "dropped",
                session_key = %session_key.as_str(),
                reason = "unknown_session",
            );
            return;
        };
        // Background-session connection failure: clear the bucket's
        // identity/auth/turn state. Skip App-global UI (input,
        // pending_submit, push_message, status). Status surface for
        // a background bucket is the bucket itself; the active
        // session's status stays as-is.
        session.session_id = None;
        session.key = None;
        session.current_model = None;
        session.mode = None;
        session.fast_mode_state = model::FastModeState::Off;
        session.session_usage = crate::app::state::SessionUsageState::default();
        session.cancelled_turn_pending_hint = false;
        session.pending_cancel_origin = None;
        session.last_rate_limit_update = None;
        session.account_info = None;
        session.mcp = super::super::McpState::default();
        super::turn::finalize_background_tool_calls(session, model::ToolCallStatus::Failed);
        session.active_turn_assistant_message_idx = None;
        session.turn_notice_refs.clear();
        session.session_scope_epoch = session.session_scope_epoch.saturating_add(1);
        // Lifecycle: a failed connection lands the bucket back in
        // Sleeping rather than leaving it stuck on the Spawning
        // glyph. The Projects pane reads this for the per-session
        // state indicator.
        session.lifecycle_state = crate::app::session::SessionLifecycleState::Sleeping;
        tracing::error!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "session_connection_failed_background",
            message = "background session connection failure applied",
            outcome = "failure",
            session_key = %session_key.as_str(),
            error_message = %msg,
        );
        return;
    }
    app.bump_session_scope_epoch();
    app.clear_session_runtime_identity();
    super::clear_compaction_state(app, false);
    app.set_cancelled_turn_pending_hint(false);
    app.set_pending_cancel_origin(None);
    app.pending_auto_submit_after_cancel = false;
    app.set_last_rate_limit_update(None);
    app.set_account_info(None);
    *app.mcp_mut() = super::super::McpState::default();
    app.config.pending_session_title_change = None;
    crate::app::usage::reset_for_session_change(app);
    app.resuming_session_id = None;
    app.pending_command_label = None;
    app.pending_command_ack = None;
    app.finalize_turn_runtime_artifacts(model::ToolCallStatus::Failed);
    app.input.clear();
    app.pending_submit = None;
    app.status = AppStatus::Error;
    app.clear_active_turn_assistant();
    super::notices::clear_turn_notice_tracking(app);
    // Lifecycle: a failed connection on the active bucket also lands
    // back in Sleeping, matching the background path.
    if let Some(session) = app.session_mut(session_key) {
        session.lifecycle_state = crate::app::session::SessionLifecycleState::Sleeping;
    }
    push_connection_error_message(app, msg);
    tracing::error!(
        target: crate::logging::targets::APP_SESSION,
        event_name = "session_connection_failed",
        message = "session connection failure applied",
        outcome = "failure",
        error_message = %msg,
    );
}

pub(super) fn handle_slash_command_error_event(app: &mut App, session_key: &SessionKey, msg: &str) {
    if app.active_session_key.as_ref() != Some(session_key) {
        let Some(session) = app.session_mut(session_key) else {
            tracing::warn!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "slash_command_error_dropped",
                message = "slash command error dropped for an unknown session",
                outcome = "dropped",
                session_key = %session_key.as_str(),
                reason = "unknown_session",
            );
            return;
        };
        // Background slash command error: append a system message to
        // the bucket's chat buffer (so a future switch shows it).
        // Skip retention enforcement / viewport auto-scroll because
        // the bucket isn't being rendered. Skip the title-change
        // overlay reconciliation (App-global config UI).
        session.messages.push(ChatMessage::new(
            MessageRole::System(None),
            vec![MessageBlock::Text(TextBlock::from_complete(msg))],
            None,
        ));
        // Append a 0 to the parallel retained-bytes vec so the
        // history-retention bookkeeping stays consistent next time
        // the bucket runs through the active path.
        session.message_retained_bytes.push(0);
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "slash_command_error_background",
            message = "slash command error appended to background session chat",
            outcome = "info",
            session_key = %session_key.as_str(),
            error_message = %msg,
        );
        return;
    }
    if app.config.pending_session_title_change.take().is_some() {
        app.config.last_error = Some(msg.to_owned());
        app.config.status_message = None;
        app.needs_redraw = true;
        return;
    }
    app.push_message_tracked(ChatMessage::new(
        MessageRole::System(None),
        vec![MessageBlock::Text(TextBlock::from_complete(msg))],
        None,
    ));
    app.enforce_history_retention_tracked();
    app.viewport_mut().engage_auto_scroll();
    clear_pending_command(app);
    app.resuming_session_id = None;
}

pub(super) fn handle_auth_completed_event(
    app: &mut App,
    session_key: &SessionKey,
    conn: &Arc<forge_agent::AgentHandle>,
) {
    if app.active_session_key.as_ref() != Some(session_key) {
        // Auth completed for a non-active session is a degenerate case
        // — the user must have triggered /login from a session that's
        // since been backgrounded. Restarting a session targets the
        // active bucket by definition; the safest thing is to log and
        // drop. Once Phase 2b ships proper multi-bridge auth flows the
        // bridge will deliver this only to the active path.
        tracing::warn!(
            target: crate::logging::targets::APP_AUTH,
            event_name = "auth_completed_background_dropped",
            message = "auth-completed event ignored for a non-active session",
            outcome = "dropped",
            session_key = %session_key.as_str(),
            reason = "non_active_session",
        );
        return;
    }
    app.login_hint = None;
    app.pending_command_label = Some("Starting session...".to_owned());
    app.pending_command_ack = None;
    push_system_message_with_severity(
        app,
        Some(SystemSeverity::Info),
        "Authentication successful. Starting new session...",
    );
    app.force_redraw = true;
    tracing::info!(
        target: crate::logging::targets::APP_AUTH,
        event_name = "login_completed",
        message = "login completed and session restart requested",
        outcome = "success",
    );

    if let Err(e) = start_new_session(app, conn.as_ref(), SessionStartReason::Login) {
        tracing::error!(
            target: crate::logging::targets::APP_AUTH,
            event_name = "login_session_restart_failed",
            message = "failed to start session after login",
            outcome = "failure",
            error_message = %e,
        );
        clear_pending_command(app);
        push_system_message_with_severity(
            app,
            Some(SystemSeverity::Error),
            &format!("Failed to start session after login: {e}"),
        );
    }
}

pub(super) fn handle_logout_completed_event(app: &mut App, session_key: &SessionKey) {
    if app.active_session_key.as_ref() != Some(session_key) {
        let Some(session) = app.session_mut(session_key) else {
            tracing::warn!(
                target: crate::logging::targets::APP_AUTH,
                event_name = "logout_completed_dropped",
                message = "logout completed dropped for an unknown session",
                outcome = "dropped",
                session_key = %session_key.as_str(),
                reason = "unknown_session",
            );
            return;
        };
        // Background-session logout: clear the bucket's auth +
        // identity state. Skip App-global UI restart (force_redraw,
        // pending_command). Foreground switching to that bucket
        // will surface the auth-required hint via AuthRequired.
        session.session_id = None;
        session.key = None;
        session.current_model = None;
        session.mode = None;
        session.fast_mode_state = model::FastModeState::Off;
        session.session_usage = crate::app::state::SessionUsageState::default();
        session.account_info = None;
        session.oauth_credentials = None;
        session.mcp = super::super::McpState::default();
        session.session_scope_epoch = session.session_scope_epoch.saturating_add(1);
        tracing::info!(
            target: crate::logging::targets::APP_AUTH,
            event_name = "logout_completed_background",
            message = "logout cleared background session state",
            outcome = "success",
            session_key = %session_key.as_str(),
        );
        return;
    }
    // Clear the session and start a new one. The bridge now checks auth
    // during initialization and will fire AuthRequired immediately.
    app.bump_session_scope_epoch();
    app.clear_session_runtime_identity();
    app.set_account_info(None);
    app.set_oauth_credentials(None);
    *app.mcp_mut() = super::super::McpState::default();
    app.config.pending_session_title_change = None;
    crate::app::usage::reset_for_session_change(app);
    app.force_redraw = true;
    tracing::info!(
        target: crate::logging::targets::APP_AUTH,
        event_name = "logout_completed",
        message = "logout cleared active session state",
        outcome = "success",
    );

    if let Some(conn) = app.conn().cloned() {
        app.pending_command_label = Some("Starting session...".to_owned());
        app.pending_command_ack = None;
        if let Err(e) = start_new_session(app, conn.as_ref(), SessionStartReason::Logout) {
            tracing::error!(
                target: crate::logging::targets::APP_AUTH,
                event_name = "logout_session_restart_failed",
                message = "failed to start replacement session after logout",
                outcome = "failure",
                error_message = %e,
            );
            clear_pending_command(app);
            push_system_message_with_severity(
                app,
                Some(SystemSeverity::Error),
                &format!("Failed to start new session after logout: {e}"),
            );
        }
    } else {
        tracing::warn!(
            target: crate::logging::targets::APP_AUTH,
            event_name = "logout_session_restart_unavailable",
            message = "logout completed without a connection to start a replacement session",
            outcome = "blocked",
            reason = "missing_connection",
        );
        clear_pending_command(app);
        push_system_message_with_severity(
            app,
            Some(SystemSeverity::Warning),
            "Logged out, but no connection available to start a new session.",
        );
    }
}

pub(super) fn handle_session_replaced_event(
    app: &mut App,
    session_id: model::SessionId,
    cwd: String,
    current_model: model::CurrentModel,
    available_models: Vec<model::AvailableModel>,
    mode: Option<super::super::ModeState>,
    history_messages: &[forge_primitives::Message],
) {
    let session_id_for_log = session_id.to_string();
    let history_message_count = history_messages.len();
    let available_model_count = available_models.len();
    super::clear_compaction_state(app, false);
    app.set_pending_cancel_origin(None);
    app.pending_auto_submit_after_cancel = false;
    let prev_session_id = app.session_id().map(ToString::to_string);
    apply_session_cwd(app, cwd);
    *app.available_models_mut() = available_models;
    reset_for_new_session(app, session_id, current_model, mode, false);
    refresh_session_git_watcher(app, prev_session_id);
    app.sync_welcome_snapshot();
    if !history_messages.is_empty() {
        load_resume_history(app, history_messages);
    }
    clear_pending_command(app);
    app.resuming_session_id = None;
    crate::app::file_index::restart(app);
    crate::app::config::refresh_runtime_tabs_for_session_change(app);
    tracing::info!(
        target: crate::logging::targets::APP_SESSION,
        event_name = "session_replaced",
        message = "replacement session applied",
        outcome = "success",
        session_id = %session_id_for_log,
        cwd = %app.cwd_raw(),
        current_model = ?app.current_model().map(|model| model.resolved_id.clone()),
        history_message_count,
        available_model_count,
    );
}

pub(super) fn handle_service_status_event(
    app: &mut App,
    severity: ServiceStatusSeverity,
    message: &str,
) {
    let ui_severity = match severity {
        ServiceStatusSeverity::Warning => SystemSeverity::Warning,
        ServiceStatusSeverity::Error => SystemSeverity::Error,
    };
    push_system_message_with_severity(app, Some(ui_severity), message);
    match severity {
        ServiceStatusSeverity::Warning => tracing::warn!(
            target: crate::logging::targets::APP_NETWORK,
            event_name = "service_status_applied",
            message = "service status warning applied",
            outcome = "success",
            severity = ?severity,
            service_message = %message,
        ),
        ServiceStatusSeverity::Error => tracing::error!(
            target: crate::logging::targets::APP_NETWORK,
            event_name = "service_status_applied",
            message = "service status error applied",
            outcome = "success",
            severity = ?severity,
            service_message = %message,
        ),
    }
}

pub(super) fn handle_fatal_error_event(app: &mut App, error: AppError) {
    app.finalize_turn_runtime_artifacts(model::ToolCallStatus::Failed);
    app.clear_active_turn_assistant();
    app.exit_error = Some(error);
    app.should_quit = true;
    app.status = AppStatus::Error;
    app.pending_submit = None;
    app.pending_command_label = None;
    app.pending_command_ack = None;
}

/// Clear the `CommandPending` state and restore `Ready`.
pub(super) fn clear_pending_command(app: &mut App) {
    app.pending_command_label = None;
    app.pending_command_ack = None;
    app.status = AppStatus::Ready;
}

fn push_connection_error_message(app: &mut App, error: &str) {
    let message = format!("Connection failed: {error}\n\n{TURN_ERROR_INPUT_LOCK_HINT}");
    push_system_message_with_severity(app, None, &message);
}

fn shorten_cwd_display(cwd_raw: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        let home_str = home.to_string_lossy();
        if cwd_raw.starts_with(home_str.as_ref()) {
            return format!("~{}", &cwd_raw[home_str.len()..]);
        }
    }
    cwd_raw.to_owned()
}

fn sync_welcome_cwd(app: &mut App) {
    app.sync_welcome_snapshot();
}

pub(super) fn apply_session_cwd(app: &mut App, cwd_raw: String) {
    let display = shorten_cwd_display(&cwd_raw);
    app.set_cwd_raw(cwd_raw);
    app.set_cwd(display);
    sync_welcome_cwd(app);
    app.reconcile_trust_state_from_preferences_and_cwd();
}

/// Restart the bridge-side git watcher for the current session's
/// cwd. Must be called AFTER `apply_session_cwd` (so `app.cwd_raw`
/// is set) AND AFTER `reset_for_new_session` (so `app.session_id`
/// is set to the new session). If `prev_session_id` is `Some`, its
/// watcher is stopped first to prevent the bridge worker from
/// accumulating zombie watchers across replaced sessions.
pub(super) fn refresh_session_git_watcher(app: &App, prev_session_id: Option<String>) {
    let Some(conn) = app.conn().cloned() else {
        return;
    };
    if let Some(prev) = prev_session_id
        && let Err(err) = conn.stop_git_context_watch(prev)
    {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            error = %err,
            "failed to stop previous git context watcher",
        );
    }
    let Some(session_id) = app.session_id() else {
        return;
    };
    let cwd = std::path::PathBuf::from(app.cwd_raw());
    if let Err(err) = conn.start_git_context_watch(session_id.to_string(), cwd) {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            error = %err,
            "failed to start git context watcher for session",
        );
    }
}

fn reconcile_session_picker_selection(app: &mut App, selected_session_id: Option<&str>) {
    let session_count = super::super::session_picker::picker_session_count(app);
    if session_count == 0 {
        app.session_picker.selected = 0;
        app.session_picker.scroll_offset = 0;
        return;
    }

    if let Some(session_id) = selected_session_id
        && let Some(idx) =
            app.recent_sessions.iter().position(|session| session.session_id == session_id)
        && idx < session_count
    {
        app.session_picker.selected = idx;
    } else {
        app.session_picker.selected =
            app.session_picker.selected.min(session_count.saturating_sub(1));
    }
    app.session_picker.scroll_offset =
        app.session_picker.scroll_offset.min(app.session_picker.selected);
}

fn maybe_open_startup_session_picker(app: &mut App) {
    if !app.startup_session_picker_requested || app.startup_session_picker_resolved {
        return;
    }
    if app.conn().is_none() || !app.startup_recent_sessions_loaded {
        return;
    }

    app.startup_session_picker_resolved = true;
    let session_count = super::super::session_picker::picker_session_count(app);
    if session_count == 0 {
        push_system_message_with_severity(
            app,
            Some(SystemSeverity::Info),
            "No recent sessions found for this directory; continuing with a new session.",
        );
        return;
    }

    app.session_picker.selected = app.session_picker.selected.min(session_count - 1);
    app.session_picker.scroll_offset = 0;
    view::set_active_view(app, ActiveView::SessionPicker);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::app::file_index::FileCandidate;
    use std::time::{Duration, Instant, SystemTime};

    fn wait_for(app: &mut App, timeout: Duration, mut predicate: impl FnMut(&App) -> bool) {
        let start = Instant::now();
        while start.elapsed() < timeout {
            crate::app::file_index::drain_events(app);
            if predicate(app) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        crate::app::file_index::drain_events(app);
        assert!(predicate(app), "condition not met before timeout");
    }

    fn candidate(rel_path: &str) -> FileCandidate {
        FileCandidate {
            rel_path: rel_path.to_owned(),
            rel_path_lower: rel_path.to_lowercase(),
            basename_lower: rel_path.rsplit('/').next().unwrap_or(rel_path).to_lowercase(),
            depth: rel_path.matches('/').count(),
            modified: SystemTime::UNIX_EPOCH,
            is_dir: rel_path.ends_with('/'),
        }
    }

    #[test]
    fn connected_refreshes_file_index_candidates_for_new_cwd() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("new.rs"), "").expect("write file");
        let mut app = App::test_default();
        app.file_index.generation = 3;
        app.file_index.entries.insert("stale.rs".to_owned(), candidate("stale.rs"));
        app.file_index.scan_finished = true;

        handle_connected_client_event(
            &mut app,
            model::SessionId::new("session-1"),
            dir.path().to_string_lossy().into_owned(),
            model::CurrentModel::new("model", "model", "model").authoritative(true),
            Vec::new(),
            None,
            &[],
            None,
        );

        assert_eq!(app.file_index.root.as_deref(), Some(dir.path()));
        assert!(app.file_index.generation > 3);
        assert!(app.file_index.scan.is_some());
        assert!(app.file_index.watch.is_some());
        assert!(app.file_index.entries.is_empty());
        assert!(app.mention.is_none());
        wait_for(&mut app, Duration::from_secs(2), |app| {
            app.file_index.scan_finished && app.file_index.entries.contains_key("new.rs")
        });
        assert_eq!(
            app.file_index.entries.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["new.rs"]
        );
    }

    #[test]
    fn session_replaced_refreshes_file_index_candidates_for_replaced_cwd() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("after.rs"), "").expect("write file");
        let mut app = App::test_default();
        app.file_index.generation = 8;
        app.file_index.entries.insert("before.rs".to_owned(), candidate("before.rs"));
        app.file_index.scan_finished = true;

        handle_session_replaced_event(
            &mut app,
            model::SessionId::new("session-2"),
            dir.path().to_string_lossy().into_owned(),
            model::CurrentModel::new("model", "model", "model").authoritative(true),
            Vec::new(),
            None,
            &[],
        );

        assert_eq!(app.file_index.root.as_deref(), Some(dir.path()));
        assert!(app.file_index.generation > 8);
        assert!(app.file_index.scan.is_some());
        assert!(app.file_index.watch.is_some());
        assert!(app.file_index.entries.is_empty());
        assert!(app.mention.is_none());
        wait_for(&mut app, Duration::from_secs(2), |app| {
            app.file_index.scan_finished && app.file_index.entries.contains_key("after.rs")
        });
        assert_eq!(
            app.file_index.entries.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["after.rs"]
        );
    }

    #[test]
    fn is_synthetic_key_recognises_double_underscore_pattern() {
        assert!(is_synthetic_key(&forge_workspace::SessionKey::from_session_id(
            "__conn_pending__"
        )));
        assert!(is_synthetic_key(&forge_workspace::SessionKey::from_session_id("__spawn_forge__")));
        assert!(is_synthetic_key(&forge_workspace::SessionKey::from_session_id("____")));
        // Real session UUIDs never look like sentinels.
        assert!(!is_synthetic_key(&forge_workspace::SessionKey::from_session_id(
            "abc123-def456-uuid-shape"
        )));
        // Half-open patterns aren't sentinels.
        assert!(!is_synthetic_key(&forge_workspace::SessionKey::from_session_id("__notclosed")));
        assert!(!is_synthetic_key(&forge_workspace::SessionKey::from_session_id("notopen__")));
        // Empty or shorter-than-`__` prefixes don't qualify.
        assert!(!is_synthetic_key(&forge_workspace::SessionKey::from_session_id("")));
        assert!(!is_synthetic_key(&forge_workspace::SessionKey::from_session_id("_")));
    }

    /// Connected fired against an active `__spawn_<name>__` synthetic
    /// bucket must migrate the bucket onto the real session key —
    /// preserving the cwd, lifecycle state, and any messages the
    /// pre-Connect path accumulated.
    #[test]
    fn connected_migrates_spawn_synthetic_bucket_onto_real_key() {
        let mut app = App::test_default();
        // Set up a `__spawn_forge__` bucket as if the user had just
        // clicked a sleeping project.
        let spawn_key = forge_workspace::SessionKey::from_session_id("__spawn_forge__".to_owned());
        let mut bucket = crate::app::session::Session::new(spawn_key.clone());
        bucket.lifecycle_state = crate::app::session::SessionLifecycleState::Spawning;
        bucket.cwd_raw = "~/Projects/forge".to_owned();
        bucket.cwd = "~/Projects/forge".to_owned();
        // Strip the test_default's pre-Connect bucket out so the
        // active key points at our spawn bucket and nothing else.
        app.sessions.clear();
        app.sessions.insert(spawn_key.clone(), bucket);
        app.active_session_key = Some(spawn_key.clone());

        let dir = tempfile::tempdir().expect("tempdir");
        handle_connected_client_event(
            &mut app,
            model::SessionId::new("real-uuid-9000"),
            dir.path().to_string_lossy().into_owned(),
            model::CurrentModel::new("model", "model", "model").authoritative(true),
            Vec::new(),
            None,
            &[],
            Some(spawn_key.clone()),
        );

        let real_key = forge_workspace::SessionKey::from_session_id("real-uuid-9000".to_owned());
        assert!(
            !app.sessions.contains_key(&spawn_key),
            "spawn synthetic bucket removed after migration"
        );
        assert!(app.sessions.contains_key(&real_key), "real-key bucket present after migration");
        assert_eq!(
            app.active_session_key.as_ref(),
            Some(&real_key),
            "active session is the real key after migration"
        );
        let migrated = app.sessions.get(&real_key).expect("real-key bucket");
        assert_eq!(
            migrated.cwd_raw,
            dir.path().to_string_lossy().into_owned(),
            "cwd updated to the Connected-supplied cwd",
        );
    }

    /// Backwards-compat: when the active key is the legacy
    /// `__conn_pending__` sentinel, Connected still migrates the
    /// pre-Connect bucket as before.
    #[test]
    fn connected_migrates_legacy_pre_connect_sentinel() {
        let mut app = App::test_default();
        // test_default already seeds a `__conn_pending__` bucket as
        // active; just confirm the pre-conditions then drive Connected.
        let pre = forge_workspace::SessionKey::from_session_id(App::PRE_CONNECT_KEY);
        assert!(app.sessions.contains_key(&pre));
        assert_eq!(app.active_session_key.as_ref(), Some(&pre));

        let dir = tempfile::tempdir().expect("tempdir");
        handle_connected_client_event(
            &mut app,
            model::SessionId::new("real-uuid-legacy"),
            dir.path().to_string_lossy().into_owned(),
            model::CurrentModel::new("model", "model", "model").authoritative(true),
            Vec::new(),
            None,
            &[],
            None,
        );

        let real_key = forge_workspace::SessionKey::from_session_id("real-uuid-legacy".to_owned());
        assert!(!app.sessions.contains_key(&pre), "pre-connect synthetic bucket removed");
        assert!(app.sessions.contains_key(&real_key));
        assert_eq!(app.active_session_key.as_ref(), Some(&real_key));
    }
}
