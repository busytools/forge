#![allow(clippy::needless_pass_by_value)]

use super::super::{
    App, AppStatus, ChatMessage, FocusTarget, InlinePermission, InlineQuestion,
    InvalidationLevel, MessageBlock, MessageRole, NoticeStage, SystemSeverity, TextBlock,
};
use super::clear_compaction_state;
use super::rate_limit::{format_rate_limit_summary, rate_limit_notice_key};
use forge_workspace::translate::error_handling::{TurnErrorClass, classify_turn_error, summarize_internal_error};
use crate::agent::model;
use forge_workspace::SessionKey;
use std::collections::BTreeSet;

const CONVERSATION_INTERRUPTED_HINT: &str =
    "Conversation interrupted. Tell the model how to proceed.";
const TURN_ERROR_INPUT_LOCK_HINT: &str =
    "Input disabled after an error. Press Ctrl+Q to quit and try again.";
const PLAN_LIMIT_NEXT_STEPS_HINT: &str = "Next steps:\n\
1. Wait a few minutes and retry.\n\
2. Reduce request size or request frequency.\n\
3. Check quota/billing for your account or switch plans.";
const AUTH_REQUIRED_NEXT_STEPS_HINT: &str =
    "Authentication required. Run `claude auth login` in a terminal to authenticate.";

#[derive(Clone, Copy)]
struct TurnExitState {
    tail_assistant_idx: Option<usize>,
    turn_was_active: bool,
    cancelled_requested: bool,
    show_interrupted_hint: bool,
}

/// Test-only entry that drives the presentation pipeline with a
/// model-side request, skipping the wire-to-model conversion.
#[cfg(feature = "testing")]
pub fn handle_permission_request_event(
    app: &mut App,
    session_key: SessionKey,
    tool_id: String,
    request: model::RequestPermissionRequest,
) {
    apply_permission_request_presentation(app, &session_key, &tool_id, request);
}

/// `SessionUpdate::PermissionRequest` reducer. Converts
/// the workspace's wire-shape `PermissionRequest` into the
/// model-side `RequestPermissionRequest` and delegates to the
/// shared presentation helper.
pub(super) fn apply_session_update_permission_request(
    app: &mut App,
    key: SessionKey,
    tool_id: String,
    request: forge_primitives::permission_ui::PermissionRequest,
) {
    let session_id = key.as_str().to_owned();
    let (model_request, _converted_tool_id) =
        crate::app::connect::type_converters::map_permission_request(&session_id, request);
    apply_permission_request_presentation(app, &key, &tool_id, model_request);
}

/// Shared body for the legacy ClientEvent path and the new
/// SessionUpdate reducer. Looks up the target bucket by `key` and
/// applies the permission request to it directly — no temp-swap on
/// `active_session_key`. Active vs background routing happens inside
/// [`apply_permission_request_to_bucket`].
fn apply_permission_request_presentation(
    app: &mut App,
    session_key: &SessionKey,
    tool_id: &str,
    request: model::RequestPermissionRequest,
) {
    let session_id = request.session_id.to_string();
    let options = request.options.clone();

    // Lifecycle: a permission prompt landing on a non-active session
    // means that bucket is paused awaiting user input — flip to
    // Attention so the Projects pane shows the △ glyph. Active
    // sessions don't need this — the inline permission card itself
    // surfaces the prompt to the user.
    if app.active_session_key.as_ref() != Some(session_key) {
        super::set_bucket_lifecycle_state(
            app,
            session_key,
            crate::app::session::SessionLifecycleState::Attention,
        );
    }

    apply_permission_request_to_bucket(app, session_key, request, &session_id, tool_id, &options);
}

fn apply_permission_request_to_bucket(
    app: &mut App,
    session_key: &SessionKey,
    request: model::RequestPermissionRequest,
    session_id: &str,
    tool_id: &str,
    options: &[model::PermissionOption],
) {
    let is_active = app.active_session_key.as_ref() == Some(session_key);
    if is_active {
        apply_permission_request_to_active_bucket(
            app,
            session_key,
            request,
            session_id,
            tool_id,
            options,
        );
    } else {
        apply_permission_request_to_background_bucket(
            app,
            session_key,
            request,
            session_id,
            tool_id,
            options,
        );
    }
}

fn apply_permission_request_to_active_bucket(
    app: &mut App,
    session_key: &SessionKey,
    request: model::RequestPermissionRequest,
    session_id: &str,
    tool_id: &str,
    options: &[model::PermissionOption],
) {
    let Some((mi, bi)) = app.lookup_tool_call(tool_id) else {
        tracing::warn!(
            target: crate::logging::targets::APP_PERMISSION,
            event_name = "permission_request_rejected",
            message = "permission request rejected for unknown tool call",
            outcome = "dropped",
            session_id = %session_id,
            tool_call_id = %tool_id,
            reason = "unknown_tool_call",
        );
        auto_reject_permission_via_workspace(app, session_key, tool_id, options);
        return;
    };

    if app.pending_interaction_ids().iter().any(|id| id == tool_id) {
        tracing::warn!(
            target: crate::logging::targets::APP_PERMISSION,
            event_name = "permission_request_rejected",
            message = "duplicate permission request rejected",
            outcome = "dropped",
            session_id = %session_id,
            tool_call_id = %tool_id,
            reason = "duplicate_pending_interaction",
        );
        auto_reject_permission_via_workspace(app, session_key, tool_id, options);
        return;
    }

    let mut layout_dirty = false;
    let auto_focus = app.pending_interaction_ids().is_empty() && !app.has_draft_input_for_focus();
    if let Some(MessageBlock::ToolCall(tc)) =
        app.active_messages_mut().get_mut(mi).and_then(|m| m.blocks.get_mut(bi))
    {
        let tc = tc.as_mut();
        tc.pending_permission = Some(InlinePermission {
            options: request.options,
            display: request.display,
            tool_id: tool_id.to_owned(),
            selected_index: 0,
            focused: auto_focus,
        });
        tc.mark_tool_call_layout_dirty();
        layout_dirty = true;
        app.pending_interaction_ids_mut().push(tool_id.to_owned());
        if auto_focus {
            app.claim_focus_target(FocusTarget::Permission);
        }
        app.active_viewport_mut().engage_auto_scroll();
        app.notifications.notify(
            app.config.preferred_notification_channel_effective(),
            super::super::notify::NotifyEvent::PermissionRequired,
        );
        tracing::info!(
            target: crate::logging::targets::APP_PERMISSION,
            event_name = "permission_request_applied",
            message = "permission request applied to inline tool call",
            outcome = "success",
            session_id = %session_id,
            tool_call_id = %tool_id,
            option_count = options.len(),
            focused = auto_focus,
        );
    } else {
        tracing::warn!(
            target: crate::logging::targets::APP_PERMISSION,
            event_name = "permission_request_rejected",
            message = "permission request rejected because target block was not a tool call",
            outcome = "dropped",
            session_id = %session_id,
            tool_call_id = %tool_id,
            reason = "non_tool_block",
        );
        auto_reject_permission_via_workspace(app, session_key, tool_id, options);
    }

    if layout_dirty {
        app.sync_render_cache_slot(mi, bi);
        app.recompute_message_retained_bytes(mi);
        app.invalidate_layout(InvalidationLevel::MessageChanged(mi));
    }
}

/// Background-session version of
/// [`apply_permission_request_to_active_bucket`]. Operates on the
/// bucket directly so no temp-swap on `active_session_key` is needed.
/// Skips active-session-only side effects (focus claim, viewport
/// auto-scroll on the active viewport, layout invalidation against the
/// active viewport). Notifications still fire — the user needs to know
/// a background session has hit a prompt. Layout state is rebuilt when
/// the bucket next becomes active, matching the
/// [`finalize_background_tool_calls`] pattern below.
fn apply_permission_request_to_background_bucket(
    app: &mut App,
    session_key: &SessionKey,
    request: model::RequestPermissionRequest,
    session_id: &str,
    tool_id: &str,
    options: &[model::PermissionOption],
) {
    let Some(bucket) = app.sessions.get_mut(session_key) else {
        tracing::warn!(
            target: crate::logging::targets::APP_PERMISSION,
            event_name = "permission_request_rejected",
            message = "permission request rejected for unknown background bucket",
            outcome = "dropped",
            session_id = %session_id,
            tool_call_id = %tool_id,
            reason = "unknown_session",
        );
        auto_reject_permission_via_workspace(app, session_key, tool_id, options);
        return;
    };
    let Some((mi, bi)) = bucket.tool_call_index.get(tool_id).copied() else {
        tracing::warn!(
            target: crate::logging::targets::APP_PERMISSION,
            event_name = "permission_request_rejected",
            message = "permission request rejected for unknown tool call",
            outcome = "dropped",
            session_id = %session_id,
            tool_call_id = %tool_id,
            reason = "unknown_tool_call",
        );
        auto_reject_permission_via_workspace(app, session_key, tool_id, options);
        return;
    };
    if bucket.pending_interaction_ids.iter().any(|id| id == tool_id) {
        tracing::warn!(
            target: crate::logging::targets::APP_PERMISSION,
            event_name = "permission_request_rejected",
            message = "duplicate permission request rejected",
            outcome = "dropped",
            session_id = %session_id,
            tool_call_id = %tool_id,
            reason = "duplicate_pending_interaction",
        );
        auto_reject_permission_via_workspace(app, session_key, tool_id, options);
        return;
    }
    let auto_focus = bucket.pending_interaction_ids.is_empty();
    let option_count = options.len();
    if let Some(MessageBlock::ToolCall(tc)) =
        bucket.messages.get_mut(mi).and_then(|m| m.blocks.get_mut(bi))
    {
        let tc = tc.as_mut();
        tc.pending_permission = Some(InlinePermission {
            options: request.options,
            display: request.display,
            tool_id: tool_id.to_owned(),
            selected_index: 0,
            focused: auto_focus,
        });
        tc.mark_tool_call_layout_dirty();
        bucket.pending_interaction_ids.push(tool_id.to_owned());
        app.notifications.notify(
            app.config.preferred_notification_channel_effective(),
            super::super::notify::NotifyEvent::PermissionRequired,
        );
        tracing::info!(
            target: crate::logging::targets::APP_PERMISSION,
            event_name = "permission_request_applied",
            message = "permission request applied to background tool call",
            outcome = "success",
            session_id = %session_id,
            tool_call_id = %tool_id,
            option_count = option_count,
            focused = auto_focus,
        );
    } else {
        tracing::warn!(
            target: crate::logging::targets::APP_PERMISSION,
            event_name = "permission_request_rejected",
            message = "permission request rejected because target block was not a tool call",
            outcome = "dropped",
            session_id = %session_id,
            tool_call_id = %tool_id,
            reason = "non_tool_block",
        );
        auto_reject_permission_via_workspace(app, session_key, tool_id, options);
    }
}

/// Test-only entry. See `handle_permission_request_event`.
#[cfg(feature = "testing")]
pub fn handle_question_request_event(
    app: &mut App,
    session_key: SessionKey,
    tool_id: String,
    request: model::RequestQuestionRequest,
) {
    apply_question_request_presentation(app, &session_key, &tool_id, request);
}

/// `SessionUpdate::QuestionRequest` reducer. Converts the
/// workspace's wire-shape `QuestionRequest` into the model-side
/// `RequestQuestionRequest` and delegates to the shared presentation
/// helper.
pub(super) fn apply_session_update_question_request(
    app: &mut App,
    key: SessionKey,
    tool_id: String,
    request: forge_primitives::question::QuestionRequest,
) {
    let session_id = key.as_str().to_owned();
    let (model_request, _converted_tool_id) =
        crate::app::connect::type_converters::map_question_request(&session_id, request);
    apply_question_request_presentation(app, &key, &tool_id, model_request);
}

/// `SessionUpdate::McpElicitationRequest` reducer. The
/// elicitation dialogue is an App-global UI overlay that's only
/// meaningful for the active session; background-session requests
/// are dropped at the routing layer (matching the legacy ClientEvent
/// path's behaviour). `elicitation_id` rides on the envelope for
/// routing-side correlation but the presentation helper consumes
/// only `request` — the id is already embedded in
/// [`forge_primitives::ElicitationRequest`].
pub(super) fn apply_session_update_mcp_elicitation_request(
    app: &mut App,
    key: SessionKey,
    elicitation_id: String,
    request: forge_primitives::ElicitationRequest,
) {
    let _ = elicitation_id;
    if app.active_session_key.as_ref() != Some(&key) {
        return;
    }
    crate::app::config::present_mcp_elicitation_request(app, request);
}

/// Shared body for the legacy ClientEvent path and the new
/// SessionUpdate reducer. Looks up the target bucket by `key` and
/// applies the question request to it directly — no temp-swap.
fn apply_question_request_presentation(
    app: &mut App,
    session_key: &SessionKey,
    tool_id: &str,
    request: model::RequestQuestionRequest,
) {
    let session_id = request.session_id.to_string();
    let option_count = request.prompt.options.len();
    let question_index = request.question_index;
    let total_questions = request.total_questions;

    // Lifecycle: as with permission prompts — a question landing on a
    // non-active session flips that bucket to Attention.
    if app.active_session_key.as_ref() != Some(session_key) {
        super::set_bucket_lifecycle_state(
            app,
            session_key,
            crate::app::session::SessionLifecycleState::Attention,
        );
    }

    apply_question_request_to_bucket(
        app,
        session_key,
        request,
        &session_id,
        tool_id,
        option_count,
        question_index,
        total_questions,
    );
}

#[allow(clippy::too_many_arguments)]
fn apply_question_request_to_bucket(
    app: &mut App,
    session_key: &SessionKey,
    request: model::RequestQuestionRequest,
    session_id: &str,
    tool_id: &str,
    option_count: usize,
    question_index: usize,
    total_questions: usize,
) {
    let is_active = app.active_session_key.as_ref() == Some(session_key);
    if is_active {
        apply_question_request_to_active_bucket(
            app,
            session_key,
            request,
            session_id,
            tool_id,
            option_count,
            question_index,
            total_questions,
        );
    } else {
        apply_question_request_to_background_bucket(
            app,
            session_key,
            request,
            session_id,
            tool_id,
            option_count,
            question_index,
            total_questions,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_question_request_to_active_bucket(
    app: &mut App,
    session_key: &SessionKey,
    request: model::RequestQuestionRequest,
    session_id: &str,
    tool_id: &str,
    option_count: usize,
    question_index: usize,
    total_questions: usize,
) {
    let Some((mi, bi)) = app.lookup_tool_call(tool_id) else {
        tracing::warn!(
            target: crate::logging::targets::APP_PERMISSION,
            event_name = "question_request_rejected",
            message = "question request rejected for unknown tool call",
            outcome = "dropped",
            session_id = %session_id,
            tool_call_id = %tool_id,
            reason = "unknown_tool_call",
        );
        cancel_question_via_workspace(app, session_key, tool_id);
        return;
    };

    if app.pending_interaction_ids().iter().any(|id| id == tool_id) {
        tracing::warn!(
            target: crate::logging::targets::APP_PERMISSION,
            event_name = "question_request_rejected",
            message = "duplicate question request rejected",
            outcome = "dropped",
            session_id = %session_id,
            tool_call_id = %tool_id,
            reason = "duplicate_pending_interaction",
        );
        cancel_question_via_workspace(app, session_key, tool_id);
        return;
    }

    let mut layout_dirty = false;
    let auto_focus = app.pending_interaction_ids().is_empty() && !app.has_draft_input_for_focus();
    if let Some(MessageBlock::ToolCall(tc)) =
        app.active_messages_mut().get_mut(mi).and_then(|m| m.blocks.get_mut(bi))
    {
        let tc = tc.as_mut();
        tc.pending_question = Some(InlineQuestion {
            prompt: request.prompt,
            tool_id: tool_id.to_owned(),
            focused_option_index: 0,
            selected_option_indices: BTreeSet::new(),
            notes: String::new(),
            notes_cursor: 0,
            editing_notes: false,
            focused: auto_focus,
            question_index: request.question_index,
            total_questions: request.total_questions,
        });
        tc.mark_tool_call_layout_dirty();
        layout_dirty = true;
        app.pending_interaction_ids_mut().push(tool_id.to_owned());
        if auto_focus {
            app.claim_focus_target(FocusTarget::Permission);
        }
        app.active_viewport_mut().engage_auto_scroll();
        app.notifications.notify(
            app.config.preferred_notification_channel_effective(),
            super::super::notify::NotifyEvent::QuestionRequired,
        );
        tracing::info!(
            target: crate::logging::targets::APP_PERMISSION,
            event_name = "question_request_applied",
            message = "question request applied to inline tool call",
            outcome = "success",
            session_id = %session_id,
            tool_call_id = %tool_id,
            question_index,
            total_questions,
            option_count,
            focused = auto_focus,
        );
    } else {
        tracing::warn!(
            target: crate::logging::targets::APP_PERMISSION,
            event_name = "question_request_rejected",
            message = "question request rejected because target block was not a tool call",
            outcome = "dropped",
            session_id = %session_id,
            tool_call_id = %tool_id,
            reason = "non_tool_block",
        );
        cancel_question_via_workspace(app, session_key, tool_id);
    }

    if layout_dirty {
        app.sync_render_cache_slot(mi, bi);
        app.recompute_message_retained_bytes(mi);
        app.invalidate_layout(InvalidationLevel::MessageChanged(mi));
    }
}

/// Background-session version of
/// [`apply_question_request_to_active_bucket`]. Operates on the bucket
/// directly so no temp-swap on `active_session_key` is needed. Skips
/// active-only side effects (focus claim, viewport auto-scroll on the
/// active viewport, layout invalidation); the bucket rebuilds its
/// layout when it next becomes active.
#[allow(clippy::too_many_arguments)]
fn apply_question_request_to_background_bucket(
    app: &mut App,
    session_key: &SessionKey,
    request: model::RequestQuestionRequest,
    session_id: &str,
    tool_id: &str,
    option_count: usize,
    question_index: usize,
    total_questions: usize,
) {
    let Some(bucket) = app.sessions.get_mut(session_key) else {
        tracing::warn!(
            target: crate::logging::targets::APP_PERMISSION,
            event_name = "question_request_rejected",
            message = "question request rejected for unknown background bucket",
            outcome = "dropped",
            session_id = %session_id,
            tool_call_id = %tool_id,
            reason = "unknown_session",
        );
        cancel_question_via_workspace(app, session_key, tool_id);
        return;
    };
    let Some((mi, bi)) = bucket.tool_call_index.get(tool_id).copied() else {
        tracing::warn!(
            target: crate::logging::targets::APP_PERMISSION,
            event_name = "question_request_rejected",
            message = "question request rejected for unknown tool call",
            outcome = "dropped",
            session_id = %session_id,
            tool_call_id = %tool_id,
            reason = "unknown_tool_call",
        );
        cancel_question_via_workspace(app, session_key, tool_id);
        return;
    };
    if bucket.pending_interaction_ids.iter().any(|id| id == tool_id) {
        tracing::warn!(
            target: crate::logging::targets::APP_PERMISSION,
            event_name = "question_request_rejected",
            message = "duplicate question request rejected",
            outcome = "dropped",
            session_id = %session_id,
            tool_call_id = %tool_id,
            reason = "duplicate_pending_interaction",
        );
        cancel_question_via_workspace(app, session_key, tool_id);
        return;
    }
    let auto_focus = bucket.pending_interaction_ids.is_empty();
    if let Some(MessageBlock::ToolCall(tc)) =
        bucket.messages.get_mut(mi).and_then(|m| m.blocks.get_mut(bi))
    {
        let tc = tc.as_mut();
        tc.pending_question = Some(InlineQuestion {
            prompt: request.prompt,
            tool_id: tool_id.to_owned(),
            focused_option_index: 0,
            selected_option_indices: BTreeSet::new(),
            notes: String::new(),
            notes_cursor: 0,
            editing_notes: false,
            focused: auto_focus,
            question_index: request.question_index,
            total_questions: request.total_questions,
        });
        tc.mark_tool_call_layout_dirty();
        bucket.pending_interaction_ids.push(tool_id.to_owned());
        app.notifications.notify(
            app.config.preferred_notification_channel_effective(),
            super::super::notify::NotifyEvent::QuestionRequired,
        );
        tracing::info!(
            target: crate::logging::targets::APP_PERMISSION,
            event_name = "question_request_applied",
            message = "question request applied to background tool call",
            outcome = "success",
            session_id = %session_id,
            tool_call_id = %tool_id,
            question_index,
            total_questions,
            option_count,
            focused = auto_focus,
        );
    } else {
        tracing::warn!(
            target: crate::logging::targets::APP_PERMISSION,
            event_name = "question_request_rejected",
            message = "question request rejected because target block was not a tool call",
            outcome = "dropped",
            session_id = %session_id,
            tool_call_id = %tool_id,
            reason = "non_tool_block",
        );
        cancel_question_via_workspace(app, session_key, tool_id);
    }
}

/// Auto-reject a permission via the workspace channel. The chosen
/// option mirrors the pre-Phase-1 inline behaviour: pick the last
/// option (typically "reject_once"). The workspace pops the
/// matching oneshot and the bridge forwards the response.
fn auto_reject_permission_via_workspace(
    app: &App,
    session_key: &SessionKey,
    tool_id: &str,
    options: &[model::PermissionOption],
) {
    let Some(last_opt) = options.last() else {
        return;
    };
    dispatch_permission_outcome(
        app,
        session_key,
        tool_id,
        forge_primitives::PermissionOutcome::Selected { option_id: last_opt.option_id.clone() },
    );
}

/// Cancel a pending question via the workspace channel. The bridge
/// forwards `QuestionOutcome::Cancelled` to the agent.
fn cancel_question_via_workspace(app: &App, session_key: &SessionKey, tool_id: &str) {
    let Some(workspace) = app.workspace.as_ref() else {
        return;
    };
    let cmd = forge_workspace::Command::RespondQuestion {
        key: session_key.clone(),
        tool_id: tool_id.to_owned(),
        outcome: forge_primitives::QuestionOutcome::Cancelled,
    };
    if let Err(err) = workspace.dispatch(cmd) {
        tracing::warn!(
            target: crate::logging::targets::APP_PERMISSION,
            event_name = "question_auto_cancel_dispatch_failed",
            session_key = %session_key.as_str(),
            tool_id = %tool_id,
            error = %err,
            "failed to dispatch auto-cancel for unrecognised question request",
        );
    }
}

/// Dispatch a [`forge_primitives::PermissionOutcome`] for `tool_id`
/// via the workspace's [`forge_workspace::Workspace::dispatch`] path.
/// Used by both the user-pick handler (`app::permissions`) and the
/// auto-reject paths in this module.
///
/// Under the `testing` Cargo feature, when `app.workspace` is `None`
/// (the `App::test_default` fixture used by every legacy permission
/// / question unit test), the outcome is captured into the App's
/// per-feature test-capture field so tests can assert "the user-pick
/// handler fired outcome X for tool_id Y" without spinning up a real
/// workspace.
pub(crate) fn dispatch_permission_outcome(
    app: &App,
    session_key: &SessionKey,
    tool_id: &str,
    outcome: forge_primitives::PermissionOutcome,
) {
    // Under the `testing` Cargo feature we always capture the outcome
    // into the App's test-capture field, regardless of whether the
    // workspace is set. The legacy permission unit tests assert via
    // this capture; Phase 5 wired `App::test_default()` to a workspace
    // stub, so the workspace path now also fires but produces no
    // observable side-effect (no `SessionTask` registered for the test
    // key — the dispatch returns an `UnknownSession` error which we
    // silence below). Production builds always set the workspace and
    // never compile this capture branch in.
    #[cfg(feature = "testing")]
    app.test_dispatched_permission_outcomes
        .borrow_mut()
        .push((tool_id.to_owned(), outcome.clone()));
    let Some(workspace) = app.workspace.as_ref() else {
        tracing::error!(
            target: crate::logging::targets::APP_PERMISSION,
            event_name = "permission_dispatch_no_workspace",
            session_key = %session_key.as_str(),
            tool_id = %tool_id,
            "permission outcome dropped: app.workspace is None — this should never happen in production",
        );
        return;
    };
    let cmd = forge_workspace::Command::RespondPermission {
        key: session_key.clone(),
        tool_id: tool_id.to_owned(),
        outcome,
    };
    if let Err(err) = workspace.dispatch(cmd) {
        // Under `testing`, the workspace stub has no `SessionTask`
        // registered for the test key — `UnknownSession` is the
        // expected outcome and the test-capture above already
        // observed the dispatch intent. Downgrade to debug.
        #[cfg(feature = "testing")]
        if matches!(err, forge_workspace::DispatchError::UnknownSession(_)) {
            tracing::debug!(
                target: crate::logging::targets::APP_PERMISSION,
                event_name = "permission_dispatch_skipped_in_test",
                session_key = %session_key.as_str(),
                tool_id = %tool_id,
                "permission dispatch skipped: no session task in test stub",
            );
            return;
        }
        tracing::warn!(
            target: crate::logging::targets::APP_PERMISSION,
            event_name = "permission_dispatch_failed",
            session_key = %session_key.as_str(),
            tool_id = %tool_id,
            error = %err,
            "failed to dispatch permission response",
        );
    }
}

/// Same-crate test helpers for `dispatch_*_outcome`'s testing-feature
/// capture vecs. Only `#[cfg(test)]` callers (in the lib's own test
/// modules) reach these; integration tests read the capture vecs
/// directly via the `pub` fields.
#[cfg(test)]
pub(crate) mod test_capture {
    use super::App;

    /// Test-only: pop the first captured permission outcome whose
    /// `tool_id` matches. Mirrors the legacy
    /// `oneshot::Receiver::try_recv()` shape that the pre-Phase-1
    /// permission tests used to assert "the user-pick handler fired
    /// outcome X for tool_id Y" without needing a live workspace.
    ///
    /// Returns the captured outcome on hit; returns
    /// `Err(TryRecvError::Empty)` on miss so tests can still match
    /// on the oneshot error shape.
    pub fn try_take_dispatched_permission_outcome(
        app: &App,
        tool_id: &str,
    ) -> Result<forge_primitives::PermissionOutcome, tokio::sync::oneshot::error::TryRecvError>
    {
        #[rustfmt::skip] #[cfg(feature = "testing")] let mut guard = app.test_dispatched_permission_outcomes.borrow_mut();
        if let Some(pos) = guard.iter().position(|(tid, _)| tid == tool_id) {
            let (_, outcome) = guard.remove(pos);
            Ok(outcome)
        } else {
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        }
    }

    /// Test-only: same as [`try_take_dispatched_permission_outcome`]
    /// but for question outcomes.
    pub fn try_take_dispatched_question_outcome(
        app: &App,
        tool_id: &str,
    ) -> Result<forge_primitives::QuestionOutcome, tokio::sync::oneshot::error::TryRecvError> {
        #[rustfmt::skip] #[cfg(feature = "testing")] let mut guard = app.test_dispatched_question_outcomes.borrow_mut();
        if let Some(pos) = guard.iter().position(|(tid, _)| tid == tool_id) {
            let (_, outcome) = guard.remove(pos);
            Ok(outcome)
        } else {
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        }
    }
}

/// Dispatch a [`forge_primitives::QuestionOutcome`] for `tool_id`
/// via the workspace. Used by `app::questions` when the user picks
/// an option. Under the `testing` Cargo feature, see
/// [`dispatch_permission_outcome`] — same test-capture rule applies.
pub(crate) fn dispatch_question_outcome(
    app: &App,
    session_key: &SessionKey,
    tool_id: &str,
    outcome: forge_primitives::QuestionOutcome,
) {
    // Mirror of [`dispatch_permission_outcome`] — see its docs.
    #[cfg(feature = "testing")]
    app.test_dispatched_question_outcomes.borrow_mut().push((tool_id.to_owned(), outcome.clone()));
    let Some(workspace) = app.workspace.as_ref() else {
        tracing::error!(
            target: crate::logging::targets::APP_PERMISSION,
            event_name = "question_dispatch_no_workspace",
            session_key = %session_key.as_str(),
            tool_id = %tool_id,
            "question outcome dropped: app.workspace is None — this should never happen in production",
        );
        return;
    };
    let cmd = forge_workspace::Command::RespondQuestion {
        key: session_key.clone(),
        tool_id: tool_id.to_owned(),
        outcome,
    };
    if let Err(err) = workspace.dispatch(cmd) {
        #[cfg(feature = "testing")]
        if matches!(err, forge_workspace::DispatchError::UnknownSession(_)) {
            tracing::debug!(
                target: crate::logging::targets::APP_PERMISSION,
                event_name = "question_dispatch_skipped_in_test",
                session_key = %session_key.as_str(),
                tool_id = %tool_id,
                "question dispatch skipped: no session task in test stub",
            );
            return;
        }
        tracing::warn!(
            target: crate::logging::targets::APP_PERMISSION,
            event_name = "question_dispatch_failed",
            session_key = %session_key.as_str(),
            tool_id = %tool_id,
            error = %err,
            "failed to dispatch question response",
        );
    }
}

/// `SessionUpdate::TurnCancelled` reducer. The workspace
/// already finalized the DomainSession via the synthesized turn event
/// in the SDK message flow upstream; this reducer is the TUI-side
/// projection. (For the direct call from `sdk_message.rs` the
/// operational hook is run there.)
pub(super) fn apply_session_update_turn_cancelled(app: &mut App, key: &SessionKey) {
    apply_turn_cancelled_presentation(app, key);
}

fn apply_turn_cancelled_presentation(app: &mut App, session_key: &SessionKey) {
    if app.active_session_key.as_ref() == Some(session_key) {
        if !app.pending_cancel() {
            app.set_pending_cancel(true);
        }
        app.set_cancelled_turn_pending_hint(app.pending_cancel());
        let _ = app.finalize_in_progress_tool_calls(model::ToolCallStatus::Failed);
        // Lifecycle: cancellation accepted — return active session
        // to Idle. (Steady-state TurnComplete fires shortly after,
        // also setting Idle — this is a defensive idempotent set.)
        super::set_bucket_lifecycle_state(
            app,
            session_key,
            crate::app::session::SessionLifecycleState::Idle,
        );
        return;
    }
    let Some(session) = app.session_mut(session_key) else {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "turn_cancelled_dropped",
            message = "turn cancelled dropped for an unknown session",
            outcome = "dropped",
            session_key = %session_key.as_str(),
            reason = "unknown_session",
        );
        return;
    };
    if !session.pending_cancel {
        session.pending_cancel = true;
    }
    session.cancelled_turn_pending_hint =
        session.pending_cancel;
    finalize_background_tool_calls(session, model::ToolCallStatus::Failed);
    // Drop the `session` mut borrow before reaching for the workspace.
    let _ = session;
    // Lifecycle: background cancel accepted — same Idle target.
    super::set_bucket_lifecycle_state(
        app,
        session_key,
        crate::app::session::SessionLifecycleState::Idle,
    );
}

/// Background-session version of [`App::finalize_in_progress_tool_calls`].
/// Walks the bucket's `messages` directly and flips InProgress /
/// Pending tool calls to `new_status`, dropping pending interactions.
/// No layout invalidation, no terminal detach handling — the bucket
/// will rebuild its layout state when it next becomes active.
pub(super) fn finalize_background_tool_calls(
    session: &mut crate::app::session::UiSession,
    new_status: model::ToolCallStatus,
) {
    for msg in &mut session.messages {
        for block in &mut msg.blocks {
            if let MessageBlock::ToolCall(tc) = block {
                let tc = tc.as_mut();
                if matches!(
                    tc.status,
                    model::ToolCallStatus::InProgress | model::ToolCallStatus::Pending
                ) {
                    tc.status = new_status;
                    let _ = tc.pending_permission.take();
                    let _ = tc.pending_question.take();
                    let _ = tc.terminal_id.take();
                }
            }
        }
    }
    session.pending_interaction_ids.clear();
}

fn begin_turn_exit(app: &mut App, emit_manual_compaction_success: bool) -> TurnExitState {
    let state = TurnExitState {
        tail_assistant_idx: app
            .messages()
            .iter()
            .rposition(|m| matches!(m.role, MessageRole::Assistant)),
        turn_was_active: matches!(app.status, AppStatus::Thinking | AppStatus::Running),
        cancelled_requested: app.pending_cancel(),
        show_interrupted_hint: app.pending_cancel(),
    };
    clear_compaction_state(app, emit_manual_compaction_success);
    app.set_pending_cancel(false);
    app.set_cancelled_turn_pending_hint(false);
    state
}

fn finish_ready_turn_exit(app: &mut App, exit: TurnExitState, tool_status: model::ToolCallStatus) {
    app.finalize_turn_runtime_artifacts(tool_status);
    app.status = AppStatus::Ready;
    app.set_files_accessed(0);

    let removed_tail_assistant = remove_empty_tail_assistant(app, exit.tail_assistant_idx);
    if exit.show_interrupted_hint {
        push_interrupted_hint(app);
    }
    if removed_tail_assistant.is_none()
        && (exit.turn_was_active || exit.cancelled_requested)
    {
        mark_turn_exit_assistant_layout_dirty(app, exit.tail_assistant_idx);
    }
    app.clear_active_turn_assistant();
    super::notices::clear_turn_notice_tracking(app);
}

/// Active-path entry point used by `sdk_message::apply_result_finalize`.
/// Runs the TUI presentation; lifecycle reset to `Idle` happens via
/// the bucket writes inside `apply_turn_complete_presentation`.
pub(super) fn handle_turn_complete_event(
    app: &mut App,
    session_key: &SessionKey,
    terminal_reason: Option<forge_primitives::TerminalReason>,
) {
    apply_turn_complete_presentation(app, session_key, terminal_reason);
}

/// `SessionUpdate::TurnComplete` reducer. The workspace
/// already updated DomainSession via the synthesized turn event in
/// the SDK message flow; this reducer is for the dispatcher path
/// only. (The active-session path from `sdk_message.rs` calls
/// [`handle_turn_complete_event`] directly to run the operational
/// hook.)
pub(super) fn apply_session_update_turn_complete(
    app: &mut App,
    key: SessionKey,
    terminal_reason: Option<forge_primitives::TerminalReason>,
) {
    apply_turn_complete_presentation(app, &key, terminal_reason);
}

fn apply_turn_complete_presentation(
    app: &mut App,
    session_key: &SessionKey,
    terminal_reason: Option<forge_primitives::TerminalReason>,
) {
    if app.active_session_key.as_ref() != Some(session_key) {
        let Some(session) = app.session_mut(session_key) else {
            // Promoted to `error` for triage visibility. When this
            // fires the bucket's `lifecycle_state` never returns to
            // `Idle`, the Projects pane glyph stays as the spinner,
            // and (if the active bucket is the one whose TurnComplete
            // dropped) the chat spinner sits on "Thinking..." until
            // forge restarts.
            let bucket_keys: Vec<String> =
                app.sessions.keys().map(|k| k.as_str().to_owned()).collect();
            tracing::error!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "turn_complete_dropped",
                message = "turn complete dropped for an unknown session",
                outcome = "dropped",
                session_key = %session_key.as_str(),
                active_session_key = ?app.active_session_key.as_ref().map(|k| k.as_str().to_owned()),
                bucket_keys = ?bucket_keys,
                reason = "unknown_session",
            );
            return;
        };
        let cancelled_requested = session.pending_cancel;
        let tool_status = if cancelled_requested {
            model::ToolCallStatus::Failed
        } else {
            model::ToolCallStatus::Completed
        };
        finalize_background_tool_calls(session, tool_status);
        session.pending_cancel = false;
        session.cancelled_turn_pending_hint = false;
        session.active_turn_assistant_message_idx = None;
        session.turn_notice_refs.clear();
        let _ = session;
        // Lifecycle: background bucket's turn has wrapped — return
        // it to Idle so the Projects pane drops the spinner glyph,
        // and reset the per-turn SDK state.
        if let Some(bucket) = app.sessions.get_mut(session_key) {
            bucket.lifecycle_state = crate::app::session::SessionLifecycleState::Idle;
            bucket.turn_state = forge_primitives::runtime::SessionTurnState::default();
        }
        if let Some(reason) = terminal_reason {
            tracing::debug!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "turn_complete_terminal_reason_background",
                message = "background turn completed with SDK terminal reason",
                outcome = "success",
                session_key = %session_key.as_str(),
                terminal_reason = reason.as_stored(),
            );
        }
        return;
    }
    let exit = begin_turn_exit(app, true);
    let turn_was_active = exit.turn_was_active;
    let tail_assistant_idx_before = exit.tail_assistant_idx;
    if let Some(reason) = terminal_reason {
        tracing::debug!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "turn_complete_terminal_reason",
            message = "turn completed with SDK terminal reason",
            outcome = "success",
            terminal_reason = reason.as_stored(),
        );
    }
    let tool_status = if exit.cancelled_requested {
        model::ToolCallStatus::Failed
    } else {
        model::ToolCallStatus::Completed
    };
    finish_ready_turn_exit(app, exit, tool_status);
    // Lifecycle: turn done, active session returns to Idle and
    // turn_state resets. The Projects pane drops the spinner glyph
    // back to the default foreground color.
    if let Some(key) = app.active_session_key.clone() {
        if let Some(bucket) = app.sessions.get_mut(&key) {
            bucket.turn_state = forge_primitives::runtime::SessionTurnState::default();
        }
        super::set_bucket_lifecycle_state(
            app,
            &key,
            crate::app::session::SessionLifecycleState::Idle,
        );
    }
    crate::app::session_runtime::request_context_usage_refresh(app);
    if turn_was_active {
        app.notifications.notify(
            app.config.preferred_notification_channel_effective(),
            super::super::notify::NotifyEvent::TurnComplete,
        );
    }
    // Mid-turn submits leave user bubbles after the active assistant.
    // When this turn wraps, claude immediately starts another turn to
    // consume those buffered prompts — but its first content chunk
    // can take 1-2s to arrive, leaving a gap where the chat looks
    // idle. Anticipate that next turn: push an empty assistant
    // placeholder + flip back to Thinking so the spinner shows
    // continuously through the handoff.
    anticipate_buffered_next_turn(app, tail_assistant_idx_before);
    // No git-diff refresh trigger here — the `git_diff` module's
    // periodic ticker (1s poke + 10s staleness rule) catches any
    // post-turn file changes within the next ticker pass.
}

/// When a turn wraps with mid-turn-submitted user bubbles still at
/// the tail (claude buffered them internally and is about to consume
/// them in a fresh turn), push the assistant placeholder + spinner
/// for that next turn proactively so the user sees continuous
/// activity instead of a brief "idle" gap.
///
/// `tail_assistant_idx_before` is the position of the assistant
/// message at the start of turn exit (before
/// `remove_empty_tail_assistant` ran). Mid-turn submits push user
/// bubbles AFTER that index, so a tail-User whose index exceeds
/// `tail_assistant_idx_before` is unambiguously a mid-turn submit.
/// A tail-User at or before that index means the prior assistant
/// placeholder was empty + got removed (degenerate turn) and there
/// were no mid-turn submits — don't anticipate.
fn anticipate_buffered_next_turn(app: &mut App, tail_assistant_idx_before: Option<usize>) {
    let Some(last_idx) = app.messages().len().checked_sub(1) else {
        return;
    };
    let last_is_user = app
        .messages()
        .get(last_idx)
        .is_some_and(|m| matches!(m.role, crate::app::MessageRole::User));
    if !last_is_user {
        return;
    }
    match tail_assistant_idx_before {
        Some(prior_idx) if last_idx > prior_idx => {}
        _ => return,
    }
    app.push_message_tracked(crate::app::ChatMessage::new(
        crate::app::MessageRole::Assistant,
        Vec::new(),
        None,
    ));
    app.bind_active_turn_assistant_to_tail();
    app.enforce_history_retention_tracked();
    app.status = super::super::AppStatus::Thinking;
    if let Some(key) = app.active_session_key.clone() {
        super::set_bucket_lifecycle_state(
            app,
            &key,
            crate::app::session::SessionLifecycleState::Running,
        );
    }
    tracing::debug!(
        target: crate::logging::targets::APP_SESSION,
        event_name = "anticipated_buffered_turn",
        message = "trailing user bubbles → pushed assistant placeholder for next turn",
        outcome = "success",
    );
}

/// Active-path entry point used by `sdk_message::apply_result_finalize`.
/// Runs the TUI presentation; lifecycle reset to `Idle` happens via
/// the bucket writes inside `apply_turn_error_presentation`.
pub(super) fn handle_turn_error_event(
    app: &mut App,
    session_key: &SessionKey,
    msg: &str,
    classified: Option<TurnErrorClass>,
    terminal_reason: Option<forge_primitives::TerminalReason>,
) {
    apply_turn_error_presentation(app, session_key, msg, classified, terminal_reason);
}

/// `SessionUpdate::TurnError` reducer. The workspace
/// already updated DomainSession via the synthesized turn event in
/// the SDK message flow; this reducer is for the dispatcher path
/// only. Maps the workspace-side
/// [`forge_workspace::TurnErrorClass`] to the local
/// [`TurnErrorClass`] so the presentation helper sees the same enum
/// it has consumed since before the protocol layer existed.
pub(super) fn apply_session_update_turn_error(
    app: &mut App,
    key: SessionKey,
    message: String,
    class: Option<forge_workspace::TurnErrorClass>,
    terminal_reason: Option<forge_primitives::TerminalReason>,
) {
    let local_class = class.map(map_workspace_turn_error_class);
    apply_turn_error_presentation(app, &key, &message, local_class, terminal_reason);
}

fn map_workspace_turn_error_class(class: forge_workspace::TurnErrorClass) -> TurnErrorClass {
    match class {
        forge_workspace::TurnErrorClass::PlanLimit => TurnErrorClass::PlanLimit,
        forge_workspace::TurnErrorClass::AuthRequired => TurnErrorClass::AuthRequired,
        forge_workspace::TurnErrorClass::Internal => TurnErrorClass::Internal,
        forge_workspace::TurnErrorClass::Other => TurnErrorClass::Other,
    }
}

fn apply_turn_error_presentation(
    app: &mut App,
    session_key: &SessionKey,
    msg: &str,
    classified: Option<TurnErrorClass>,
    terminal_reason: Option<forge_primitives::TerminalReason>,
) {
    if app.active_session_key.as_ref() != Some(session_key) {
        let Some(session) = app.session_mut(session_key) else {
            tracing::warn!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "turn_error_dropped",
                message = "turn error dropped for an unknown session",
                outcome = "dropped",
                session_key = %session_key.as_str(),
                reason = "unknown_session",
            );
            return;
        };
        // Background-session turn error: clear the bucket's
        // turn-tracking flags and finalize tool calls; skip all
        // user-visible UI side effects (notifications, exit_error,
        // chat message inserts). The bucket is logically failed.
        let cancelled_requested = session.pending_cancel;
        finalize_background_tool_calls(session, model::ToolCallStatus::Failed);
        session.pending_cancel = false;
        session.cancelled_turn_pending_hint = false;
        session.active_turn_assistant_message_idx = None;
        session.turn_notice_refs.clear();
        let _ = session;
        // Lifecycle: turn ended (with error) — return the background
        // bucket to Idle so the Projects pane drops the spinner glyph,
        // and reset the per-turn SDK state.
        if let Some(bucket) = app.sessions.get_mut(session_key) {
            bucket.lifecycle_state = crate::app::session::SessionLifecycleState::Idle;
            bucket.turn_state = forge_primitives::runtime::SessionTurnState::default();
        }
        let summary = summarize_internal_error(msg);
        if cancelled_requested {
            tracing::warn!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "turn_error_suppressed_background",
                message = "background turn error suppressed after cancellation request",
                outcome = "cancelled",
                session_key = %session_key.as_str(),
                error_preview = %summary,
                terminal_reason = terminal_reason.map_or("", forge_primitives::TerminalReason::as_stored),
            );
        } else {
            let error_class = classified.unwrap_or_else(|| classify_turn_error(msg));
            tracing::error!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "turn_error_received_background",
                message = "background turn error received",
                outcome = "failure",
                session_key = %session_key.as_str(),
                error_class = ?error_class,
                error_preview = %summary,
                terminal_reason = terminal_reason.map_or("", forge_primitives::TerminalReason::as_stored),
            );
        }
        return;
    }
    let exit = begin_turn_exit(app, true);

    if exit.cancelled_requested {
        let summary = summarize_internal_error(msg);
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "turn_error_suppressed",
            message = "turn error suppressed after cancellation request",
            outcome = "cancelled",
            error_preview = %summary,
            terminal_reason = terminal_reason.map_or("", forge_primitives::TerminalReason::as_stored),
        );
        *app.pending_submit_mut() = None;
        finish_ready_turn_exit(app, exit, model::ToolCallStatus::Failed);
        // Lifecycle: cancelled turn — back to Idle, reset turn_state.
        if let Some(key) = app.active_session_key.clone() {
            if let Some(bucket) = app.sessions.get_mut(&key) {
                bucket.turn_state = forge_primitives::runtime::SessionTurnState::default();
            }
            super::set_bucket_lifecycle_state(
                app,
                &key,
                crate::app::session::SessionLifecycleState::Idle,
            );
        }
        crate::app::session_runtime::request_context_usage_refresh(app);
        if app.active_view == super::super::ActiveView::Chat {
            // Issue #85 (revised): no drain — claude handles queueing.
        }
        return;
    }

    let error_class = classified.unwrap_or_else(|| classify_turn_error(msg));
    let summary = summarize_internal_error(msg);
    tracing::error!(
        target: crate::logging::targets::APP_SESSION,
        event_name = "turn_error_received",
        message = "turn error received",
        outcome = "failure",
        error_class = ?error_class,
        error_preview = %summary,
        terminal_reason = terminal_reason.map_or("", forge_primitives::TerminalReason::as_stored),
    );
    match error_class {
        TurnErrorClass::PlanLimit => {
            tracing::warn!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "turn_error_classified",
                message = "turn error classified as plan limit",
                outcome = "degraded",
                error_class = "plan_limit",
                error_preview = %summary,
            );
        }
        TurnErrorClass::AuthRequired => {
            tracing::warn!(
                target: crate::logging::targets::APP_AUTH,
                event_name = "turn_error_classified",
                message = "turn error indicates authentication is required",
                outcome = "degraded",
                error_class = "auth_required",
                error_preview = %summary,
            );
            app.exit_error = Some(crate::error::AppError::AuthRequired);
            app.should_quit = true;
        }
        TurnErrorClass::Internal => {
            tracing::debug!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "turn_error_classified",
                message = "turn error classified as internal SDK error",
                outcome = "degraded",
                error_class = "internal",
                error_preview = %summary,
            );
        }
        TurnErrorClass::Other => {}
    }
    app.finalize_turn_runtime_artifacts(model::ToolCallStatus::Failed);
    app.input_mut().clear();
    *app.pending_submit_mut() = None;
    app.status = AppStatus::Error;
    let rate_limit_context = if matches!(error_class, TurnErrorClass::PlanLimit) {
        app.last_rate_limit_update()
            .cloned()
            .filter(|update| !matches!(update.status, model::RateLimitStatus::Allowed))
    } else {
        None
    };
    let removed_tail_assistant = remove_empty_tail_assistant(app, exit.tail_assistant_idx);
    push_turn_error_message(app, msg, error_class, rate_limit_context.as_ref());
    if removed_tail_assistant.is_none() && exit.turn_was_active {
        mark_turn_exit_assistant_layout_dirty(app, exit.tail_assistant_idx);
    }
    app.clear_active_turn_assistant();
    super::notices::clear_turn_notice_tracking(app);
    // Lifecycle: errored turn — back to Idle, reset turn_state. The
    // active session status itself is already AppStatus::Error (set
    // above) so the input remains locked; lifecycle is the per-session
    // pane glyph, independent of the App-global input lock.
    if let Some(key) = app.active_session_key.clone() {
        if let Some(bucket) = app.sessions.get_mut(&key) {
            bucket.turn_state = forge_primitives::runtime::SessionTurnState::default();
        }
        super::set_bucket_lifecycle_state(
            app,
            &key,
            crate::app::session::SessionLifecycleState::Idle,
        );
    }
    crate::app::session_runtime::request_context_usage_refresh(app);
}

fn push_interrupted_hint(app: &mut App) {
    app.push_message_tracked(ChatMessage::new(
        MessageRole::System(Some(SystemSeverity::Info)),
        vec![MessageBlock::Text(TextBlock::from_complete(CONVERSATION_INTERRUPTED_HINT))],
        None,
    ));
    app.enforce_history_retention_tracked();
    app.active_viewport_mut().engage_auto_scroll();
}

fn remove_empty_tail_assistant(app: &mut App, idx: Option<usize>) -> Option<usize> {
    let idx = idx?;
    let should_remove = app
        .messages()
        .get(idx)
        .is_some_and(|msg| matches!(msg.role, MessageRole::Assistant) && msg.blocks.is_empty());
    if !should_remove {
        return None;
    }
    app.remove_message_tracked(idx)?;
    Some(idx)
}

fn mark_turn_exit_assistant_layout_dirty(app: &mut App, idx: Option<usize>) {
    let Some(idx) = idx else {
        return;
    };
    if app.messages().get(idx).is_some_and(|msg| matches!(msg.role, MessageRole::Assistant)) {
        app.invalidate_layout(InvalidationLevel::MessageChanged(idx));
    }
}

fn push_turn_error_message(
    app: &mut App,
    error: &str,
    class: TurnErrorClass,
    rate_limit_context: Option<&model::RateLimitUpdate>,
) {
    match class {
        TurnErrorClass::PlanLimit => {
            let base_message = {
                let summary = summarize_internal_error(error);
                format!(
                    "Turn blocked by account or plan limits: {summary}\n\n{PLAN_LIMIT_NEXT_STEPS_HINT}\n\n{TURN_ERROR_INPUT_LOCK_HINT}"
                )
            };
            let (severity, message, dedup_key) = if let Some(update) = rate_limit_context {
                let prefix = format_rate_limit_summary(update);
                let severity = match update.status {
                    model::RateLimitStatus::AllowedWarning => SystemSeverity::Warning,
                    model::RateLimitStatus::Rejected | model::RateLimitStatus::Allowed => {
                        SystemSeverity::Error
                    }
                };
                (severity, format!("{prefix}\n\n{base_message}"), rate_limit_notice_key(update))
            } else {
                (
                    SystemSeverity::Error,
                    base_message,
                    super::super::NoticeDedupKey::RateLimit(super::super::RateLimitIncidentKey {
                        rate_limit_type: None,
                        resets_at_bucket: None,
                    }),
                )
            };
            super::notices::upsert_turn_notice(
                app,
                dedup_key,
                NoticeStage::PlanLimitTurnError,
                severity,
                &message,
            );
        }
        TurnErrorClass::AuthRequired => {
            let message =
                format!("{AUTH_REQUIRED_NEXT_STEPS_HINT}\n\n{TURN_ERROR_INPUT_LOCK_HINT}");
            super::push_system_message_with_severity(app, None, &message);
        }
        TurnErrorClass::Internal | TurnErrorClass::Other => {
            // Empty `error` means the source side had no useful detail
            // beyond the SDK's bookkeeping subtype — render the bare
            // "Turn failed." rather than a stranded ":" with nothing
            // after it.
            let message = if error.trim().is_empty() {
                format!("Turn failed.\n\n{TURN_ERROR_INPUT_LOCK_HINT}")
            } else {
                format!("Turn failed: {error}\n\n{TURN_ERROR_INPUT_LOCK_HINT}")
            };
            super::push_system_message_with_severity(app, None, &message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;

    fn empty_assistant_message() -> ChatMessage {
        ChatMessage::new(MessageRole::Assistant, Vec::new(), None)
    }

    fn user_message(text: &str) -> ChatMessage {
        ChatMessage::new(
            MessageRole::User,
            vec![MessageBlock::Text(TextBlock::from_complete(text))],
            None,
        )
    }

    fn active_session_key(app: &App) -> SessionKey {
        app.active_session_key.clone().expect("active session key seeded by App::test_default")
    }

    #[test]
    fn turn_complete_removes_empty_tail_assistant() {
        let mut app = App::test_default();
        app.status = AppStatus::Thinking;
        app.active_messages_mut().push(user_message("hello"));
        app.active_messages_mut().push(empty_assistant_message());

        let key = active_session_key(&app);
        apply_session_update_turn_complete(&mut app, key, None);

        assert_eq!(app.messages().len(), 1);
        assert!(matches!(app.messages()[0].role, MessageRole::User));
    }

    #[test]
    fn cancelled_turn_error_removes_empty_tail_assistant_before_hint() {
        let mut app = App::test_default();
        app.status = AppStatus::Thinking;
        app.set_pending_cancel(true);
        app.active_messages_mut().push(user_message("hello"));
        app.active_messages_mut().push(empty_assistant_message());

        let key = active_session_key(&app);
        apply_session_update_turn_error(&mut app, key, "cancelled".to_owned(), None, None);

        assert_eq!(app.messages().len(), 2);
        assert!(matches!(app.messages()[0].role, MessageRole::User));
        assert!(matches!(app.messages()[1].role, MessageRole::System(Some(SystemSeverity::Info))));
    }

    #[test]
    fn turn_error_removes_empty_tail_assistant_before_error_message() {
        let mut app = App::test_default();
        app.status = AppStatus::Thinking;
        app.active_messages_mut().push(user_message("hello"));
        app.active_messages_mut().push(empty_assistant_message());

        let key = active_session_key(&app);
        apply_session_update_turn_error(&mut app, key, "boom".to_owned(), None, None);

        assert_eq!(app.messages().len(), 2);
        assert!(matches!(app.messages()[0].role, MessageRole::User));
        assert!(matches!(app.messages()[1].role, MessageRole::System(None)));
    }

    #[test]
    fn turn_complete_for_background_session_does_not_touch_active_messages() {
        use crate::app::session::UiSession;
        let mut app = App::test_default();
        app.status = AppStatus::Thinking;
        app.active_messages_mut().push(user_message("active hello"));
        app.active_messages_mut().push(empty_assistant_message());
        let active_messages_before = app.messages().len();

        let bg_key = SessionKey::from_str_for_test("background-session");
        let mut bg_session = UiSession::new(bg_key.clone());
        bg_session.messages.push(user_message("bg hello"));
        bg_session.messages.push(empty_assistant_message());
        app.sessions.insert(bg_key.clone(), bg_session);

        apply_session_update_turn_complete(&mut app, bg_key.clone(), None);

        // Active session messages untouched.
        assert_eq!(app.messages().len(), active_messages_before);
        // Active app status unchanged (still Thinking) — background
        // turn-complete must not flip the active session to Ready.
        assert!(matches!(app.status, AppStatus::Thinking));
        // Background bucket's active_turn_assistant_message_idx cleared.
        let bg = app.sessions.get(&bg_key).expect("bg present");
        assert!(bg.active_turn_assistant_message_idx.is_none());
    }

    #[test]
    fn turn_cancelled_for_background_session_marks_only_target_bucket() {
        use crate::app::session::UiSession;
        let mut app = App::test_default();
        let bg_key = SessionKey::from_str_for_test("background-session");
        let bg_session = UiSession::new(bg_key.clone());
        app.sessions.insert(bg_key.clone(), bg_session);

        // Active session has no pending cancel origin set.
        assert!(!app.pending_cancel());

        apply_session_update_turn_cancelled(&mut app, &bg_key);

        // Background bucket got the cancel marker.
        let bg = app.sessions.get(&bg_key).expect("bg present");
        assert!(bg.pending_cancel);
        assert!(bg.cancelled_turn_pending_hint);
        // Active session's cancel state untouched.
        assert!(!app.pending_cancel());
    }

    #[test]
    fn turn_error_for_background_session_does_not_set_should_quit() {
        use crate::app::session::UiSession;
        let mut app = App::test_default();
        let bg_key = SessionKey::from_str_for_test("background-session");
        let bg_session = UiSession::new(bg_key.clone());
        app.sessions.insert(bg_key.clone(), bg_session);

        // Auth-required class would normally set should_quit=true when
        // applied to the active session; for a background session it
        // must not.
        apply_session_update_turn_error(
            &mut app,
            bg_key.clone(),
            "auth required".to_owned(),
            Some(forge_workspace::TurnErrorClass::AuthRequired),
            None,
        );

        assert!(!app.should_quit);
        assert!(app.exit_error.is_none());
    }
}
