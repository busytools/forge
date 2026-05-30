use super::{App, AppStatus, ChatMessage, MessageBlock, MessageRole, TextBlock};
use crate::agent::model;
use crate::app::slash;

/// Handle Enter on the input editor. Dispatches `Command::Prompt`
/// immediately and pushes both a user bubble and a fresh empty
/// assistant placeholder at the tail, reparenting the active turn
/// onto the new placeholder so claude's continuing wire tokens land
/// below the user's submission rather than pinning to the prior
/// assistant bubble. Mid-turn queuing happens inside the CLI; the
/// resume path reconstructs queued submits from JSONL `attachment`
/// rows via `forge_agent::userdata::catalog::scan`.
pub(super) fn submit_input(app: &mut App) {
    if matches!(app.status, AppStatus::Connecting | AppStatus::CommandPending | AppStatus::Error) {
        return;
    }

    // Dismiss any open autocomplete dropdown.
    *app.mention_mut() = None;
    *app.slash_mut() = None;
    *app.subagent_mut() = None;

    let text = app.input().text();
    if text.trim().is_empty() {
        return;
    }
    app.set_prompt_suggestion(None);

    // Slash commands are TUI-side meta-commands (`/config`, `/mcp`,
    // `/status`, …) that consume the input without dispatching to
    // claude. `try_handle_submit` returns true when it consumed the
    // input; `false` falls through to the regular prompt path
    // (e.g. `/compact` passes through as a real user prompt).
    if slash::try_handle_submit(app, &text) {
        app.input_mut().clear();
        app.sync_help_open_with_input();
        return;
    }

    app.input_mut().clear();
    app.sync_help_open_with_input();
    dispatch_prompt(app, text);
}

/// Submit a pre-built diff-review markdown bundle as if the user
/// typed it into the chat input. Used by the diff overlay's Esc
/// path (and banner ✕ click) to deliver pending comments to claude
/// in one shot.
///
/// This is a thin wrapper around `dispatch_prompt`  -  the bundle is
/// already formatted markdown, so we skip the slash-command try
/// path and go straight to the agent dispatch. A real chat-input
/// path would have to handle slash commands; here we know the text
/// is a review bundle and never a slash command.
pub(super) fn dispatch_diff_comment_bundle(app: &mut App, text: String) {
    if text.trim().is_empty() {
        return;
    }
    dispatch_prompt(app, text);
}

/// True when a turn is currently in flight against claude. Used to
/// decide whether `dispatch_prompt` should also push the assistant
/// placeholder + flip the lifecycle to Running (idle path), or just
/// append the user bubble and let the in-flight turn keep going
/// (mid-turn path).
fn is_turn_busy(app: &App) -> bool {
    matches!(app.status, AppStatus::Thinking | AppStatus::Running)
        || app.pending_cancel()
        || app.is_compacting()
}

/// Cancel the in-flight turn. The only routine caller is Escape;
/// submit dispatches immediately and claude internally buffers
/// mid-turn writes, so there are no auto-induced cancels.
pub(super) fn request_cancel(app: &mut App) -> Result<(), String> {
    if !matches!(app.status, AppStatus::Thinking | AppStatus::Running) {
        return Ok(());
    }
    if app.pending_cancel() {
        // Already cancelling  -  second Escape is a no-op.
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
    app.set_pending_cancel(true);
    app.set_cancelled_turn_pending_hint(true);
    let session_key = forge_workspace::SessionKey::from_session_id(session_id.clone());
    let _ = app.update_tx.send(forge_workspace::SessionUpdate::TurnCancelled { key: session_key });
    tracing::info!(
        target: crate::logging::targets::APP_INPUT,
        event_name = "turn_cancel_requested",
        message = "turn cancel requested",
        outcome = "success",
        session_id = %session_id,
    );
    Ok(())
}

/// Push a fresh user bubble + an empty assistant placeholder and
/// dispatch `Command::Prompt`. Always pushes both  -  idle or mid-turn.
///
/// Mid-turn shape: every submit reparents
/// `active_turn_assistant_idx` onto a fresh assistant placeholder
/// at the tail. Claude's continuing wire tokens then land in that
/// new placeholder, below the user's new bubble  -  append-only
/// geometry regardless of whether a turn is already in flight. The
/// CLI queues the mid-turn prompt and folds it into the next
/// user→model envelope (typically the tool_result cycle), so the
/// response to the queued prompt streams into the new asst bubble
/// alongside whatever turn-1 continuation arrives.
///
/// Pure-text turns with no tool cycle leave a small artifact: any
/// turn-1 tokens that arrive between the submit and `result` land
/// in the new placeholder rather than the prior asst bubble. If
/// claude emits zero tokens between submit and `result` the
/// placeholder stays empty and `remove_empty_tail_assistant` strips
/// it on TurnComplete. Otherwise the stub remains visible  -  a known
/// cost of the always-reparent design.
fn dispatch_prompt(app: &mut App, text: String) {
    let busy = is_turn_busy(app);

    if !busy {
        // Pre-flight for a new turn: stop any leftover in-progress
        // tools so their spinners don't bleed into the new turn.
        // Runs even when no agent is present so stale UI state
        // doesn't linger.
        let _ = app.finalize_in_progress_tool_calls(model::ToolCallStatus::Failed);
    }

    if !app.has_active_agent() {
        // Pre-Connect: leave state untouched. The user can resubmit
        // once the session connects.
        tracing::debug!(
            target: crate::logging::targets::APP_INPUT,
            event_name = "prompt_dispatch_deferred_no_agent",
            message = "no active agent yet — submit ignored",
            outcome = "deferred",
        );
        return;
    }
    let Some(sid) = app.session_id() else {
        return;
    };

    let images = std::mem::take(app.pending_images_mut());

    // If the tail is an empty assistant placeholder (the one a
    // previous submit pushed that claude never had a chance to
    // fill), drop it before appending. Without this, rapid mid-turn
    // submits accumulate visible empty asst bubbles between user
    // messages.
    if let Some(tail_idx) = app.messages().len().checked_sub(1) {
        let tail_is_empty_asst = app
            .messages()
            .get(tail_idx)
            .is_some_and(|msg| matches!(msg.role, MessageRole::Assistant) && msg.blocks.is_empty());
        if tail_is_empty_asst {
            let _ = app.remove_message_tracked(tail_idx);
        }
    }

    // A submit overrides any in-flight cancel intent  -  the new prompt
    // IS the user's next move, so the "Cancelling current turn..."
    // hint should clear immediately. Without this, a cancel followed
    // by a fast submit leaves the hint pinned on screen forever
    // because the CLI fuses the new prompt with the in-flight turn
    // and never emits a Result for the interrupted state that would
    // otherwise clear the flag.
    if app.pending_cancel() {
        app.set_pending_cancel(false);
        app.set_cancelled_turn_pending_hint(false);
    }

    let user_blocks = vec![MessageBlock::Text(TextBlock::from_complete(&text))];
    app.push_message_tracked(ChatMessage::new(MessageRole::User, user_blocks, None));

    // Always push an empty assistant placeholder + reparent the
    // active turn assistant onto it. Mid-turn submits get this
    // treatment too  -  that's the entire point of the new shape, so
    // claude's continuing tokens land below the new user bubble
    // instead of above it.
    app.push_message_tracked(ChatMessage::new(MessageRole::Assistant, Vec::new(), None));
    app.bind_active_turn_assistant_to_tail();
    app.status = AppStatus::Thinking;
    if let Some(key) = app.active_session_key.clone() {
        crate::app::events::set_bucket_lifecycle_state(
            app,
            &key,
            crate::app::session::SessionLifecycleState::Running,
        );
    }
    app.enforce_history_retention_tracked();
    app.active_viewport_mut().engage_auto_scroll();

    let session_id = sid.to_string();
    let input_chars = text.chars().count();
    let tx = app.update_tx.clone();
    match app.dispatch_command(|key| forge_workspace::Command::Prompt {
        key,
        text,
        attachments: images,
    }) {
        Ok(()) => {
            if !busy {
                // Mid-turn submits ride the in-flight turn's context
                // updates  -  only refresh on the idle → new-turn path.
                crate::app::session_runtime::request_context_usage_refresh(app);
            }
            tracing::info!(
                target: crate::logging::targets::APP_INPUT,
                event_name = "prompt_dispatched",
                message = "prompt dispatched to the bridge",
                outcome = "success",
                session_id = %session_id,
                input_chars,
                mid_turn = busy,
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
    -> (App, tokio::sync::mpsc::UnboundedReceiver<forge_primitives::AgentCommand>) {
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
        // user bubble + empty assistant placeholder
        assert_eq!(app.messages().len(), 2);
        let prompt = rx.try_recv().expect("prompt command should be sent");
        assert!(matches!(
            prompt,
            forge_primitives::AgentCommand::PromptWithImages { session_id, .. } if session_id == "session-1"
        ));
    }

    #[test]
    fn submit_input_while_running_appends_bubble_and_reparents_active_idx() {
        // Mid-turn submit: user bubble appears immediately AND a
        // fresh empty assistant placeholder is pushed right after
        // it, with `active_turn_assistant_idx`
        // reparented onto the new placeholder. Status flips back to
        // Thinking so the spinner attaches to the new placeholder
        // while claude's continuing tokens stream into it. The prompt
        // still dispatches immediately  -  claude's internal queue folds
        // it into the next user→model envelope.
        let (mut app, mut rx) = app_with_connection();
        app.status = AppStatus::Running;
        app.input_mut().set_text("mid-turn prompt");

        submit_input(&mut app);

        assert!(app.input().text().is_empty());
        assert!(matches!(app.status, AppStatus::Thinking), "status flipped back to Thinking");
        // user bubble + freshly-pushed empty asst placeholder
        assert_eq!(app.messages().len(), 2, "user bubble + asst placeholder");
        let user_msg = &app.messages()[0];
        let asst_msg = &app.messages()[1];
        assert!(matches!(user_msg.role, MessageRole::User));
        assert!(matches!(asst_msg.role, MessageRole::Assistant));
        assert!(asst_msg.blocks.is_empty(), "asst placeholder is empty until claude streams");
        assert_eq!(
            app.active_turn_assistant_message_idx(),
            Some(1),
            "active asst idx reparented onto the new placeholder at tail",
        );
        assert!(!app.pending_cancel(), "no cancel fired");
        let prompt = rx.try_recv().expect("prompt dispatched immediately");
        assert!(matches!(
            prompt,
            forge_primitives::AgentCommand::PromptWithImages { session_id, text, .. }
                if session_id == "session-1" && text == "mid-turn prompt"
        ));
    }

    #[test]
    fn multiple_mid_turn_submits_strip_empty_placeholder_between_them() {
        // Each mid-turn submit pushes its own asst placeholder, but
        // if the next submit fires before claude streams any tokens
        // into the previous placeholder, that empty placeholder is
        // dropped on the next submit. Net effect: rapid-fire user
        // bubbles sit adjacent in the scrollback, with exactly ONE
        // empty placeholder at the tail (the latest one, awaiting
        // claude's next token).
        let (mut app, mut rx) = app_with_connection();
        app.status = AppStatus::Running;
        app.input_mut().set_text("first");
        submit_input(&mut app);
        app.input_mut().set_text("second");
        submit_input(&mut app);

        // [user-first, user-second, asst empty]  -  the empty
        // placeholder from the first submit got dropped.
        assert_eq!(app.messages().len(), 3, "stripped the in-between empty placeholder");
        assert!(matches!(app.messages()[0].role, MessageRole::User));
        assert!(matches!(app.messages()[1].role, MessageRole::User));
        assert!(matches!(app.messages()[2].role, MessageRole::Assistant));
        assert!(
            app.messages()[2].blocks.is_empty(),
            "tail asst placeholder still empty until claude streams",
        );
        assert_eq!(
            app.active_turn_assistant_message_idx(),
            Some(2),
            "active asst idx tracks the surviving tail placeholder",
        );
        let first = rx.try_recv().expect("first prompt dispatched");
        assert!(matches!(
            first,
            forge_primitives::AgentCommand::PromptWithImages { session_id, text, .. }
                if session_id == "session-1" && text == "first"
        ));
        let second = rx.try_recv().expect("second prompt dispatched");
        assert!(matches!(
            second,
            forge_primitives::AgentCommand::PromptWithImages { session_id, text, .. }
                if session_id == "session-1" && text == "second"
        ));
        assert!(rx.try_recv().is_err(), "exactly two prompts dispatched");
    }

    #[test]
    fn mid_turn_submit_keeps_non_empty_prior_placeholder() {
        // Defensive: if the tail placeholder has SOME content (claude
        // streamed a few tokens between submits), it must NOT be
        // dropped  -  that would lose claude's content. The new submit
        // appends below it normally.
        let (mut app, _rx) = app_with_connection();
        app.status = AppStatus::Running;
        // Seed a prior turn + non-empty asst at the tail.
        app.active_messages_mut().push(ChatMessage::new(
            MessageRole::User,
            vec![MessageBlock::Text(TextBlock::from_complete("earlier"))],
            None,
        ));
        app.active_messages_mut().push(ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::Text(TextBlock::from_complete("partial..."))],
            None,
        ));
        app.bind_active_turn_assistant_to_tail();
        app.input_mut().set_text("follow-up");
        submit_input(&mut app);

        // [user-earlier, asst-partial (KEPT), user-follow-up, asst empty]
        assert_eq!(app.messages().len(), 4);
        assert!(matches!(app.messages()[1].role, MessageRole::Assistant));
        assert!(!app.messages()[1].blocks.is_empty(), "non-empty asst preserved");
        assert!(matches!(app.messages()[2].role, MessageRole::User));
        assert!(matches!(app.messages()[3].role, MessageRole::Assistant));
        assert!(app.messages()[3].blocks.is_empty(), "tail asst is the new empty placeholder");
    }

    #[test]
    fn submit_input_with_empty_text_is_noop() {
        let (mut app, mut rx) = app_with_connection();
        app.status = AppStatus::Running;
        app.input_mut().set_text("   ");

        submit_input(&mut app);

        assert!(app.messages().is_empty());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn repeated_manual_cancel_is_idempotent() {
        let (mut app, mut rx) = app_with_connection();
        app.status = AppStatus::Thinking;

        request_cancel(&mut app).expect("first manual cancel");
        request_cancel(&mut app).expect("second manual cancel");

        assert!(app.pending_cancel());
        let envelope = rx.try_recv().expect("single cancel command should be sent");
        assert!(matches!(
            envelope,
            forge_primitives::AgentCommand::Cancel { session_id } if session_id == "session-1"
        ));
        assert!(rx.try_recv().is_err(), "second cancel should not re-dispatch");
    }

    #[test]
    fn dispatch_prompt_without_session_id_leaves_state_unchanged() {
        let mut app = App::test_default();
        let _rx = app.install_testing_stub();
        app.status = AppStatus::Ready;

        dispatch_prompt(&mut app, "hello".into());

        // No agent → submit is ignored: no bubble pushed, no status
        // flip, no dispatch.
        assert!(app.messages().is_empty());
        assert!(matches!(app.status, AppStatus::Ready));
    }

    #[test]
    fn plugins_slash_command_fires_regardless_of_busy() {
        // /plugins is a TUI-side view switch and should work even when
        // a turn is in flight (slash commands don't queue).
        let (mut app, mut rx) = app_with_connection();
        let dir = tempfile::tempdir().expect("tempdir");
        app.settings_home_override = Some(dir.path().to_path_buf());
        app.set_cwd_raw(dir.path().to_string_lossy().to_string());
        app.status = AppStatus::Running;
        app.input_mut().set_text("/plugins");

        submit_input(&mut app);

        assert_eq!(app.active_view, ActiveView::Plugins);
        assert!(app.input().text().is_empty());
        assert!(!app.pending_cancel());
        assert!(rx.try_recv().is_err());
    }
}
