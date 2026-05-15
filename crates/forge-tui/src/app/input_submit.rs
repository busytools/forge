use super::{App, AppStatus, CancelOrigin, ChatMessage, MessageBlock, MessageRole, TextBlock};
use crate::agent::model;
use crate::app::slash;

/// Handle Enter on the input editor.
///
/// Issue #85 (revised 2026-05-14): forge holds no local queue, and
/// every submit dispatches `Command::Prompt` to claude immediately.
/// The mid-turn fix from this round is in the receive-path
/// placement, not the dispatch path: every submit (idle OR mid-turn)
/// pushes both a user bubble AND a fresh empty assistant placeholder
/// at the tail, reparenting `active_turn_assistant_idx` onto that
/// new placeholder. Claude's continuing wire tokens then land in
/// the new bubble — below the user's new message — instead of
/// pinning in the prior turn's assistant bubble above it.
///
/// Claude's CLI internally queues the mid-turn prompt (the bundled
/// JS `queuedCommands` array, flushed into the next user→model
/// envelope at the next tool cycle or turn boundary). The model's
/// reply then covers both turn-1's continuation and the queued
/// prompt's answer in a single assistant message; forge folds the
/// whole thing into the reparented placeholder so geometry stays
/// append-only.
///
/// Pure-text turns produce a small artifact: any turn-1 tokens that
/// arrive between the user's submit and claude's `result` event
/// land in the new placeholder rather than the prior asst bubble.
/// If claude emits nothing in that window, `remove_empty_tail_assistant`
/// strips the empty stub on TurnComplete. If it emits a few
/// trailing tokens, they appear as a thin stub below the new user
/// bubble — known cost of the always-reparent design.
///
/// The previous dim → un-dim handshake was a pre-investigation
/// artefact already removed in PR #117. This revision keeps the
/// no-dim outcome and fixes the ordering issue PR #117 left behind.
///
/// Session resume reconstructs queued submits from JSONL
/// `type:"attachment"` rows via the catalog/scan layer in
/// `forge-agent` (see `userdata::catalog::scan`).
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
/// This is a thin wrapper around `dispatch_prompt` — the bundle is
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
        || app.pending_cancel_origin().is_some()
        || app.is_compacting()
}

/// Cancel the in-flight turn. The only routine caller is Escape;
/// submit dispatches immediately and claude internally buffers
/// mid-turn writes, so there are no auto-induced cancels.
pub(super) fn request_cancel(app: &mut App, origin: CancelOrigin) -> Result<(), String> {
    if !matches!(app.status, AppStatus::Thinking | AppStatus::Running) {
        return Ok(());
    }
    if app.pending_cancel_origin().is_some() {
        // Already cancelling — second Escape is a no-op.
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

/// Push a fresh user bubble + an empty assistant placeholder and
/// dispatch `Command::Prompt`. Always pushes both — idle or mid-turn.
///
/// The mid-turn shape (post 2026-05-14): every submit reparents
/// `active_turn_assistant_idx` onto a fresh assistant placeholder at
/// the tail. Claude's continuing wire tokens then land in that new
/// placeholder, below the user's new bubble — append-only geometry
/// regardless of whether a turn is already in flight. Claude's
/// internal `gO6` queue folds the mid-turn prompt into the next
/// user→model envelope (typically the tool_result cycle), so the
/// response to the queued prompt naturally streams into the new
/// asst bubble alongside whatever turn-1 continuation arrives.
///
/// Pure-text turns with no tool cycle leave a small artifact: any
/// turn-1 tokens that arrive between the submit and `result` land
/// in the new placeholder rather than the prior asst bubble. If
/// claude emits zero tokens between submit and `result` the
/// placeholder stays empty and `remove_empty_tail_assistant` strips
/// it on TurnComplete. Otherwise the stub remains visible — a known
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
    // messages — exactly the artifact the screenshot from 2026-05-14
    // showed: `restarted lets test`, empty asst, `1`, empty asst,
    // `2`, …
    if let Some(tail_idx) = app.messages().len().checked_sub(1) {
        let tail_is_empty_asst = app
            .messages()
            .get(tail_idx)
            .is_some_and(|msg| matches!(msg.role, MessageRole::Assistant) && msg.blocks.is_empty());
        if tail_is_empty_asst {
            let _ = app.remove_message_tracked(tail_idx);
        }
    }

    let user_blocks = vec![MessageBlock::Text(TextBlock::from_complete(&text))];
    app.push_message_tracked(ChatMessage::new(MessageRole::User, user_blocks, None));

    // Always push an empty assistant placeholder + reparent the
    // active turn assistant onto it. Mid-turn submits get this
    // treatment too — that's the entire point of the new shape, so
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
                // updates — only refresh on the idle → new-turn path.
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
        // user bubble + empty assistant placeholder
        assert_eq!(app.messages().len(), 2);
        let prompt = rx.try_recv().expect("prompt command should be sent");
        assert!(matches!(
            prompt,
            forge_primitives::Command::PromptWithImages { session_id, .. } if session_id == "session-1"
        ));
    }

    #[test]
    fn submit_input_while_running_appends_bubble_and_reparents_active_idx() {
        // Mid-turn submit (post 2026-05-14): user bubble appears
        // immediately AND a fresh empty assistant placeholder is
        // pushed right after it, with `active_turn_assistant_idx`
        // reparented onto the new placeholder. Status flips back to
        // Thinking so the spinner attaches to the new placeholder
        // while claude's continuing tokens stream into it. The prompt
        // still dispatches immediately — claude's internal queue folds
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
        assert!(app.pending_cancel_origin().is_none(), "no cancel fired");
        let prompt = rx.try_recv().expect("prompt dispatched immediately");
        assert!(matches!(
            prompt,
            forge_primitives::Command::PromptWithImages { session_id, text, .. }
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

        // [user-first, user-second, asst empty] — the empty
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
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err(), "exactly two prompts dispatched");
    }

    #[test]
    fn mid_turn_submit_keeps_non_empty_prior_placeholder() {
        // Defensive: if the tail placeholder has SOME content (claude
        // streamed a few tokens between submits), it must NOT be
        // dropped — that would lose claude's content. The new submit
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

        request_cancel(&mut app, CancelOrigin::Manual).expect("first manual cancel");
        request_cancel(&mut app, CancelOrigin::Manual).expect("second manual cancel");

        assert_eq!(app.pending_cancel_origin(), Some(CancelOrigin::Manual));
        let envelope = rx.try_recv().expect("single cancel command should be sent");
        assert!(matches!(
            envelope,
            forge_primitives::Command::Cancel { session_id } if session_id == "session-1"
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
    fn config_slash_command_fires_regardless_of_busy() {
        // /config is a TUI-side meta-command and should work even when
        // a turn is in flight (slash commands don't queue).
        let (mut app, mut rx) = app_with_connection();
        let dir = tempfile::tempdir().expect("tempdir");
        app.settings_home_override = Some(dir.path().to_path_buf());
        app.set_cwd_raw(dir.path().to_string_lossy().to_string());
        app.status = AppStatus::Running;
        app.input_mut().set_text("/config");

        submit_input(&mut app);

        assert_eq!(app.active_view, ActiveView::Config);
        assert!(app.input().text().is_empty());
        assert!(app.pending_cancel_origin().is_none());
        assert!(rx.try_recv().is_err());
    }
}
