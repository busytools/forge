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

/// Drain queued messages. Issue #85.
///
/// Concatenates all queued texts with `\n\n` paragraph breaks, merges
/// attachments, fires a single `Command::Prompt`. The dimmed user
/// bubbles representing the queued messages get un-dimmed so the user
/// sees the model has now seen them. Chat history retains the
/// N-separate-bubble visual shape even though the wire sees one
/// combined prompt.
///
/// Dispatches based on app status:
/// - **Ready** (turn just ended): pushes a fresh assistant placeholder
///   and starts a new turn with the combined payload. Used by the
///   `TurnComplete` / cancelled-error drain hooks.
/// - **Thinking / Running** (turn still in flight): injects the
///   combined payload mid-turn AND pushes a fresh assistant
///   placeholder at the tail, re-binding the active assistant index
///   so claude's continued stream chunks land in chronological order
///   BELOW the un-dimmed user bubbles. The previously-streaming
///   assistant message freezes wherever it was. Used by the
///   `tool_result` and time-based-fallback hooks.
/// - Other statuses (Connecting, Error, CommandPending): no-op; queue
///   waits.
///
/// Empirical question to answer during testing: does claude in
/// stream-json subprocess mode accept user-message writes to stdin
/// mid-turn (i.e., during a Thinking/Running state)? Architect's
/// peer-via-MCP pattern + the user's interactive-CLI observation
/// suggest yes, but forge-sdk's transport may handle this
/// differently. If a mid-turn write triggers an error, the drain
/// gracefully reverts: the bubbles stay un-dimmed (claude got the
/// message), the dispatch error surfaces via `TurnError`.
pub(super) fn drain_queued_messages(app: &mut App) {
    if app.pending_cancel_origin().is_some() {
        return;
    }
    let Some(key) = app.active_session_key.clone() else {
        return;
    };

    // Collect the queue before mutating messages — avoids overlapping
    // borrows between `app.session_mut` and the active-bucket access
    // below.
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

    let drain_mode = match app.status {
        AppStatus::Ready => DrainMode::FreshTurn,
        AppStatus::Thinking | AppStatus::Running => DrainMode::MidTurnInjection,
        _ => {
            tracing::warn!(
                target: crate::logging::targets::APP_INPUT,
                event_name = "queue_drain_skipped_bad_status",
                message = "queue drain skipped — app status doesn't permit dispatch",
                outcome = "deferred",
                status = ?app.status,
                queue_size,
            );
            // Bubbles are already un-dimmed (we did that above) but
            // the texts haven't been dispatched. Re-enqueue is
            // awkward because chat_message_idx points at the same
            // bubbles we just un-dimmed. Acceptable v1 trade-off:
            // user-visible signal is "bubbles look sent" but model
            // didn't actually see them. The bad statuses
            // (Connecting/Error/CommandPending) are rare enough to
            // defer the fix; document in PR + #85.
            return;
        }
    };

    tracing::info!(
        target: crate::logging::targets::APP_INPUT,
        event_name = "queue_drained",
        message = "queued messages drained into a single combined prompt",
        outcome = "start",
        queue_size,
        drain_mode = ?drain_mode,
    );

    match drain_mode {
        DrainMode::FreshTurn => fire_combined_turn(app, combined_text, combined_attachments),
        DrainMode::MidTurnInjection => {
            inject_combined_into_running_turn(app, combined_text, combined_attachments);
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum DrainMode {
    /// Turn ended; start a fresh turn with the combined payload.
    FreshTurn,
    /// Turn still in flight; write the combined payload to claude
    /// mid-stream. Claude appends to current conversation context.
    MidTurnInjection,
}

/// Mid-turn drain: dispatch `Command::Prompt` while a turn is in
/// flight, AND push a fresh empty assistant placeholder so subsequent
/// stream chunks land in chronological order (below the queued user
/// bubbles, not into the previously-active assistant message above
/// them).
///
/// Why the placeholder is mandatory here: the streaming chunk handler
/// appends text to whichever message [`App::active_turn_assistant_message_idx`]
/// points at. If we left the index pointing at the old assistant
/// message that was streaming BEFORE the queued bubbles, claude's
/// continued response would land ABOVE the user bubbles even though
/// it's responding to them — confusing visual ordering. By pushing
/// a fresh placeholder AT THE TAIL (after the un-dimmed user
/// bubbles) and re-binding the active index, the next chunk lands
/// below, producing the correct chronological flow:
///
/// ```text
/// user1
/// assistant1   ← frozen at whatever it had streamed pre-injection
/// user2        ← was queued, just un-dimmed
/// user3        ← was queued, just un-dimmed
/// assistant2   ← fresh placeholder, continued response lands here
/// ```
///
/// Status + lifecycle stay as Thinking/Running — the turn never
/// ended; we just reparented the streaming target.
fn inject_combined_into_running_turn(
    app: &mut App,
    text: String,
    images: Vec<crate::app::clipboard_image::ImageAttachment>,
) {
    if !app.has_active_agent() {
        return;
    }
    let Some(sid) = app.session_id() else {
        return;
    };
    let input_chars = text.chars().count();
    let session_id = sid.to_string();

    // Reparent the streaming assistant target onto a fresh placeholder
    // at the tail — see doc comment above for why this is mandatory.
    app.push_message_tracked(ChatMessage::new(MessageRole::Assistant, Vec::new(), None));
    app.bind_active_turn_assistant_to_tail();
    app.enforce_history_retention_tracked();
    app.active_viewport_mut().engage_auto_scroll();

    let tx = app.update_tx.clone();
    let prompt_text = text;
    match app.dispatch_command(|key| forge_workspace::Command::Prompt {
        key,
        text: prompt_text,
        attachments: images,
    }) {
        Ok(()) => {
            tracing::info!(
                target: crate::logging::targets::APP_INPUT,
                event_name = "queue_injected_mid_turn",
                message = "queued prompt injected into running turn",
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
                message = "combined queued prompt dispatched (fresh turn)",
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

/// Time-based drain trigger called from the per-tick render loop.
/// Only fires when:
/// - app is mid-turn (`Thinking`/`Running`), AND
/// - the queue is non-empty, AND
/// - the OLDEST queued message has been waiting longer than
///   [`QUEUE_TIMER_FALLBACK_SECS`].
///
/// Heuristic fallback for tool-free turns (pure-text turns emit no
/// `tool_result` events, so the concrete `apply_tool_result_block`
/// drain hook never fires). 10s gives the user enough time to keep
/// typing if they're composing a longer follow-up while watching
/// the model stream.
pub(super) fn maybe_drain_queue_on_timer(app: &mut App) {
    if !matches!(app.status, AppStatus::Thinking | AppStatus::Running) {
        return;
    }
    let Some(key) = app.active_session_key.clone() else {
        return;
    };
    let oldest_age = match app.session_mut(&key) {
        Some(s) if !s.queued_messages.is_empty() => {
            s.queued_messages.front().map(|q| q.queued_at.elapsed())
        }
        _ => return,
    };
    let Some(age) = oldest_age else { return };
    if age < std::time::Duration::from_secs(QUEUE_TIMER_FALLBACK_SECS) {
        return;
    }
    drain_queued_messages(app);
}

/// Threshold for the time-based queue drain fallback (see
/// [`maybe_drain_queue_on_timer`]). 10s mirrors the rough "few
/// seconds" gap behaviour the user observed in the interactive
/// claude CLI. Conservative — bumping higher delays delivery; lower
/// risks pre-empting the user mid-compose.
const QUEUE_TIMER_FALLBACK_SECS: u64 = 10;

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
    fn drain_queued_messages_mid_turn_reparents_assistant_target() {
        // Issue #85 amendment: mid-turn drain (status=Running) injects
        // the prompt AND pushes a fresh assistant placeholder at the
        // tail, re-binding the active assistant index so subsequent
        // stream chunks land BELOW the un-dimmed user bubbles (in
        // chronological order). Without this, claude's continued
        // response streams into the previously-active assistant
        // message ABOVE the queued bubbles — broken ordering.
        let (mut app, mut rx) = app_with_connection();
        app.status = AppStatus::Running;
        app.input_mut().set_text("steering message");
        submit_input(&mut app);

        // One queued dimmed user bubble.
        assert_eq!(app.messages().len(), 1);
        assert!(app.messages()[0].queued);

        // Drain while still Running — mid-turn injection path.
        drain_queued_messages(&mut app);

        // Bubble un-dimmed; status UNCHANGED (still Running); a new
        // empty assistant placeholder pushed at the tail.
        assert!(!app.messages()[0].queued);
        assert_eq!(
            app.messages().len(),
            2,
            "mid-turn drain must push a fresh assistant placeholder at tail"
        );
        assert!(matches!(app.messages()[1].role, MessageRole::Assistant));
        assert!(app.messages()[1].blocks.is_empty(), "tail assistant placeholder must be empty");
        assert!(matches!(app.status, AppStatus::Running));

        let prompt = rx.try_recv().expect("mid-turn prompt should be dispatched");
        match prompt {
            forge_primitives::Command::PromptWithImages { session_id, text, .. } => {
                assert_eq!(session_id, "session-1");
                assert_eq!(text, "steering message");
            }
            other => panic!("expected PromptWithImages, got: {other:?}"),
        }
    }

    #[test]
    fn drain_queued_messages_skips_when_status_invalid() {
        // Statuses outside Ready/Thinking/Running (Connecting, Error,
        // CommandPending) skip drain — bubbles are un-dimmed but the
        // dispatch is deferred. Acceptable v1 trade-off.
        let (mut app, mut rx) = app_with_connection();
        app.status = AppStatus::Running;
        app.input_mut().set_text("queued");
        submit_input(&mut app);
        // Flip to a bad status before drain fires.
        app.status = AppStatus::Connecting;
        drain_queued_messages(&mut app);
        // No dispatch.
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn maybe_drain_queue_on_timer_noop_if_idle() {
        // Timer only fires mid-turn — idle sessions drain via the
        // TurnComplete path (already covered elsewhere).
        let (mut app, mut rx) = app_with_connection();
        app.status = AppStatus::Ready;
        // No queue + idle = pure no-op.
        maybe_drain_queue_on_timer(&mut app);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn maybe_drain_queue_on_timer_noop_within_threshold() {
        // Queue a message while busy; timer should NOT fire
        // immediately (within the 10s threshold).
        let (mut app, mut rx) = app_with_connection();
        app.status = AppStatus::Running;
        app.input_mut().set_text("recent");
        submit_input(&mut app);
        // Immediate timer check — queued_at is "just now".
        maybe_drain_queue_on_timer(&mut app);
        // Queue still populated, nothing dispatched.
        let bucket = app.try_active_bucket_mut().expect("active bucket exists");
        assert_eq!(bucket.queued_messages.len(), 1);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn maybe_drain_queue_on_timer_fires_after_threshold() {
        // Synthesise an "old" queued_at by writing directly to the
        // bucket, then call maybe_drain. The timer-based drain
        // should fire because the oldest message is past threshold.
        let (mut app, mut rx) = app_with_connection();
        app.status = AppStatus::Running;
        app.input_mut().set_text("steering");
        submit_input(&mut app);

        // Backdate the queued_at by 15s.
        {
            let bucket = app.try_active_bucket_mut().expect("active bucket exists");
            let entry = bucket.queued_messages.front_mut().expect("queued entry");
            entry.queued_at = Instant::now()
                .checked_sub(std::time::Duration::from_secs(15))
                .expect("system clock far enough past epoch to backdate 15s");
        }

        maybe_drain_queue_on_timer(&mut app);

        // Drain fired — queue empty, bubble un-dimmed, prompt dispatched.
        let bucket = app.try_active_bucket_mut().expect("active bucket exists");
        assert!(bucket.queued_messages.is_empty());
        assert!(!app.messages()[0].queued);
        let prompt = rx.try_recv().expect("timer-triggered drain should dispatch");
        match prompt {
            forge_primitives::Command::PromptWithImages { text, .. } => {
                assert_eq!(text, "steering");
            }
            other => panic!("expected PromptWithImages, got: {other:?}"),
        }
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
