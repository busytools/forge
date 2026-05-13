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

    // Issue #85 (revised 2026-05-13): send is non-destructive AND
    // always dispatches immediately. Claude CLI's internal queue
    // (the `queued_command` mechanism in the binary's `gO6` function)
    // handles in-flight bundling — when forge writes `Command::Prompt`
    // mid-turn, claude buffers it and packages it as a `queued_command`
    // content block on the next outbound user-message envelope going
    // to the model. The "popped" signal is the `QueuedCommand` block
    // echoing back on the wire (caught by
    // `events::sdk_message::handle_queued_command_echo`).
    //
    // While busy, forge pushes a DIMMED bubble locally so the user
    // sees their input acknowledged before claude consumes it, and
    // tracks the (text, message_idx) pair in
    // `UiSession.pending_echo_bubbles` so the wire-echo handler can
    // find + un-dim it. While idle, the existing fresh-turn path
    // applies (pushes user + assistant placeholder, sets Thinking).
    if is_turn_busy(app) {
        let images = std::mem::take(app.pending_images_mut());
        dispatch_pending_bubble(app, text, images);
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

/// Push a dimmed user bubble + dispatch `Command::Prompt` immediately.
/// Called when the user submits while a turn is in flight (#85).
///
/// Claude CLI's internal queue handles in-flight bundling: the
/// dispatched message becomes a `queued_command` content block on
/// claude's next outbound user-message envelope. When that block
/// echoes back on the wire (handled by
/// `events::sdk_message::handle_queued_command_echo`), forge un-dims
/// the bubble we pushed here. The (text, message_idx) pair is tracked
/// in `UiSession.pending_echo_bubbles` for the matching.
fn dispatch_pending_bubble(
    app: &mut App,
    text: String,
    attachments: Vec<crate::app::clipboard_image::ImageAttachment>,
) {
    let user_blocks = vec![MessageBlock::Text(TextBlock::from_complete(&text))];
    let pending_bubble = ChatMessage::new_queued(MessageRole::User, user_blocks);
    app.push_message_tracked(pending_bubble);
    app.enforce_history_retention_tracked();
    let chat_message_idx = app.messages().len().saturating_sub(1);

    let Some(key) = app.active_session_key.clone() else {
        return;
    };

    // Track (text, idx) for the wire-echo handler to un-dim later.
    // Clone the text once for the pending-echo entry; the original
    // moves into Command::Prompt below.
    let queue_depth_after = match app.session_mut(&key) {
        Some(session) => {
            session.pending_echo_bubbles.push_back((text.clone(), chat_message_idx));
            session.pending_echo_bubbles.len()
        }
        None => return,
    };

    app.active_viewport_mut().engage_auto_scroll();

    // Dispatch immediately — claude internally queues in-flight inputs
    // as `queued_command` attachments on the next outbound message.
    let tx = app.update_tx.clone();
    let Some(sid) = app.session_id() else {
        tracing::warn!(
            target: crate::logging::targets::APP_INPUT,
            event_name = "pending_bubble_no_session",
            message = "pending bubble pushed but no session id to dispatch against",
            outcome = "deferred",
        );
        return;
    };
    let session_id = sid.to_string();
    let input_chars = text.chars().count();
    match app.dispatch_command(|key| forge_workspace::Command::Prompt { key, text, attachments }) {
        Ok(()) => {
            tracing::debug!(
                target: crate::logging::targets::APP_INPUT,
                event_name = "pending_bubble_dispatched",
                message = "dispatched mid-turn prompt; awaiting queued_command echo",
                outcome = "success",
                session_id = %session_id,
                queue_depth_after,
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

/// Un-dim a pending bubble when its `queued_command` echo arrives on
/// the wire. Called by the content-block walker in
/// `events::sdk_message::walk_user_tool_results` when it encounters
/// a `ContentBlock::QueuedCommand`. Returns `true` if a matching
/// pending bubble was found and un-dimmed; `false` if no match (the
/// caller should push a fresh user bubble for the replay case).
pub(crate) fn un_dim_matching_pending(app: &mut App, prompt_text: &str) -> bool {
    let Some(key) = app.active_session_key.clone() else {
        return false;
    };
    let matched_idx = match app.session_mut(&key) {
        Some(session) => {
            let position =
                session.pending_echo_bubbles.iter().position(|(text, _)| text == prompt_text);
            position.and_then(|pos| session.pending_echo_bubbles.remove(pos).map(|(_, idx)| idx))
        }
        None => return false,
    };
    let Some(idx) = matched_idx else {
        return false;
    };
    if let Some(bucket) = app.try_active_bucket_mut()
        && let Some(msg) = bucket.messages.get_mut(idx)
        && matches!(msg.role, MessageRole::User)
    {
        msg.queued = false;
        msg.invalidate_render_cache();
        tracing::debug!(
            target: crate::logging::targets::APP_INPUT,
            event_name = "pending_bubble_un_dimmed",
            message = "matching queued_command echo arrived; un-dimmed bubble",
            outcome = "success",
            idx,
            prompt_chars = prompt_text.chars().count(),
        );
        return true;
    }
    false
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

        // Input cleared, dimmed bubble appended, prompt dispatched
        // immediately (claude will queue internally).
        assert!(app.input().text().is_empty());
        assert!(matches!(app.status, AppStatus::Running));
        assert_eq!(app.messages().len(), 1);
        let msg = &app.messages()[0];
        assert!(matches!(msg.role, MessageRole::User));
        assert!(msg.queued, "pending bubble must carry the queued flag");
        // pending_echo_bubbles tracks the (text, idx) for un-dim later.
        let bucket = app.try_active_bucket_mut().expect("active bucket exists");
        assert_eq!(bucket.pending_echo_bubbles.len(), 1);
        assert_eq!(bucket.pending_echo_bubbles[0].0, "queued prompt");
        assert_eq!(bucket.pending_echo_bubbles[0].1, 0);
        // No auto-submit-after-cancel set (vestigial path stays cold).
        assert!(!bucket.pending_auto_submit_after_cancel);
        assert!(app.pending_cancel_origin().is_none());
        // Prompt IS dispatched immediately — claude will internally
        // queue it as a `queued_command` on the next outbound message.
        let prompt = rx.try_recv().expect("prompt dispatched immediately");
        assert!(matches!(
            prompt,
            forge_primitives::Command::PromptWithImages { session_id, text, .. }
                if session_id == "session-1" && text == "queued prompt"
        ));
    }

    #[test]
    fn un_dim_matching_pending_finds_and_clears() {
        // Wire echo arrives for a pending dimmed bubble → bubble
        // un-dims, pending entry removed.
        let (mut app, _rx) = app_with_connection();
        app.status = AppStatus::Running;
        app.input_mut().set_text("hello");
        submit_input(&mut app);

        assert!(app.messages()[0].queued);
        let matched = un_dim_matching_pending(&mut app, "hello");

        assert!(matched, "exact-text match should succeed");
        assert!(!app.messages()[0].queued);
        let bucket = app.try_active_bucket_mut().expect("active bucket exists");
        assert!(bucket.pending_echo_bubbles.is_empty());
    }

    #[test]
    fn un_dim_matching_pending_no_match_returns_false() {
        // Echo for text that wasn't pending → returns false so the
        // caller can push a fresh bubble (replay path).
        let (mut app, _rx) = app_with_connection();
        app.status = AppStatus::Running;
        app.input_mut().set_text("hello");
        submit_input(&mut app);

        let matched = un_dim_matching_pending(&mut app, "different text");

        assert!(!matched);
        // Pending bubble stays dimmed.
        assert!(app.messages()[0].queued);
        let bucket = app.try_active_bucket_mut().expect("active bucket exists");
        assert_eq!(bucket.pending_echo_bubbles.len(), 1);
    }

    #[test]
    fn un_dim_matching_pending_fifo_disambiguates_duplicates() {
        // Two identical pending bubbles → first echo un-dims the
        // FIRST bubble (FIFO), second echo un-dims the second.
        let (mut app, _rx) = app_with_connection();
        app.status = AppStatus::Running;
        app.input_mut().set_text("dup");
        submit_input(&mut app);
        app.input_mut().set_text("dup");
        submit_input(&mut app);

        assert_eq!(app.messages().len(), 2);
        assert!(app.messages()[0].queued);
        assert!(app.messages()[1].queued);

        un_dim_matching_pending(&mut app, "dup");
        assert!(!app.messages()[0].queued, "first FIFO match un-dims idx 0");
        assert!(app.messages()[1].queued, "second pending stays dimmed");

        un_dim_matching_pending(&mut app, "dup");
        assert!(!app.messages()[1].queued, "second match un-dims idx 1");
    }

    #[test]
    fn submit_input_with_empty_text_is_noop() {
        let (mut app, mut rx) = app_with_connection();
        app.status = AppStatus::Running;
        app.input_mut().set_text("   ");

        submit_input(&mut app);

        let bucket = app.try_active_bucket_mut().expect("active bucket exists");
        assert!(bucket.pending_echo_bubbles.is_empty());
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
        assert!(bucket.pending_echo_bubbles.is_empty());
        assert!(app.pending_cancel_origin().is_none());
        assert!(rx.try_recv().is_err());
    }
}
