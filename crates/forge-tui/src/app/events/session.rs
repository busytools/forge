use super::super::state::RecentSessionInfo;
use super::super::{
    App, AppStatus, ChatMessage, LoginHint, MessageBlock, MessageRole, SystemSeverity, TextBlock,
};
use super::push_system_message_with_severity;
use super::session_reset::{load_resume_history, reset_for_new_session};
use super::set_bucket_lifecycle_state;
use crate::agent::model;
use crate::app::session::SessionLifecycleState;
use crate::error::AppError;
use forge_primitives::cloud::service_status::ServiceSeverity;
use forge_workspace::SessionKey;

const TURN_ERROR_INPUT_LOCK_HINT: &str =
    "Input disabled after an error. Press Ctrl+Q to quit and try again.";

/// Bump the `session_scope_epoch` field on the bucket for `key`.
/// Background-session paths (auth-required for non-active session,
/// logout for non-active session, connection failure for non-active
/// session) use this helper instead of the
/// `App::bump_session_scope_epoch` accessor, which targets the
/// *active* session. No-op when no bucket is registered for `key`.
fn bump_bucket_session_scope_epoch(app: &mut App, key: &SessionKey) {
    if let Some(bucket) = app.sessions.get_mut(key) {
        bucket.session_scope_epoch = bucket.session_scope_epoch.saturating_add(1);
    }
}

/// Apply chain for `Connected` events that runs after the
/// synthetic-key → real-key migration completes. The migration
/// itself runs via `SessionUpdate::KeyRenamed` (emitted by
/// `SessionTask::translate_event` ahead of the matching `Connected`).
/// This chain handles the welcome / file-index / runtime-tabs / trust
/// / tab-title work that follows.
///
/// `was_active` indicates whether the user is watching this session
/// (active path: full apply chain) or whether it completed in the
/// background (background path: write into the bucket directly).
fn apply_connected_presentation(
    app: &mut App,
    session_key: &SessionKey,
    session_id: model::SessionId,
    cwd: String,
    current_model: model::CurrentModel,
    available_models: Vec<model::AvailableModel>,
    mode: Option<super::super::ModeState>,
    history_messages: &[forge_primitives::Message],
    was_active: bool,
) {
    let session_id_for_log = session_id.to_string();
    let history_message_count = history_messages.len();
    let available_model_count = available_models.len();
    // Log the event's own values: the app-level accessors resolve the
    // user's FOCUSED session, which on a background connect is some
    // unrelated tab. Resume connects deliberately carry an empty event
    // cwd (the bucket was pre-seeded at spawn), so fall back to the
    // connecting bucket's value - read before the arms below rewrite it.
    let cwd_for_log = if cwd.is_empty() {
        app.sessions.get(session_key).map(|b| b.cwd_raw.clone()).unwrap_or_default()
    } else {
        cwd.clone()
    };
    let model_for_log = current_model.clone();
    if was_active {
        // Active path: the user is watching this session. Run the
        // full active-session apply chain so welcome / file-index /
        // runtime-tabs all sync.
        app.active_session_key = Some(session_key.clone());
        apply_session_cwd(app, cwd);
        reset_for_new_session(app, session_id, current_model, mode, true);
        *app.available_models_mut() = available_models;
        app.sync_welcome_snapshot();
        if !history_messages.is_empty() {
            load_resume_history(app, history_messages);
        }
        clear_pending_command(app);
        *app.resuming_session_id_mut() = None;
        crate::app::file_index::restart(app);
        app.rebuild_chat_focus_from_state();
        crate::app::config::refresh_runtime_tabs_for_session_change(app);
        crate::app::tab_title::update_tab_title(app.shows_activity(), app.spinner_frame, app.cwd());
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
        if let Some(bucket) = app.sessions.get_mut(session_key) {
            bucket.cwd = display;
            bucket.cwd_raw = cwd;
            bucket.session_id =
                Some(forge_primitives::SessionId::new(session_id_for_domain.to_string()));
            bucket.current_model = Some(current_model);
            bucket.mode = mode;
            bucket.available_models = available_models;
        }
        set_bucket_lifecycle_state(app, session_key, SessionLifecycleState::Idle);
        // Mirror session_id onto the workspace's DomainSession so
        // AgentHandle dispatch (which routes by claude-issued session
        // UUID) can resolve this bucket.
        if let Some(workspace) = app.workspace.as_ref() {
            workspace.set_session_id_in_domain(
                session_key,
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
        cwd = %cwd_for_log,
        current_model = %model_for_log.resolved_id,
        history_message_count,
        available_model_count,
        was_active,
    );
}

pub(super) fn handle_sessions_listed_event(
    app: &mut App,
    key: &SessionKey,
    sessions: Vec<forge_primitives::SessionListEntry>,
) {
    let session_count = sessions.len();
    let mapped: Vec<RecentSessionInfo> = sessions
        .into_iter()
        .map(|entry| RecentSessionInfo {
            session_id: entry.session_id,
            summary: entry.summary,
            last_modified_ms: entry.last_modified_ms,
            cwd: entry.cwd,
            custom_title: entry.custom_title,
            first_prompt: entry.first_prompt,
        })
        .collect();
    let Some(slot) = app.recent_sessions_mut_for(key) else {
        // Bucket no longer exists - session was closed before the
        // listing landed. Drop silently.
        tracing::debug!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "sessions_listed_dropped",
            outcome = "dropped",
            session_key = %key.as_str(),
            reason = "unknown_bucket",
        );
        return;
    };
    *slot = mapped;
    tracing::info!(
        target: crate::logging::targets::APP_SESSION,
        event_name = "sessions_list_updated",
        message = "sessions list applied",
        outcome = "success",
        session_count,
    );
}

pub(super) fn handle_auth_required_event(
    app: &mut App,
    session_key: &SessionKey,
    method_name: String,
    method_description: String,
) {
    if app.active_session_key.as_ref() != Some(session_key) {
        bump_bucket_session_scope_epoch(app, session_key);
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
        session.session_id = None;
        session.account_info = None;
        session.current_model = None;
        session.mode = None;
        session.runtime_session_state = None;
        session.session_usage = crate::app::state::SessionUsageState::default();
        session.last_rate_limit_update = None;
        session.cancelled_turn_pending_hint = false;
        session.pending_cancel = false;
        session.mcp = super::super::McpState::default();
        // Teardown is a hard terminal - clear the roster first so the sweep
        // has nothing to exempt and every open card fails.
        session.clear_background_task_registry();
        super::turn::finalize_background_tool_calls(session, model::ToolCallStatus::Failed);
        session.active_turn_assistant_message_idx = None;
        session.turn_notice_refs.clear();
        let _ = session;
        // Flip the bucket's lifecycle state so the Projects pane
        // glyph reflects the auth-blocked condition.
        set_bucket_lifecycle_state(app, session_key, SessionLifecycleState::AuthRequired);
        // Mirror session_id reset onto the workspace's DomainSession
        // so AgentHandle dispatch stops routing to a no-longer-valid
        // session id.
        if let Some(workspace) = app.workspace.as_ref() {
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
    *app.resuming_session_id_mut() = None;
    *app.login_hint_mut() = Some(LoginHint { method_name, method_description });
    app.bump_session_scope_epoch();
    app.clear_session_runtime_identity();
    super::clear_compaction_state(app, false);
    app.set_last_rate_limit_update(None);
    app.set_cancelled_turn_pending_hint(false);
    app.set_pending_cancel(false);
    app.set_account_info(None);
    *app.mcp_mut() = super::super::McpState::default();
    crate::app::usage::reset_for_session_change(app);
    // Teardown is a hard terminal - clear the roster first so the sweep
    // has nothing to exempt and every open card fails.
    app.clear_active_session_background_task_registry();
    app.finalize_turn_runtime_artifacts(model::ToolCallStatus::Failed);
    app.clear_active_turn_assistant();
    super::notices::clear_turn_notice_tracking(app);
    // Flip the active bucket's lifecycle state too - if the user
    // switches away from this session while it's auth-blocked, the
    // Projects pane glyph picks up the AuthRequired marker without
    // waiting for a subsequent event.
    if let Some(key) = app.active_session_key.clone() {
        super::set_bucket_lifecycle_state(
            app,
            &key,
            crate::app::session::SessionLifecycleState::AuthRequired,
        );
    }
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
        // borrow - the bump needs `&app.workspace` and the mut borrow
        // would conflict.
        bump_bucket_session_scope_epoch(app, session_key);
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
        session.session_id = None;
        session.account_info = None;
        session.current_model = None;
        session.mode = None;
        session.runtime_session_state = None;
        session.session_usage = crate::app::state::SessionUsageState::default();
        session.cancelled_turn_pending_hint = false;
        session.pending_cancel = false;
        session.last_rate_limit_update = None;
        session.mcp = super::super::McpState::default();
        // Teardown is a hard terminal - clear the roster first so the sweep
        // has nothing to exempt and every open card fails.
        session.clear_background_task_registry();
        super::turn::finalize_background_tool_calls(session, model::ToolCallStatus::Failed);
        session.active_turn_assistant_message_idx = None;
        session.turn_notice_refs.clear();
        let next_state = if is_rate_limited {
            SessionLifecycleState::Attention
        } else {
            SessionLifecycleState::Sleeping
        };
        // Rate-limit fallback message in the bucket's own chat
        // buffer so a future switch shows the explainer. Other
        // failures stay quiet on a background bucket - the user
        // didn't choose to look at this session and we don't want
        // to clutter its history with unrelated errors.
        if is_rate_limited {
            session.messages.push(ChatMessage::new(
                MessageRole::System(Some(SystemSeverity::Warning)),
                vec![MessageBlock::Text(TextBlock::from_complete(RATE_LIMIT_FALLBACK_MESSAGE))],
            ));
            session.message_retained_bytes.push(0);
        }
        // Capture the failure reason on the bucket for the launchpad
        // picker to surface beneath a failed project row. Cleared on
        // a successful reconnect via `clear_connection_error`. Skip
        // for rate-limit failures - those use the `Attention` glyph
        // and the inline RATE_LIMIT_FALLBACK_MESSAGE explainer
        // rather than the per-row error tail.
        if !is_rate_limited {
            session.last_connection_error = Some(msg.to_owned());
        }
        // Drop the `session` mut borrow before reaching for `app.workspace`.
        let _ = session;
        // Non-rate-limited failures land on Failed (visible as `✗`
        // in the launchpad picker); rate-limit gets `Attention`.
        let lifecycle_state =
            if is_rate_limited { next_state } else { SessionLifecycleState::Failed };
        set_bucket_lifecycle_state(app, session_key, lifecycle_state);
        // Mirror session_id reset onto the workspace's DomainSession
        // so AgentHandle dispatch stops routing to a no-longer-valid
        // session id.
        if let Some(workspace) = app.workspace.as_ref() {
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
    app.set_pending_cancel(false);
    app.set_last_rate_limit_update(None);
    app.set_account_info(None);
    *app.mcp_mut() = super::super::McpState::default();
    crate::app::usage::reset_for_session_change(app);
    *app.resuming_session_id_mut() = None;
    *app.pending_command_label_mut() = None;
    *app.pending_command_ack_mut() = None;
    // Teardown is a hard terminal - clear the roster first so the sweep
    // has nothing to exempt and every open card fails.
    app.clear_active_session_background_task_registry();
    app.finalize_turn_runtime_artifacts(model::ToolCallStatus::Failed);
    app.input_mut().clear();
    *app.pending_submit_mut() = None;
    app.status = AppStatus::Error;
    app.clear_active_turn_assistant();
    super::notices::clear_turn_notice_tracking(app);
    // Lifecycle: rate-limit failure → Attention; other failures →
    // Failed (surfaced as the `✗` glyph in the launchpad picker,
    // identical to background failure handling above). Capture the
    // raw message on the bucket so the launchpad's per-row error
    // tail can surface it.
    let next_state = if is_rate_limited {
        crate::app::session::SessionLifecycleState::Attention
    } else {
        crate::app::session::SessionLifecycleState::Failed
    };
    if !is_rate_limited && let Some(session) = app.session_mut(session_key) {
        session.last_connection_error = Some(msg.to_owned());
    }
    set_bucket_lifecycle_state(app, session_key, next_state);
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

/// Chat message rendered when all accounts are rate-limited.
const RATE_LIMIT_FALLBACK_MESSAGE: &str =
    "Waiting for account reset; click another project or wait.";

/// Returns true when the connection-failed message looks like
/// "all accounts are rate-limited". Workspace doesn't surface a
/// typed error variant for this yet, so we fall back to substring
/// matching on the rendered message. False positives are
/// preferable to false negatives - if the heuristic misfires the
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
    app.push_message_tracked(ChatMessage::new(
        MessageRole::System(None),
        vec![MessageBlock::Text(TextBlock::from_complete(msg))],
    ));
    app.enforce_history_retention_tracked();
    app.active_viewport_mut().engage_auto_scroll();
    clear_pending_command(app);
    *app.resuming_session_id_mut() = None;
}

/// Foreground arm: the replaced session is the one on screen, so the
/// full App-level apply chain runs. `previous_key` is the outgoing
/// bucket (equal to `app.active_session_key` on this path); `session_key`
/// is the replacement.
fn handle_session_replaced_event(
    app: &mut App,
    session_key: &SessionKey,
    previous_key: &SessionKey,
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
    app.set_pending_cancel(false);

    // The replacement bucket is minted blank by `reset_for_new_session`
    // (via `set_session_id`), so grab the outgoing tab's project name to
    // carry across the swap (same project, new UUID). Read it before
    // that call orphans the outgoing bucket.
    let carried_project = app.sessions.get(previous_key).and_then(|b| b.project.clone());

    *app.available_models_mut() = available_models;
    reset_for_new_session(app, session_id, current_model, mode, false);

    // The AgentHandle binding lives on the workspace's `DomainSession`
    // (post-strict-wiring) - TUI no longer caches it on the bucket.
    // `SessionTask` re-binds the handle for the replacement session
    // ahead of emitting `SessionReplaced`, so nothing for TUI to do
    // here.

    // Apply cwd AFTER the bucket swap so it lands on the new bucket.
    // Pre-swap it would write to the now-abandoned outgoing bucket
    // and the new welcome card would render an empty path.
    apply_session_cwd(app, cwd);

    // Restamp the tab's project identity onto the fresh bucket: prefer
    // the carried name (robust when the resume cwd arrives empty), else
    // resolve from the new bucket's cwd.
    match carried_project {
        Some(name) => {
            if let Some(bucket) = app.sessions.get_mut(session_key) {
                bucket.project = Some(name);
            }
        }
        None => stamp_bucket_project_from_cwd(app, session_key, true),
    }
    app.sync_welcome_snapshot();
    if !history_messages.is_empty() {
        load_resume_history(app, history_messages);
    }
    clear_pending_command(app);
    *app.resuming_session_id_mut() = None;
    crate::app::file_index::restart(app);
    crate::app::config::refresh_runtime_tabs_for_session_change(app);

    // Drop the now-orphaned outgoing bucket. The CLI replaced the
    // underlying session (the previous subprocess is gone), and the
    // user explicitly asked for a fresh start, so its chat history /
    // tool-call indices / viewport scroll are not reachable from
    // anywhere in the UI. Leaving it behind would have the Projects
    // pane resolve "click forge" through the orphan instead of the
    // new bucket.
    if previous_key != session_key {
        app.sessions.remove(previous_key);
    }

    // Reset lifecycle_state to Idle - the replacement session
    // reuses the same DomainSession, so a previously-set Attention
    // from a pending permission on the outgoing session would
    // otherwise carry forward and leave the Projects pane glyph
    // stale until the next lifecycle event.
    super::set_bucket_lifecycle_state(
        app,
        session_key,
        crate::app::session::SessionLifecycleState::Idle,
    );

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
    *app.pending_submit_mut() = None;
    *app.pending_command_label_mut() = None;
    *app.pending_command_ack_mut() = None;
}

/// Clear the `CommandPending` state and restore `Ready`.
pub(super) fn clear_pending_command(app: &mut App) {
    *app.pending_command_label_mut() = None;
    *app.pending_command_ack_mut() = None;
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
    // in-session `/resume`) emit `Connected` with an empty cwd - the
    // resumed session's cwd isn't re-derived by `forge-sdk-worker`.
    // The spawn-side path always pre-seeds the bucket's `cwd_raw`
    // before Connected can fire, so when Connected delivers an empty
    // value we keep what's there rather than blanking the welcome
    // card to `"-"` and triggering a spurious trust re-check against
    // an empty path.
    if cwd_raw.is_empty() {
        return;
    }
    let cwd_changed = app.cwd_raw() != cwd_raw;
    let display = shorten_cwd_display(&cwd_raw);
    app.set_cwd_raw(cwd_raw);
    app.set_cwd(display);
    sync_welcome_cwd(app);

    // Bump the git-diff generation when the cwd genuinely changed,
    // reset the cached snapshot to `None`, AND abandon the in-flight
    // guard. The next periodic tick repopulates against the new cwd.
    // The generation guard means any in-flight scan against the old
    // cwd lands stale and is dropped by `git_diff::drain_events`.
    // Clearing `scan_in_flight` here lets the next ticker re-fire
    // immediately instead of waiting for the abandoned scan to time
    // out (worst case ~50s on a hung remote mount). The abandoned
    // scan's eventual `store(false)` on its `Arc<AtomicBool>` is
    // harmless - it stores onto the same atomic, which is already
    // `false`. Skips a no-op when the cwd didn't actually change
    // (idempotent Connected re-applies).
    if cwd_changed
        && let Some(key) = app.active_session_key.clone()
        && let Some(session) = app.sessions.get_mut(&key)
    {
        session.git_diff_generation = session.git_diff_generation.saturating_add(1);
        session.git_diff_snapshot = None;
        session.git_diff_last_refreshed_at = None;
        session.git_diff_scan_in_flight.store(false, std::sync::atomic::Ordering::Release);
    }
}

/// Stamp the bucket for `key` with the forge.toml project name that
/// owns its `cwd_raw`, resolved once via the workspace. `force`
/// re-stamps even when a name is already set; otherwise only fills an
/// empty slot so a name a preceding `Spawning` stamped survives. No-op
/// when the cwd maps to no configured project - the slot is left as-is
/// rather than cleared.
fn stamp_bucket_project_from_cwd(app: &mut App, key: &SessionKey, force: bool) {
    let cwd = app.sessions.get(key).map(|b| b.cwd_raw.clone()).unwrap_or_default();
    let Some(name) = app.workspace.as_ref().and_then(|ws| ws.project_name_for_path(&cwd)) else {
        return;
    };
    if let Some(bucket) = app.sessions.get_mut(key)
        && (force || bucket.project.is_none())
    {
        bucket.project = Some(name);
    }
}

// ─────────────────────────────────────────────────────────────────
// `SessionUpdate` reducers for the 11 session-lifecycle events.
//
// Each `apply_session_update_*` function consumes the corresponding
// `forge_workspace::SessionUpdate` variant, converts wire types to
// TUI runtime model types, and dispatches to the matching
// presentation helper.
// ─────────────────────────────────────────────────────────────────

// Reducers below receive owned values from the SessionUpdate
// destructure in `events::client::apply_workspace_update`. The
// module-level `needless_pass_by_value` allow at the top of this
// file covers the "I own this but only pass it by reference" arms.

pub(super) fn apply_session_update_connected(
    app: &mut App,
    key: &SessionKey,
    session_id: forge_primitives::SessionId,
    cwd: String,
    current_model: forge_primitives::CurrentModel,
    available_models: Vec<forge_primitives::AvailableModel>,
    mode: Option<forge_primitives::ModeState>,
    history: &[forge_primitives::Message],
    compaction_count: u32,
) {
    use super::super::connect::type_converters::map_available_models;
    // Defensive synthetic→real migration: in production, the
    // `SessionTask` emits `SessionUpdate::KeyRenamed` ahead of
    // `Connected` so the bucket already lives at `key`. Tests that
    // fire `Connected` directly (without the prior KeyRenamed) and
    // legacy single-session bridges still rely on this reducer to
    // do the migration when the active key is a synthetic
    // placeholder.
    let synthetic_to_migrate = if !app.sessions.contains_key(key)
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
            && workspace.domain_session_for(key).is_none()
        {
            workspace.register_domain_session(key.clone(), None);
        }
    }
    set_bucket_lifecycle_state(app, key, crate::app::session::SessionLifecycleState::Idle);
    // Clear any captured connection error on a successful reconnect -
    // the launchpad picker stops surfacing the stale `✗` row tail.
    if let Some(session) = app.session_mut(key) {
        session.last_connection_error = None;
    }
    // Connected applies welcome/model snapshots to active-session UI
    // only when the key already matches `active_session_key`. Focus
    // routing lives in the `KeyRenamed` reducer, not here.
    let was_active = app.active_session_key.as_ref() == Some(key);
    apply_connected_presentation(
        app,
        key,
        model::SessionId::new(session_id.into_string()),
        cwd,
        current_model,
        map_available_models(available_models),
        mode,
        history,
        was_active,
    );
    // After the presentation chain, not before: its `reset_for_new_session`
    // assigns a whole default `SessionUsageState`, so a seed set earlier
    // is wiped.
    seed_compaction_count(app, key, compaction_count);
    // Resolve the tab's project identity from its (now-applied) cwd,
    // unless a preceding `Spawning` already named it. The boot project
    // and workers reach Connected without a Spawning, so this is where
    // their SCHEDULES / GOTIFY scope gets stamped.
    stamp_bucket_project_from_cwd(app, key, false);
}

/// Sentinel-pattern check: synthetic keys (`__conn_pending__`,
/// `__spawn_<project>__`, `__resume_<id>__`) all wrap a name in
/// double underscores. Real claude session UUIDs never look like
/// this.
fn is_synthetic_key(key: &SessionKey) -> bool {
    let s = key.as_str();
    s.len() >= 4 && s.starts_with("__") && s.ends_with("__")
}

pub(super) fn apply_session_update_session_replaced(
    app: &mut App,
    key: &SessionKey,
    previous_key: &SessionKey,
    session_id: forge_primitives::SessionId,
    cwd: String,
    current_model: forge_primitives::CurrentModel,
    available_models: Vec<forge_primitives::AvailableModel>,
    mode: Option<forge_primitives::ModeState>,
    history: &[forge_primitives::Message],
    compaction_count: u32,
) {
    use super::super::connect::type_converters::map_available_models;
    let session_id = model::SessionId::new(session_id.into_string());
    let available_models = map_available_models(available_models);
    if app.active_session_key.as_ref() == Some(previous_key) {
        handle_session_replaced_event(
            app,
            key,
            previous_key,
            session_id,
            cwd,
            current_model,
            available_models,
            mode,
            history,
        );
        seed_compaction_count(app, key, compaction_count);
        return;
    }
    // The replaced session is not the one on screen. Migrate its bucket
    // onto the replacement key and re-seed it through the same
    // background chain `Connected` uses, leaving every App-global
    // surface (focus, input, status, terminals, overlays) untouched.
    super::client::apply_session_update_key_renamed(app, previous_key, key.clone());
    tracing::info!(
        target: crate::logging::targets::APP_SESSION,
        event_name = "session_replaced",
        message = "replacement session applied to a background bucket",
        outcome = "success",
        session_id = %session_id,
        previous_session_key = %previous_key.as_str(),
        history_message_count = history.len(),
        available_model_count = available_models.len(),
    );
    apply_connected_presentation(
        app,
        key,
        session_id,
        cwd,
        current_model,
        available_models,
        mode,
        history,
        false,
    );
    // The foreground arm clears these through the active-bucket
    // accessors (`clear_compaction_state`, `set_pending_cancel`), which
    // the background arm cannot reach and `apply_connected_presentation`
    // does not cover. Left set, they surface on the next switch to this
    // tab as a stale cancel hint and a stale compacting indicator.
    if let Some(bucket) = app.sessions.get_mut(key) {
        bucket.pending_cancel = false;
        bucket.is_compacting = false;
        bucket.pending_compact_clear = false;
    }
    seed_compaction_count(app, key, compaction_count);
}

/// Set a session's compaction count from what its transcript records.
///
/// Assignment, not accumulation: the transcript already holds every
/// boundary counted live this run, so re-seeding on a later Connected
/// lands on the same total.
fn seed_compaction_count(app: &mut App, key: &SessionKey, compaction_count: u32) {
    if let Some(session) = app.session_mut(key) {
        session.session_usage.compaction_count = compaction_count;
    } else {
        // This is the count's only durable source, so a missed bucket
        // silently costs the whole number rather than one update.
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "compaction_seed_dropped",
            outcome = "dropped",
            session_key = %key.as_str(),
            compaction_count,
            "no bucket for the seeded compaction count; session will read zero",
        );
    }
}

pub(super) fn apply_session_update_sessions_listed(
    app: &mut App,
    key: &SessionKey,
    sessions: Vec<forge_primitives::SessionListEntry>,
) {
    handle_sessions_listed_event(app, key, sessions);
}

pub(super) fn apply_session_update_auth_required(
    app: &mut App,
    key: &SessionKey,
    method_name: String,
    method_description: String,
) {
    handle_auth_required_event(app, key, method_name, method_description);
}

pub(super) fn apply_session_update_connection_failed(
    app: &mut App,
    key: &SessionKey,
    message: &str,
    _fatal: bool,
) {
    handle_connection_failed_event(app, key, message);
}

pub(super) fn apply_session_update_slash_command_error(
    app: &mut App,
    key: &SessionKey,
    message: &str,
) {
    handle_slash_command_error_event(app, key, message);
}

/// `SessionUpdate::SetModeFailed` reducer: restore the pre-apply mode
/// snapshot the optimistic `/mode` apply parked on the bucket, then
/// surface the CLI's refusal as a system message. Rapid submits
/// overlap, so a refusal only rolls back when it names the mode the
/// chip currently shows - a refusal for a superseded request leaves
/// the newer optimistic apply (and its snapshot) alone.
pub(super) fn apply_session_update_set_mode_failed(
    app: &mut App,
    key: &SessionKey,
    mode: forge_workspace::PermissionMode,
    message: &str,
) {
    let Some(session) = app.sessions.get_mut(key) else {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "set_mode_failed_dropped",
            message = "set mode failure dropped for an unknown session",
            outcome = "dropped",
            session_key = %key.as_str(),
            reason = "unknown_session",
        );
        return;
    };
    let attempted = mode.as_wire();
    let chip_shows_attempted =
        session.mode.as_ref().is_some_and(|m| m.current_mode_id == attempted);
    if chip_shows_attempted && session.rollback_pending_mode() {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "set_mode_rollback_applied",
            message = "mode chip rolled back after a CLI refusal",
            outcome = "failure",
            session_key = %key.as_str(),
            mode = %attempted,
            error_message = %message,
        );
        app.invalidate_layout(crate::app::state::LayoutInvalidation::Global);
    }
    handle_slash_command_error_event(
        app,
        key,
        &format!("Mode switch to {attempted} was refused by the CLI: {message}"),
    );
}

pub(super) fn apply_session_update_set_model_failed(
    app: &mut App,
    key: &SessionKey,
    model: &str,
    message: &str,
) {
    let Some(session) = app.sessions.get_mut(key) else {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "set_model_failed_dropped",
            message = "set model failure dropped for an unknown session",
            outcome = "dropped",
            session_key = %key.as_str(),
            reason = "unknown_session",
        );
        return;
    };
    let chip_shows_attempted = session.turn_state.requested_model_id.as_deref() == Some(model);
    if chip_shows_attempted && session.rollback_pending_model() {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "set_model_rollback_applied",
            message = "model chip rolled back after a CLI refusal",
            outcome = "failure",
            session_key = %key.as_str(),
            model = %model,
            error_message = %message,
        );
        app.invalidate_layout(crate::app::state::LayoutInvalidation::Global);
    }
    handle_slash_command_error_event(
        app,
        key,
        &format!("Model switch to {model} was refused by the CLI: {message}"),
    );
}

pub(super) fn apply_session_update_service_status(
    app: &mut App,
    severity: forge_primitives::cloud::service_status::ServiceSeverity,
    message: &str,
) {
    present_service_status(app, severity, message);
}

pub(super) fn apply_session_update_fatal_error(app: &mut App, error: AppError) {
    handle_fatal_error_event(app, error);
}

#[cfg(test)]
mod stamp_project_tests {
    use super::stamp_bucket_project_from_cwd;
    use crate::app::App;

    /// The Connected stamp resolves the bucket's project from its cwd:
    /// a project-root cwd and a worktree-worker cwd both land on the
    /// parent project name. This is the boot-project / worker path that
    /// reaches Connected without a naming `Spawning`.
    #[test]
    fn stamp_resolves_project_root_and_worktree_cwd() {
        let mut app = App::test_default();
        let ws = app.workspace.clone().expect("test workspace");
        let path = "/tmp/stamp-proj";
        ws.seed_test_project("stampproj", path);

        let lead = forge_workspace::SessionKey::from_session_id("lead-uuid");
        let mut bucket = crate::app::session::UiSession::new(lead.clone());
        bucket.cwd_raw = path.to_owned();
        app.sessions.insert(lead.clone(), bucket);
        stamp_bucket_project_from_cwd(&mut app, &lead, false);
        assert_eq!(
            app.sessions.get(&lead).and_then(|b| b.project.clone()).as_deref(),
            Some("stampproj"),
            "a project-root cwd stamps the project name",
        );

        let worker = forge_workspace::SessionKey::from_session_id("worker-uuid");
        let mut bucket = crate::app::session::UiSession::new(worker.clone());
        bucket.cwd_raw = format!("{path}/.claude/worktrees/reviewer");
        app.sessions.insert(worker.clone(), bucket);
        stamp_bucket_project_from_cwd(&mut app, &worker, false);
        assert_eq!(
            app.sessions.get(&worker).and_then(|b| b.project.clone()).as_deref(),
            Some("stampproj"),
            "a worktree worker's cwd stamps its parent project name",
        );
    }

    /// `force = false` leaves an already-stamped name intact (a Spawning
    /// name survives Connect); `force = true` re-stamps (SessionReplaced).
    #[test]
    fn stamp_respects_force_flag() {
        let mut app = App::test_default();
        let ws = app.workspace.clone().expect("test workspace");
        ws.seed_test_project("stampproj", "/tmp/stamp-force-proj");

        let key = forge_workspace::SessionKey::from_session_id("k");
        let mut bucket = crate::app::session::UiSession::new(key.clone());
        bucket.cwd_raw = "/tmp/stamp-force-proj".to_owned();
        bucket.project = Some("preset".to_owned());
        app.sessions.insert(key.clone(), bucket);

        stamp_bucket_project_from_cwd(&mut app, &key, false);
        assert_eq!(
            app.sessions.get(&key).and_then(|b| b.project.clone()).as_deref(),
            Some("preset"),
            "force = false keeps the preceding name",
        );

        stamp_bucket_project_from_cwd(&mut app, &key, true);
        assert_eq!(
            app.sessions.get(&key).and_then(|b| b.project.clone()).as_deref(),
            Some("stampproj"),
            "force = true re-stamps from the cwd",
        );
    }
}

#[cfg(test)]
mod seed_compaction_count_tests {
    use super::seed_compaction_count;
    use crate::app::App;
    use crate::app::session::UiSession;
    use forge_workspace::SessionKey;

    /// Assignment, not accumulation. The transcript the seed comes from
    /// already contains every boundary counted live this run, so adding
    /// would double each one. Asserted against a bucket that already
    /// carries a live count, because the reducer paths that reach this
    /// helper all zero the count on the way in and so cannot tell the
    /// two apart.
    #[test]
    fn seeding_replaces_a_live_count_rather_than_adding_to_it() {
        let mut app = App::test_default();
        let key = SessionKey::from_session_id("seeded".to_owned());
        let mut bucket = UiSession::new(key.clone());
        bucket.session_usage.compaction_count = 3;
        app.sessions.insert(key.clone(), bucket);

        seed_compaction_count(&mut app, &key, 5);

        assert_eq!(
            app.sessions.get(&key).expect("bucket").session_usage.compaction_count,
            5,
            "the transcript count is the whole answer, not an addition to it",
        );
    }
}

#[cfg(test)]
mod teardown_clears_background_registry_tests {
    use super::handle_auth_required_event;
    use super::handle_connection_failed_event;
    use crate::app::App;
    use crate::app::BackgroundTask;
    use crate::app::session::UiSession;
    use forge_workspace::SessionKey;

    fn seed_task(bucket: &mut UiSession) {
        bucket.background_tasks.push(BackgroundTask {
            task_id: "t1".to_owned(),
            task_type: "local_bash".to_owned(),
            description: "gh run watch".to_owned(),
        });
        bucket.session_task_tool_use_ids.insert("t1".to_owned(), "tc-1".to_owned());
    }

    /// A background (non-active) session that fails to connect while a
    /// backgrounded task is registered must drop the registry: the CLI
    /// never sends a terminal `background_tasks_changed` for a dead
    /// session, so nothing else would clear it and the row would spin
    /// forever over its Failed glyph.
    #[test]
    fn background_connection_failure_clears_background_registry() {
        let mut app = App::test_default();
        let key = SessionKey::from_session_id("bg-fail");
        let mut bucket = UiSession::new(key.clone());
        seed_task(&mut bucket);
        app.sessions.insert(key.clone(), bucket);
        assert_ne!(
            app.active_session_key.as_ref(),
            Some(&key),
            "precondition: the failing session is not the active one",
        );

        handle_connection_failed_event(&mut app, &key, "connection refused");

        let bucket = app.sessions.get(&key).expect("bucket survives as a Failed shell");
        assert!(!bucket.has_live_background_work(), "background_tasks cleared on teardown");
        assert!(bucket.session_task_tool_use_ids.is_empty(), "task-id mirror cleared too");
    }

    /// Same guarantee on the focused (active-session) failure path.
    #[test]
    fn focused_connection_failure_clears_background_registry() {
        let mut app = App::test_default();
        let key = app.active_session_key.clone().expect("active key");
        seed_task(app.sessions.get_mut(&key).expect("active bucket"));

        handle_connection_failed_event(&mut app, &key, "connection refused");

        let bucket = app.sessions.get(&key).expect("bucket");
        assert!(!bucket.has_live_background_work(), "background_tasks cleared on teardown");
        assert!(bucket.session_task_tool_use_ids.is_empty(), "task-id mirror cleared too");
    }

    /// Token-expiry is the same bug through a different door: a background
    /// session hitting auth-required with a live task must drop the registry
    /// too, or it spins forever over its `⚠` glyph.
    #[test]
    fn background_auth_required_clears_background_registry() {
        let mut app = App::test_default();
        let key = SessionKey::from_session_id("bg-auth");
        let mut bucket = UiSession::new(key.clone());
        seed_task(&mut bucket);
        app.sessions.insert(key.clone(), bucket);
        assert_ne!(
            app.active_session_key.as_ref(),
            Some(&key),
            "precondition: the auth-blocked session is not the active one",
        );

        handle_auth_required_event(&mut app, &key, "oauth".to_owned(), "Log in".to_owned());

        let bucket = app.sessions.get(&key).expect("bucket");
        assert!(!bucket.has_live_background_work(), "auth-required clears background_tasks");
        assert!(bucket.session_task_tool_use_ids.is_empty(), "task-id mirror cleared too");
    }

    /// Same guarantee on the focused (active-session) auth-required path.
    #[test]
    fn focused_auth_required_clears_background_registry() {
        let mut app = App::test_default();
        let key = app.active_session_key.clone().expect("active key");
        seed_task(app.sessions.get_mut(&key).expect("active bucket"));

        handle_auth_required_event(&mut app, &key, "oauth".to_owned(), "Log in".to_owned());

        let bucket = app.sessions.get(&key).expect("bucket");
        assert!(!bucket.has_live_background_work(), "auth-required clears background_tasks");
        assert!(bucket.session_task_tool_use_ids.is_empty(), "task-id mirror cleared too");
    }
}

#[cfg(test)]
mod connected_log_tests {
    use super::apply_connected_presentation;
    use crate::agent::model;
    use crate::app::{App, session::UiSession};
    use forge_workspace::SessionKey;

    /// The session_connected log must carry the CONNECTING session's cwd
    /// and model, not whatever session the user has focused. A background
    /// connect (every worker) used to print the focused session's cwd -
    /// which made the worker-boot log name the wrong project entirely.
    #[test]
    fn background_connect_logs_the_event_cwd_not_the_focused_cwd() {
        let mut app = App::test_default();
        let focused = SessionKey::from_session_id("focused-uuid");
        let mut focused_bucket = UiSession::new(focused.clone());
        focused_bucket.cwd_raw = "/Users/vedhavyas/Projects/granite-backend".to_owned();
        app.sessions.insert(focused.clone(), focused_bucket);
        app.active_session_key = Some(focused);

        let worker = SessionKey::from_session_id("worker-uuid");
        let log = capture_logs(|| {
            apply_connected_presentation(
                &mut app,
                &worker,
                model::SessionId::new("worker-uuid"),
                "/Users/vedhavyas/Projects/companies".to_owned(),
                model::CurrentModel::new("claude-opus-5", "opus", "Opus"),
                Vec::new(),
                None,
                &[],
                false,
            );
        });
        assert!(
            log.contains("cwd=/Users/vedhavyas/Projects/companies"),
            "the log must carry the connecting session's cwd; got: {log}"
        );
        assert!(
            !log.contains("granite-backend"),
            "the focused session's cwd must not leak into the log; got: {log}"
        );
        assert!(
            log.contains("claude-opus-5"),
            "the log must carry the connecting session's model; got: {log}"
        );
    }

    /// A resume connect carries an EMPTY event cwd (the bucket was
    /// pre-seeded at spawn); the log must fall back to that pre-seeded
    /// value, not print an empty path.
    #[test]
    fn empty_event_cwd_falls_back_to_the_buckets_seeded_cwd() {
        let mut app = App::test_default();
        let focused = SessionKey::from_session_id("focused-uuid");
        let mut focused_bucket = UiSession::new(focused.clone());
        focused_bucket.cwd_raw = "/Users/vedhavyas/Projects/granite-backend".to_owned();
        app.sessions.insert(focused.clone(), focused_bucket);
        app.active_session_key = Some(focused);

        let worker = SessionKey::from_session_id("worker-uuid");
        let mut worker_bucket = UiSession::new(worker.clone());
        worker_bucket.cwd_raw =
            "/Users/vedhavyas/Projects/forge/.claude/worktrees/reviewer".to_owned();
        app.sessions.insert(worker.clone(), worker_bucket);

        let log = capture_logs(|| {
            apply_connected_presentation(
                &mut app,
                &worker,
                model::SessionId::new("worker-uuid"),
                String::new(),
                model::CurrentModel::new("claude-opus-5", "opus", "Opus"),
                Vec::new(),
                None,
                &[],
                false,
            );
        });
        assert!(
            log.contains("cwd=/Users/vedhavyas/Projects/forge/.claude/worktrees/reviewer"),
            "the log must fall back to the bucket's pre-seeded cwd on an empty event cwd; got: {log}"
        );
        assert!(
            !log.contains("cwd=granite-backend")
                && !log.contains("cwd=/Users/vedhavyas/Projects/granite-backend"),
            "the focused session's cwd must not leak into the log; got: {log}"
        );
    }

    /// Log capture mirroring `forge_agent`'s test helper - the tracing
    /// line is the artifact under test.
    fn capture_logs(f: impl FnOnce()) -> String {
        use std::sync::{Arc, Mutex};

        #[derive(Clone)]
        struct Writer(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Writer {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().expect("capture lock").extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let capture: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let writer = Writer(Arc::clone(&capture));
        let subscriber =
            tracing_subscriber::fmt().with_ansi(false).with_writer(move || writer.clone()).finish();
        tracing::subscriber::with_default(subscriber, f);
        String::from_utf8_lossy(&capture.lock().expect("capture lock")).into_owned()
    }
}
