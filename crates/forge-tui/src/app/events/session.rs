use super::super::connect::{SessionStartReason, start_new_session};
use super::super::state::RecentSessionInfo;
use super::super::view::{self, ActiveView};
use super::super::{
    App, AppStatus, ChatMessage, LoginHint, MessageBlock, MessageRole, SystemSeverity, TextBlock,
};
use super::push_system_message_with_severity;
use super::session_reset::{load_resume_history, reset_for_new_session};
use super::set_lifecycle_state_in_workspace;
use crate::agent::model;
use crate::error::AppError;
use forge_primitives::cloud::service_status::ServiceSeverity;
use forge_workspace::SessionKey;
use std::sync::Arc;

const TURN_ERROR_INPUT_LOCK_HINT: &str =
    "Input disabled after an error. Press Ctrl+Q to quit and try again.";

/// Bump the `session_scope_epoch` field on the workspace's
/// `DomainSession` for `key`. Background-session paths (auth-required
/// for non-active session, logout for non-active session, connection
/// failure for non-active session) use this helper instead of the
/// `App::bump_session_scope_epoch` accessor, which targets the
/// *active* session. No-op when no workspace is bound or no domain
/// handle is registered for `key`.
fn bump_session_scope_epoch_in_workspace(app: &App, key: &SessionKey) {
    let Some(workspace) = app.workspace.as_ref() else { return };
    if let Some(domain) = workspace.domain_session_for(key) {
        let mut guard = domain.lock();
        guard.session_scope_epoch = guard.session_scope_epoch.saturating_add(1);
    }
}

/// Post-migration apply chain for `Connected` events.
///
/// Runs the welcome/file-index/runtime-tabs/trust/tab-title work that
/// follows the synthetic-key → real-key migration. The migration
/// itself runs via `SessionUpdate::KeyRenamed` (emitted by
/// `SessionTask::translate_event` ahead of the matching `Connected`).
///
/// `was_active` indicates whether the user is watching this session
/// (active path: full apply chain) or whether it completed in the
/// background (background path: write into the bucket directly).
#[allow(clippy::too_many_arguments, clippy::needless_pass_by_value)]
fn apply_connected_presentation(
    app: &mut App,
    session_key: SessionKey,
    session_id: model::SessionId,
    cwd: String,
    current_model: model::CurrentModel,
    available_models: Vec<model::AvailableModel>,
    mode: Option<super::super::ModeState>,
    history_messages: &[forge_primitives::Message],
    conn: Arc<forge_workspace::AgentHandle>,
    was_active: bool,
) {
    let session_id_for_log = session_id.to_string();
    let history_message_count = history_messages.len();
    let available_model_count = available_models.len();
    let prev_session_id = app.session_id().map(|s| s.to_string());
    if was_active {
        // Active path: the user is watching this session. Run the
        // full active-session apply chain so welcome / file-index /
        // runtime-tabs all sync.
        app.active_session_key = Some(session_key.clone());
        if let Some(bucket) = app.sessions.get_mut(&session_key) {
            bucket.conn = Some(conn);
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
        crate::app::tab_title::update_tab_title(&app.status, app.spinner_frame, app.cwd());
    } else {
        // Background path: temp-swap `active_session_key` so the
        // App-level message + viewport accessors land on the migrated
        // bucket. Without this the bucket keeps its placeholder
        // "Waking …" message as its only content. User-visible UI
        // state (input draft, status) is snapshotted across the
        // swap.
        //
        // Welcome + history replay run via an RAII active-bucket
        // pivot scope (see `app::active_bucket_scope::Scope`). The
        // pivot lets `build_welcome_message` / `load_resume_history`
        // address the migrated bucket via the App-level accessors,
        // and the guard restores the user's actual `App.input` /
        // `App.status` / `App.active_session_key` on drop so the
        // session the user is currently looking at is never visibly
        // disturbed.
        let display = shorten_cwd_display(&cwd);
        let session_id_for_domain = session_id.clone();
        if let Some(bucket) = app.sessions.get_mut(&session_key) {
            bucket.conn = Some(conn);
            bucket.cwd = display;
            bucket.current_model = Some(current_model);
            bucket.mode = mode;
            bucket.available_models = available_models;
        }
        // Mirror cwd_raw + session_id onto the DomainSession
        // (post-Phase 5 authoritative source for the projection).
        if let Some(workspace) = app.workspace.as_ref() {
            workspace.set_cwd_raw_in_domain(&session_key, cwd);
            workspace.set_session_id_in_domain(
                &session_key,
                Some(forge_primitives::SessionId::new(session_id_for_domain.to_string())),
            );
        }
        crate::app::active_bucket_scope::with_pivoted(app, session_key.clone(), |app| {
            app.clear_messages_tracked();
            let welcome = app.build_welcome_message();
            app.push_message_tracked(welcome);
            app.sync_welcome_snapshot();
            *app.active_viewport_mut() = super::super::ChatViewport::new();
            if !history_messages.is_empty() {
                load_resume_history(app, history_messages);
            }
        });
        app.needs_redraw = true;
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
        // Bump workspace's epoch BEFORE the bucket's mut borrow.
        bump_session_scope_epoch_in_workspace(app, session_key);
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
        session.key = None;
        session.current_model = None;
        session.mode = None;
        session.fast_mode_state = model::FastModeState::Off;
        session.session_usage = crate::app::state::SessionUsageState::default();
        session.last_rate_limit_update = None;
        session.cancelled_turn_pending_hint = false;
        session.pending_cancel_origin = None;
        session.mcp = super::super::McpState::default();
        super::turn::finalize_background_tool_calls(session, model::ToolCallStatus::Failed);
        session.active_turn_assistant_message_idx = None;
        session.turn_notice_refs.clear();
        let _ = session;
        if let Some(workspace) = app.workspace.as_ref() {
            workspace.set_account_info_in_domain(session_key, None);
            workspace.set_session_id_in_domain(session_key, None);
        }
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
    let is_rate_limited = is_rate_limited_failure(msg);
    if app.active_session_key.as_ref() != Some(session_key) {
        // Bump workspace's epoch BEFORE acquiring the session's mut
        // borrow — the bump needs `&app.workspace` and the mut borrow
        // would conflict.
        bump_session_scope_epoch_in_workspace(app, session_key);
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
        session.key = None;
        session.current_model = None;
        session.mode = None;
        session.fast_mode_state = model::FastModeState::Off;
        session.session_usage = crate::app::state::SessionUsageState::default();
        session.cancelled_turn_pending_hint = false;
        session.pending_cancel_origin = None;
        session.last_rate_limit_update = None;
        session.mcp = super::super::McpState::default();
        super::turn::finalize_background_tool_calls(session, model::ToolCallStatus::Failed);
        session.active_turn_assistant_message_idx = None;
        session.turn_notice_refs.clear();
        let next_state = if is_rate_limited {
            crate::app::session::SessionLifecycleState::Attention
        } else {
            crate::app::session::SessionLifecycleState::Sleeping
        };
        // Rate-limit fallback message in the bucket's own chat
        // buffer so a future switch shows the explainer. Other
        // failures stay quiet on a background bucket — the user
        // didn't choose to look at this session and we don't want
        // to clutter its history with unrelated errors.
        if is_rate_limited {
            session.messages.push(ChatMessage::new(
                MessageRole::System(Some(SystemSeverity::Warning)),
                vec![MessageBlock::Text(TextBlock::from_complete(RATE_LIMIT_FALLBACK_MESSAGE))],
                None,
            ));
            session.message_retained_bytes.push(0);
        }
        // Drop the `session` mut borrow before reaching for `app.workspace`.
        let _ = session;
        set_lifecycle_state_in_workspace(app, session_key, next_state);
        if let Some(workspace) = app.workspace.as_ref() {
            workspace.set_account_info_in_domain(session_key, None);
            workspace.set_session_id_in_domain(session_key, None);
        }
        tracing::error!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "session_connection_failed_background",
            message = "background session connection failure applied",
            outcome = "failure",
            session_key = %session_key.as_str(),
            error_message = %msg,
            is_rate_limited,
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
    app.input_mut().clear();
    app.pending_submit = None;
    app.status = AppStatus::Error;
    app.clear_active_turn_assistant();
    super::notices::clear_turn_notice_tracking(app);
    // Lifecycle: rate-limit failure → Attention; other failures →
    // Sleeping. Matches the background-bucket branch.
    let next_state = if is_rate_limited {
        crate::app::session::SessionLifecycleState::Attention
    } else {
        crate::app::session::SessionLifecycleState::Sleeping
    };
    set_lifecycle_state_in_workspace(app, session_key, next_state);
    if is_rate_limited {
        push_system_message_with_severity(
            app,
            Some(SystemSeverity::Warning),
            RATE_LIMIT_FALLBACK_MESSAGE,
        );
    } else {
        push_connection_error_message(app, msg);
    }
    tracing::error!(
        target: crate::logging::targets::APP_SESSION,
        event_name = "session_connection_failed",
        message = "session connection failure applied",
        outcome = "failure",
        error_message = %msg,
        is_rate_limited,
    );
}

/// Spec text for the rate-limit fallback chat message. See
/// `~/.claude-subspace/plans/2026-05-10-forge-tui-projects-pane-wide-design.md`.
const RATE_LIMIT_FALLBACK_MESSAGE: &str =
    "Waiting for account reset; click another project or wait.";

/// Returns true when the connection-failed message looks like
/// "all accounts are rate-limited". Workspace doesn't surface a
/// typed error variant for this yet, so we fall back to substring
/// matching on the rendered message. False positives are
/// preferable to false negatives — if the heuristic misfires the
/// user sees the rate-limit explainer instead of the raw error,
/// which is still recoverable (click another project, wait).
fn is_rate_limited_failure(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    (lower.contains("rate") && lower.contains("limit"))
        || lower.contains("rate-limited")
        || lower.contains("rate_limited")
        || lower.contains("all accounts")
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
    app.active_viewport_mut().engage_auto_scroll();
    clear_pending_command(app);
    app.resuming_session_id = None;
}

pub(super) fn handle_auth_completed_event(
    app: &mut App,
    session_key: &SessionKey,
    conn: &Arc<forge_workspace::AgentHandle>,
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
        // Bump workspace's epoch BEFORE the bucket's mut borrow.
        bump_session_scope_epoch_in_workspace(app, session_key);
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
        session.key = None;
        session.current_model = None;
        session.mode = None;
        session.fast_mode_state = model::FastModeState::Off;
        session.session_usage = crate::app::state::SessionUsageState::default();
        session.oauth_credentials = None;
        session.mcp = super::super::McpState::default();
        let _ = session;
        if let Some(workspace) = app.workspace.as_ref() {
            workspace.set_account_info_in_domain(session_key, None);
            workspace.set_session_id_in_domain(session_key, None);
        }
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

#[allow(clippy::too_many_arguments)]
pub(super) fn handle_session_replaced_event(
    app: &mut App,
    session_id: model::SessionId,
    cwd: String,
    current_model: model::CurrentModel,
    available_models: Vec<model::AvailableModel>,
    mode: Option<super::super::ModeState>,
    history_messages: &[forge_primitives::Message],
    conn: Arc<forge_workspace::AgentHandle>,
) {
    let session_id_for_log = session_id.to_string();
    let session_key = SessionKey::from_session_id(session_id.to_string());
    let history_message_count = history_messages.len();
    let available_model_count = available_models.len();
    super::clear_compaction_state(app, false);
    app.set_pending_cancel_origin(None);
    app.pending_auto_submit_after_cancel = false;
    let prev_session_id = app.session_id().map(|s| s.to_string());

    // Capture the outgoing bucket's key BEFORE `reset_for_new_session`
    // runs — that call goes through `set_session_id`, which inserts a
    // fresh bucket under the new session_id and flips
    // `active_session_key` to it. The outgoing bucket is then orphaned
    // in `app.sessions` and needs to be removed; without the capture
    // we'd lose track of which key to drop.
    let prev_active_key = app.active_session_key.clone();

    *app.available_models_mut() = available_models;
    reset_for_new_session(app, session_id, current_model, mode, false);

    // Install the AgentHandle that the bridge routed alongside this
    // event onto the freshly-inserted bucket. The bridge swapped its
    // Client to the new CLI session (see
    // `forge_sdk_worker::spawn_session`); the Arc identity is the
    // same as before but we don't have to dig it out of the outgoing
    // bucket — the event carries it directly.
    if let Some(new_bucket) = app.session_mut(&session_key) {
        new_bucket.conn = Some(conn);
    }

    // Apply cwd AFTER the bucket swap so it lands on the new bucket.
    // Pre-swap it would write to the now-abandoned outgoing bucket
    // and the new welcome card would render an empty path.
    apply_session_cwd(app, cwd);
    refresh_session_git_watcher(app, prev_session_id);
    app.sync_welcome_snapshot();
    if !history_messages.is_empty() {
        load_resume_history(app, history_messages);
    }
    clear_pending_command(app);
    app.resuming_session_id = None;
    crate::app::file_index::restart(app);
    crate::app::config::refresh_runtime_tabs_for_session_change(app);

    // Drop the now-orphaned outgoing bucket. The CLI replaced the
    // underlying session (the previous subprocess is gone), and the
    // user explicitly asked for a fresh start, so its chat history /
    // tool-call indices / viewport scroll are not reachable from
    // anywhere in the UI. Leaving it behind would have the Projects
    // pane resolve "click forge" through the orphan instead of the
    // new bucket, which is exactly the bug this whole sequence fixes.
    if let Some(prev) = prev_active_key
        && prev != session_key
    {
        app.sessions.remove(&prev);
    }

    // Workspace catalog is now updated by
    // `Workspace::record_event_for_domain` on the
    // `AgentEvent::SessionReplaced` arm (Phase 3a); no TUI-side
    // write is needed here.

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

fn present_service_status(app: &mut App, severity: ServiceSeverity, message: &str) {
    let ui_severity = match severity {
        ServiceSeverity::Warning => SystemSeverity::Warning,
        ServiceSeverity::Error => SystemSeverity::Error,
    };
    push_system_message_with_severity(app, Some(ui_severity), message);
    match severity {
        ServiceSeverity::Warning => tracing::warn!(
            target: crate::logging::targets::APP_NETWORK,
            event_name = "service_status_applied",
            message = "service status warning applied",
            outcome = "success",
            severity = ?severity,
            service_message = %message,
        ),
        ServiceSeverity::Error => tracing::error!(
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
    // `resume_session` paths (startup + Projects-pane lead-resume +
    // in-session `/resume`) emit `Connected` with an empty cwd — the
    // resumed session's cwd isn't re-derived by `forge-sdk-worker`.
    // The spawn-side path always pre-seeds the bucket's `cwd_raw`
    // before Connected can fire, so when Connected delivers an empty
    // value we keep what's there rather than blanking the welcome
    // card to `"-"` and triggering a spurious trust re-check against
    // an empty path.
    if cwd_raw.is_empty() {
        return;
    }
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

// ─────────────────────────────────────────────────────────────────
// `SessionUpdate` reducers for the 11 session-lifecycle events.
//
// Each `apply_session_update_*` function consumes the corresponding
// `forge_workspace::SessionUpdate` variant, converts wire types to
// TUI runtime model types, and dispatches to the matching
// presentation helper.
// ─────────────────────────────────────────────────────────────────

// Each reducer below receives owned values from the SessionUpdate
// destructure in `events::client::apply_workspace_update`. Several
// of them merely forward references into the underlying handler;
// `#[allow(clippy::needless_pass_by_value)]` is the standard escape
// for that "I own this but only pass it by reference" pattern.

#[allow(clippy::too_many_arguments, clippy::needless_pass_by_value)]
pub(super) fn apply_session_update_connected(
    app: &mut App,
    key: SessionKey,
    session_id: forge_primitives::SessionId,
    cwd: String,
    current_model: forge_primitives::CurrentModel,
    available_models: Vec<forge_primitives::AvailableModel>,
    mode: Option<forge_primitives::ModeState>,
    history: Vec<forge_primitives::Message>,
    conn: Arc<forge_workspace::AgentHandle>,
) {
    use super::super::connect::type_converters::{
        convert_current_model, convert_mode_state, map_available_models,
    };
    // Defensive synthetic→real migration: in production, the
    // `SessionTask` emits `SessionUpdate::KeyRenamed` ahead of
    // `Connected` so the bucket already lives at `key`. Tests that
    // fire `Connected` directly (without the prior KeyRenamed) and
    // legacy single-session bridges still rely on this reducer to
    // do the migration when the active key is a synthetic
    // placeholder.
    let synthetic_to_migrate = if !app.sessions.contains_key(&key)
        && let Some(active_key) = app.active_session_key.clone()
        && is_synthetic_key(&active_key)
    {
        Some(active_key)
    } else {
        None
    };
    if let Some(synth_key) = synthetic_to_migrate.as_ref() {
        if let Some(mut existing) = app.sessions.remove(synth_key) {
            existing.key = Some(key.clone());
            app.sessions.insert(key.clone(), existing);
            app.active_session_key = Some(key.clone());
            // Mirror the bucket re-key onto the workspace's
            // `DomainSession` handle map so the migrated bucket's
            // accessors (`cwd_raw`, `session_id`, …) keep resolving.
            if let Some(workspace) = app.workspace.as_ref() {
                workspace.rekey_domain_session(synth_key, key.clone());
            }
        }
    } else {
        app.sessions
            .entry(key.clone())
            .or_insert_with(|| crate::app::session::UiSession::new(key.clone()));
        // Ensure a workspace-side DomainSession exists for `key`. In
        // production, `SessionTask` already registered one before
        // emitting `Connected`; this branch covers tests that
        // synthesize `SessionUpdate::Connected` directly and the
        // legacy single-session SessionReplaced shape.
        if let Some(workspace) = app.workspace.as_ref()
            && workspace.domain_session_for(&key).is_none()
        {
            workspace.register_domain_session(key.clone(), None);
        }
    }
    set_lifecycle_state_in_workspace(app, &key, crate::app::session::SessionLifecycleState::Idle);
    // Match legacy single-session semantics: when there's no
    // synthetic to migrate (single-session session-reset path /
    // SessionReplaced semantics arriving as Connected), default to
    // the active-path apply chain so a fresh Connected resets the
    // visible session state. When there IS a synthetic migration,
    // honour whether the user is still on the migrated bucket
    // (active path) versus already switched away (background path).
    let was_active = if synthetic_to_migrate.is_some() {
        app.active_session_key.as_ref() == Some(&key)
    } else {
        true
    };
    // Ensure active_session_key follows when was_active is true.
    if was_active && app.active_session_key.as_ref() != Some(&key) {
        app.active_session_key = Some(key.clone());
    }
    apply_connected_presentation(
        app,
        key,
        model::SessionId::new(session_id.into_string()),
        cwd,
        convert_current_model(current_model),
        map_available_models(available_models),
        mode.map(convert_mode_state),
        &history,
        conn,
        was_active,
    );
}

/// Sentinel-pattern check: synthetic keys (`__conn_pending__`,
/// `__spawn_<project>__`, `__resume_<id>__`) all wrap a name in
/// double underscores. Real claude session UUIDs never look like
/// this.
fn is_synthetic_key(key: &SessionKey) -> bool {
    let s = key.as_str();
    s.len() >= 4 && s.starts_with("__") && s.ends_with("__")
}

#[allow(clippy::too_many_arguments, clippy::needless_pass_by_value)]
pub(super) fn apply_session_update_session_replaced(
    app: &mut App,
    _key: SessionKey,
    session_id: forge_primitives::SessionId,
    cwd: String,
    current_model: forge_primitives::CurrentModel,
    available_models: Vec<forge_primitives::AvailableModel>,
    mode: Option<forge_primitives::ModeState>,
    history: Vec<forge_primitives::Message>,
    conn: Arc<forge_workspace::AgentHandle>,
) {
    use super::super::connect::type_converters::{
        convert_current_model, convert_mode_state, map_available_models,
    };
    handle_session_replaced_event(
        app,
        model::SessionId::new(session_id.into_string()),
        cwd,
        convert_current_model(current_model),
        map_available_models(available_models),
        mode.map(convert_mode_state),
        &history,
        conn,
    );
}

pub(super) fn apply_session_update_sessions_listed(
    app: &mut App,
    sessions: Vec<forge_primitives::SessionListEntry>,
) {
    handle_sessions_listed_event(app, sessions);
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn apply_session_update_auth_required(
    app: &mut App,
    key: SessionKey,
    method_name: String,
    method_description: String,
) {
    handle_auth_required_event(app, &key, method_name, method_description);
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn apply_session_update_connection_failed(
    app: &mut App,
    key: SessionKey,
    message: String,
    _fatal: bool,
) {
    handle_connection_failed_event(app, &key, &message);
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn apply_session_update_slash_command_error(
    app: &mut App,
    key: SessionKey,
    message: String,
) {
    handle_slash_command_error_event(app, &key, &message);
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn apply_session_update_auth_completed(
    app: &mut App,
    key: SessionKey,
    conn: Arc<forge_workspace::AgentHandle>,
) {
    handle_auth_completed_event(app, &key, &conn);
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn apply_session_update_logout_completed(app: &mut App, key: SessionKey) {
    handle_logout_completed_event(app, &key);
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn apply_session_update_service_status(
    app: &mut App,
    severity: forge_primitives::cloud::service_status::ServiceSeverity,
    message: String,
) {
    present_service_status(app, severity, &message);
}

pub(super) fn apply_session_update_fatal_error(app: &mut App, error: AppError) {
    handle_fatal_error_event(app, error);
}
