use std::time::Instant;

use super::state::types::QueuedMessage;
use super::{App, AppStatus, CancelOrigin, ChatMessage, MessageBlock, MessageRole, TextBlock};
use crate::agent::model;
use crate::app::slash;

pub(super) fn submit_input(app: &mut App) {
    if matches!(app.status, AppStatus::Connecting | AppStatus::CommandPending | AppStatus::Error) {
        return;
    }

    // Dismiss any open mention dropdown
    *app.mention_mut() = None;
    *app.slash_mut() = None;
    *app.subagent_mut() = None;

    // No connection yet - can't submit
    let text = app.input().text();
    if text.trim().is_empty() {
        return;
    }
    app.set_prompt_suggestion(None);

    // Slash commands fire regardless of in-flight turn state — they're
    // TUI-side meta-commands (e.g. `/config`, `/mcp`, `/status`) that
    // don't queue. `try_handle_submit` returns `true` when it consumed
    // the input; `false` if it didn't match (e.g. `/compact` passes
    // through as a normal user prompt — that DOES queue if busy).
    if slash::try_handle_submit(app, &text) {
        app.input_mut().clear();
        app.sync_help_open_with_input();
        return;
    }

    // Issue #85: send is non-destructive. If a turn is in flight, queue
    // the message rather than cancelling. The queue drains on the next
    // TurnComplete (whether natural or Escape-induced cancel). To
    // explicitly cancel the in-flight turn, the user presses Escape —
    // which routes through `request_cancel(Manual)` from keys.rs.
    if is_turn_busy(app) {
        let images = std::mem::take(app.pending_images_mut());
        enqueue_message(app, text, images);
        app.input_mut().clear();
        app.sync_help_open_with_input();
        return;
    }

    app.input_mut().clear();
    app.sync_help_open_with_input();
    dispatch_prompt_turn(app, text);
}

fn is_turn_busy(app: &App) -> bool {
    matches!(app.status, AppStatus::Thinking | AppStatus::Running)
        || app.pending_cancel_origin().is_some()
        || app.is_compacting()
}

/// Cancel the in-flight turn. As of issue #85 the only routine caller
/// is the Escape keybinding (`CancelOrigin::Manual`); submit no longer
/// calls this for `AutoQueue` cancels — submit queues instead.
/// `CancelOrigin::AutoQueue` is still set by other code paths (paste
/// burst, error recovery) that haven't migrated to the queue model.
pub(super) fn request_cancel(app: &mut App, origin: CancelOrigin) -> Result<(), String> {
    if matches!(origin, CancelOrigin::Manual) {
        app.set_pending_auto_submit_after_cancel(false);
    }

    if !matches!(app.status, AppStatus::Thinking | AppStatus::Running) {
        return Ok(());
    }

    if let Some(existing_origin) = app.pending_cancel_origin() {
        if matches!(existing_origin, CancelOrigin::AutoQueue)
            && matches!(origin, CancelOrigin::Manual)
        {
            app.set_pending_cancel_origin(Some(CancelOrigin::Manual));
            app.set_cancelled_turn_pending_hint(true);
        }
        return Ok(());
    }

    if !app.has_active_agent() {
        return Err("not connected yet".to_owned());
    }
    let Some(sid) = app.session_id() else {
        return Err("no active session".to_owned());
    };

    let session_id = sid.to_string();
    app.dispatch_command(|key| forge_workspace::Command::Cancel { key })
        .map_err(|e| e.to_string())?;
    app.set_pending_cancel_origin(Some(origin));
    app.set_cancelled_turn_pending_hint(matches!(origin, CancelOrigin::Manual));
    let session_key = forge_workspace::SessionKey::from_session_id(session_id.clone());
    let _ = app.update_tx.send(forge_workspace::SessionUpdate::TurnCancelled { key: session_key });
    tracing::info!(
        target: crate::logging::targets::APP_INPUT,
        event_name = "turn_cancel_requested",
        message = "turn cancel requested",
        outcome = "success",
        session_id = %session_id,
        origin = ?origin,
    );
    Ok(())
}

/// **Vestigial** as of issue #85: the cancel-on-submit + post-cancel
/// auto-submit pattern was replaced with message queueing (see
/// [`drain_queued_messages`]). `pending_auto_submit_after_cancel` is
/// never set to `true` in the new submit flow, so this function is a
/// no-op in practice. Kept in place to avoid churning every turn-
/// complete consumer in the behaviour-change PR. Future cleanup:
/// delete this function + the field together once the queue path has
/// soaked.
pub(super) fn maybe_auto_submit_after_cancel(app: &mut App) {
    if !app.pending_auto_submit_after_cancel() {
        return;
    }
    if !matches!(app.status, AppStatus::Ready) || app.pending_cancel_origin().is_some() {
        return;
    }
    if app.input().text().trim().is_empty() {
        app.set_pending_auto_submit_after_cancel(false);
        return;
    }
    app.set_pending_auto_submit_after_cancel(false);
    submit_input(app);
}

/// Drain queued messages on a turn-complete boundary. Issue #85.
///
/// Concatenates all queued texts with `\n\n` paragraph breaks, merges
/// attachments, fires a single fresh `Command::Prompt`. The dimmed
/// user bubbles representing the queued messages get un-dimmed so the
/// user sees the model has now seen them. Chat history retains the
/// N-separate-bubble visual shape even though the wire sees one
/// combined prompt.
///
/// Caller contract: only invoke after the active turn has fully ended
/// (status transitioned back to `Ready`, lifecycle to `Idle`). The
/// `apply_turn_complete_presentation` and cancelled-error paths in
/// `events::turn` are the wired call sites.
///
/// V1 scope: only the ACTIVE session's queue drains here. Background-
/// session queues drain when the user switches to them and the next
/// turn completes there. Real-error (non-cancelled) blocks drain —
/// queue waits until the user clears the error. Both are documented
/// limitations in #85; revisit in v1.5 if real-world usage hits them.
pub(super) fn drain_queued_messages(app: &mut App) {
    if !matches!(app.status, AppStatus::Ready) {
        return;
    }
    if app.pending_cancel_origin().is_some() {
        return;
    }
    let Some(key) = app.active_session_key.clone() else {
        return;
    };

    // Collect the queue before mutating messages — avoids overlapping
    // borrows between `app.session_mut` and `app.messages_mut`.
    let queued: Vec<QueuedMessage> = {
        let Some(session) = app.session_mut(&key) else {
            return;
        };
        if session.queued_messages.is_empty() {
            return;
        }
        session.queued_messages.drain(..).collect()
    };

    // Un-dim the bubbles. Stale indices (e.g. trimmed by history
    // retention) are skipped silently — the dimmed visual is already
    // gone in that case.
    if let Some(bucket) = app.try_active_bucket_mut() {
        for q in &queued {
            let idx = q.chat_message_idx;
            if let Some(msg) = bucket.messages.get_mut(idx)
                && matches!(msg.role, MessageRole::User)
            {
                msg.queued = false;
                msg.invalidate_render_cache();
            }
        }
    }

    let queue_size = queued.len();
    let combined_text = queued.iter().map(|q| q.text.as_str()).collect::<Vec<_>>().join("\n\n");
    let combined_attachments: Vec<crate::app::clipboard_image::ImageAttachment> =
        queued.into_iter().flat_map(|q| q.attachments.into_iter()).collect();

    tracing::info!(
        target: crate::logging::targets::APP_INPUT,
        event_name = "queue_drained",
        message = "queued messages drained into a single combined turn",
        outcome = "start",
        queue_size,
    );

    fire_combined_turn(app, combined_text, combined_attachments);
}

/// Fire the combined queued payload as a fresh turn. Mirrors
/// `dispatch_prompt_turn` but skips the user-message push — the
/// dimmed bubbles already exist in chat history; the drain handler
/// un-dimmed them above.
fn fire_combined_turn(
    app: &mut App,
    text: String,
    images: Vec<crate::app::clipboard_image::ImageAttachment>,
) {
    let _ = app.finalize_in_progress_tool_calls(model::ToolCallStatus::Failed);

    if !app.has_active_agent() {
        return;
    }
    let Some(sid) = app.session_id() else {
        return;
    };
    let input_chars = text.chars().count();
    let session_id = sid.to_string();

    // Push the empty assistant message — the thinking indicator
    // anchors here. No user message push: the user bubbles are
    // already in chat from the earlier `enqueue_message` calls.
    app.push_message_tracked(ChatMessage::new(MessageRole::Assistant, Vec::new(), None));
    app.bind_active_turn_assistant_to_tail();
    app.enforce_history_retention_tracked();
    app.status = AppStatus::Thinking;
    if let Some(key) = app.active_session_key.clone() {
        crate::app::events::set_bucket_lifecycle_state(
            app,
            &key,
            crate::app::session::SessionLifecycleState::Running,
        );
    }
    app.active_viewport_mut().engage_auto_scroll();

    let tx = app.update_tx.clone();
    let prompt_text = text;
    match app.dispatch_command(|key| forge_workspace::Command::Prompt {
        key,
        text: prompt_text,
        attachments: images,
    }) {
        Ok(()) => {
            crate::app::session_runtime::request_context_usage_refresh(app);
            tracing::info!(
                target: crate::logging::targets::APP_INPUT,
                event_name = "queue_drained_dispatched",
                message = "combined queued prompt dispatched",
                outcome = "success",
                session_id = %session_id,
                input_chars,
            );
        }
        Err(e) => {
            let session_key = forge_workspace::SessionKey::from_session_id(session_id);
            let _ = tx.send(forge_workspace::SessionUpdate::TurnError {
                key: session_key,
                message: e.to_string(),
                class: None,
                terminal_reason: None,
            });
        }
    }
}

/// Append a dimmed user bubble to the chat AND push a `QueuedMessage`
/// onto the active session's queue. Called when the user submits while
/// a turn is in flight (issue #85).
fn enqueue_message(
    app: &mut App,
    text: String,
    attachments: Vec<crate::app::clipboard_image::ImageAttachment>,
) {
    let user_blocks = vec![MessageBlock::Text(TextBlock::from_complete(&text))];
    let queued_bubble = ChatMessage::new_queued(MessageRole::User, user_blocks);
    app.push_message_tracked(queued_bubble);
    app.enforce_history_retention_tracked();
    let chat_message_idx = app.messages().len().saturating_sub(1);

    let Some(key) = app.active_session_key.clone() else {
        return;
    };
    let queue_depth_after = match app.session_mut(&key) {
        Some(session) => {
            session.queued_messages.push_back(QueuedMessage {
                text,
                attachments,
                chat_message_idx,
                queued_at: Instant::now(),
            });
            session.queued_messages.len()
        }
        None => return,
    };

    app.active_viewport_mut().engage_auto_scroll();

    tracing::debug!(
        target: crate::logging::targets::APP_INPUT,
        event_name = "message_queued",
        message = "message queued for drain on next turn-complete",
        outcome = "success",
        queue_depth_after,
    );
}

fn dispatch_prompt_turn(app: &mut App, text: String) {
    // New turn started by user input: force-stop stale tool calls from older turns
    // so their spinners don't continue during this turn.
    let _ = app.finalize_in_progress_tool_calls(model::ToolCallStatus::Failed);

    if !app.has_active_agent() {
        return;
    }
    let Some(sid) = app.session_id() else {
        return;
    };
    let input_chars = text.chars().count();
    let session_id = sid.to_string();

    // Take pending images for this turn.
    let images = std::mem::take(app.pending_images_mut());

    let user_blocks = vec![MessageBlock::Text(TextBlock::from_complete(&text))];

    app.push_message_tracked(ChatMessage::new(MessageRole::User, user_blocks, None));
    // Create empty assistant message immediately -- message.rs shows thinking indicator
    app.push_message_tracked(ChatMessage::new(MessageRole::Assistant, Vec::new(), None));
    app.bind_active_turn_assistant_to_tail();
    app.enforce_history_retention_tracked();
    app.status = AppStatus::Thinking;
    // Lifecycle: turn started, the active session moves into Running.
    // The Projects pane reads this so the spinner glyph picks up the
    // accent color while the turn is in flight.
    if let Some(key) = app.active_session_key.clone() {
        crate::app::events::set_bucket_lifecycle_state(
            app,
            &key,
            crate::app::session::SessionLifecycleState::Running,
        );
    }
    app.active_viewport_mut().engage_auto_scroll();

    let tx = app.update_tx.clone();
    // The text already contains [Image #N] badges from the textarea,
    // so the model can correlate user references with image attachments.
    let prompt_text = text;
    match app.dispatch_command(|key| forge_workspace::Command::Prompt {
        key,
        text: prompt_text,
        attachments: images,
    }) {
        Ok(()) => {
            crate::app::session_runtime::request_context_usage_refresh(app);
            tracing::info!(
                target: crate::logging::targets::APP_INPUT,
                event_name = "prompt_dispatched",
                message = "prompt dispatched to the bridge",
                outcome = "success",
                session_id = %session_id,
                input_chars,
            );
        }
        Err(e) => {
            let session_key = forge_workspace::SessionKey::from_session_id(session_id);
            let _ = tx.send(forge_workspace::SessionUpdate::TurnError {
                key: session_key,
                message: e.to_string(),
                class: None,
                terminal_reason: None,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::app::ActiveView;

    fn app_with_connection()
    -> (App, tokio::sync::mpsc::UnboundedReceiver<forge_primitives::Command>) {
        let mut app = App::test_default();
        let rx = app.install_testing_stub();
        app.set_session_id(Some(model::SessionId::new("session-1")));
        (app, rx)
    }

    #[test]
    fn submit_input_while_idle_dispatches_prompt() {
        let (mut app, mut rx) = app_with_connection();
        app.status = AppStatus::Ready;
        app.input_mut().set_text("hello");

        submit_input(&mut app);

        assert!(app.input().text().is_empty());
        assert!(matches!(app.status, AppStatus::Thinking));
        // user message + empty assistant message
        assert_eq!(app.messages().len(), 2);
        let prompt = rx.try_recv().expect("prompt command should be sent");
        assert!(matches!(
            prompt,
            forge_primitives::Command::PromptWithImages { session_id, .. } if session_id == "session-1"
        ));
    }

    #[test]
    fn submit_input_while_running_queues_instead_of_cancelling() {
        let (mut app, mut rx) = app_with_connection();
        app.status = AppStatus::Running;
        app.input_mut().set_text("queued prompt");

        submit_input(&mut app);

        // Input cleared (queued; nothing left to edit).
        assert!(app.input().text().is_empty());
        // No cancel dispatched — send is non-destructive now.
        assert!(rx.try_recv().is_err(), "submit while busy must not cancel");
        // Status unchanged.
        assert!(matches!(app.status, AppStatus::Running));
        // Dimmed user bubble appended to chat history.
        assert_eq!(app.messages().len(), 1);
        let msg = &app.messages()[0];
        assert!(matches!(msg.role, MessageRole::User));
        assert!(msg.queued, "queued bubble must carry the queued flag");
        // Queue contains the message.
        let bucket = app.try_active_bucket_mut().expect("active bucket exists");
        assert_eq!(bucket.queued_messages.len(), 1);
        assert_eq!(bucket.queued_messages[0].text, "queued prompt");
        // No auto-submit-after-cancel set (vestigial path stays cold).
        assert!(!bucket.pending_auto_submit_after_cancel);
        assert!(app.pending_cancel_origin().is_none());
    }

    #[test]
    fn drain_queued_messages_fires_combined_prompt() {
        let (mut app, mut rx) = app_with_connection();

        // Queue three messages while busy.
        app.status = AppStatus::Running;
        app.input_mut().set_text("first");
        submit_input(&mut app);
        app.input_mut().set_text("second");
        submit_input(&mut app);
        app.input_mut().set_text("third");
        submit_input(&mut app);

        // Sanity: three dimmed bubbles + zero turns dispatched.
        assert_eq!(app.messages().len(), 3);
        assert!(rx.try_recv().is_err());
        {
            let bucket = app.try_active_bucket_mut().expect("active bucket exists");
            assert_eq!(bucket.queued_messages.len(), 3);
        }

        // Transition to Ready (simulates TurnComplete arriving).
        app.status = AppStatus::Ready;

        drain_queued_messages(&mut app);

        // Queue empty; bubbles un-dimmed; single combined prompt dispatched.
        {
            let bucket = app.try_active_bucket_mut().expect("active bucket exists");
            assert!(bucket.queued_messages.is_empty());
        }
        for i in 0..3 {
            let msg = &app.messages()[i];
            assert!(matches!(msg.role, MessageRole::User));
            assert!(!msg.queued, "drain must un-dim the bubble at idx {i}");
        }
        // Empty assistant message pushed for the new turn (idx 3).
        assert_eq!(app.messages().len(), 4);
        assert!(matches!(app.messages()[3].role, MessageRole::Assistant));
        assert!(matches!(app.status, AppStatus::Thinking));

        let prompt = rx.try_recv().expect("combined prompt should be dispatched");
        match prompt {
            forge_primitives::Command::PromptWithImages { session_id, text, .. } => {
                assert_eq!(session_id, "session-1");
                assert_eq!(text, "first\n\nsecond\n\nthird");
            }
            other => panic!("expected PromptWithImages, got: {other:?}"),
        }
    }

    #[test]
    fn drain_queued_messages_is_noop_when_empty() {
        let (mut app, mut rx) = app_with_connection();
        app.status = AppStatus::Ready;

        drain_queued_messages(&mut app);

        assert!(rx.try_recv().is_err());
        assert!(app.messages().is_empty());
    }

    #[test]
    fn drain_queued_messages_skips_when_not_ready() {
        let (mut app, mut rx) = app_with_connection();
        // Queue a message.
        app.status = AppStatus::Running;
        app.input_mut().set_text("queued");
        submit_input(&mut app);
        // Status still Running — drain must not fire.
        drain_queued_messages(&mut app);
        // Queue still populated.
        let bucket = app.try_active_bucket_mut().expect("active bucket exists");
        assert_eq!(bucket.queued_messages.len(), 1);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn submit_input_with_empty_text_is_noop() {
        let (mut app, mut rx) = app_with_connection();
        app.status = AppStatus::Running;
        app.input_mut().set_text("   ");

        submit_input(&mut app);

        // Nothing queued, nothing dispatched.
        let bucket = app.try_active_bucket_mut().expect("active bucket exists");
        assert!(bucket.queued_messages.is_empty());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn manual_cancel_promotes_existing_auto_cancel() {
        // request_cancel itself is still exercised by non-submit callers
        // (paste burst, error recovery); this test pins the promotion
        // semantics independent of submit.
        let (mut app, mut rx) = app_with_connection();
        app.status = AppStatus::Thinking;

        request_cancel(&mut app, CancelOrigin::AutoQueue).expect("auto cancel request");
        request_cancel(&mut app, CancelOrigin::Manual).expect("manual cancel request");

        assert_eq!(app.pending_cancel_origin(), Some(CancelOrigin::Manual));
        assert!(app.cancelled_turn_pending_hint());
        let envelope = rx.try_recv().expect("single cancel command should be sent");
        assert!(matches!(
            envelope,
            forge_primitives::Command::Cancel { session_id } if session_id == "session-1"
        ));
        assert!(rx.try_recv().is_err(), "manual promotion should not send second cancel");
    }

    #[test]
    fn dispatch_prompt_turn_without_session_id_leaves_state_unchanged() {
        let mut app = App::test_default();
        let _rx = app.install_testing_stub();
        app.status = AppStatus::Ready;

        dispatch_prompt_turn(&mut app, "hello".into());

        assert!(app.messages().is_empty());
        assert!(matches!(app.status, AppStatus::Ready));
    }

    #[test]
    fn config_slash_command_fires_regardless_of_busy() {
        // /config is a TUI-side meta-command and should work even when
        // a turn is in flight (issue #85: slash commands don't queue).
        let (mut app, mut rx) = app_with_connection();
        let dir = tempfile::tempdir().expect("tempdir");
        app.settings_home_override = Some(dir.path().to_path_buf());
        app.set_cwd_raw(dir.path().to_string_lossy().to_string());
        app.status = AppStatus::Running;
        app.input_mut().set_text("/config");

        submit_input(&mut app);

        // Slash handled → input cleared, view switched to Config.
        assert_eq!(app.active_view, ActiveView::Config);
        assert!(app.input().text().is_empty());
        // No cancel + no queue entry.
        let bucket = app.try_active_bucket_mut().expect("active bucket exists");
        assert!(bucket.queued_messages.is_empty());
        assert!(app.pending_cancel_origin().is_none());
        assert!(rx.try_recv().is_err());
    }
}
