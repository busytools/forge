use super::{App, AppStatus, CancelOrigin, ChatMessage, MessageBlock, MessageRole, TextBlock};
use crate::agent::model;
use crate::app::slash;

/// Handle Enter on the input editor.
///
/// Issue #85 (final shape 2026-05-13): forge holds no local queue.
/// Every submit dispatches `Command::Prompt` to claude immediately.
/// Claude's CLI maintains its own in-flight buffer (the `gO6`
/// queue in the bundled JS): when forge writes mid-turn, claude
/// merges the new prompt into the next user-message envelope going
/// to the model. The visual chat just gets one fresh user bubble
/// per submit regardless of whether a turn is in flight.
///
/// The previous dim → un-dim handshake was a pre-investigation
/// artefact. Live-capture proved claude does not echo
/// `queued_command` on stream-json stdout, so the only signal forge
/// would have had was the turn boundary anyway. Simpler to trust
/// claude's internal queue and skip the local bookkeeping entirely.
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

/// Push a fresh user bubble and dispatch `Command::Prompt`. Whether
/// claude is idle or mid-turn, the shape is the same — claude's
/// internal queue handles mid-turn delivery.
///
/// When claude is idle this also pushes the empty assistant
/// placeholder, finalises stale tool calls from prior turns, and
/// flips the status + lifecycle to indicate a new turn has started.
/// Mid-turn submits skip that bookkeeping because the in-flight turn
/// is still going.
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
    let user_blocks = vec![MessageBlock::Text(TextBlock::from_complete(&text))];
    app.push_message_tracked(ChatMessage::new(MessageRole::User, user_blocks, None));

    if !busy {
        // New turn: push the empty assistant placeholder (message.rs
        // renders the thinking indicator on the trailing-empty-
        // assistant) and flip status + lifecycle.
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
    fn submit_input_while_running_appends_bubble_without_changing_status() {
        // Mid-turn submit: bubble appears immediately, status stays
        // Running, no cancel, no assistant placeholder added, prompt
        // dispatches to claude which buffers it internally.
        let (mut app, mut rx) = app_with_connection();
        app.status = AppStatus::Running;
        app.input_mut().set_text("mid-turn prompt");

        submit_input(&mut app);

        assert!(app.input().text().is_empty());
        assert!(matches!(app.status, AppStatus::Running), "status untouched");
        assert_eq!(app.messages().len(), 1, "only the user bubble appended");
        let msg = &app.messages()[0];
        assert!(matches!(msg.role, MessageRole::User));
        assert!(app.pending_cancel_origin().is_none(), "no cancel fired");
        let prompt = rx.try_recv().expect("prompt dispatched immediately");
        assert!(matches!(
            prompt,
            forge_primitives::Command::PromptWithImages { session_id, text, .. }
                if session_id == "session-1" && text == "mid-turn prompt"
        ));
    }

    #[test]
    fn multiple_mid_turn_submits_each_append_a_bubble() {
        // Each mid-turn submit gets its own user bubble. Claude
        // batches them internally; the chat shows them in submit
        // order.
        let (mut app, mut rx) = app_with_connection();
        app.status = AppStatus::Running;
        app.input_mut().set_text("first");
        submit_input(&mut app);
        app.input_mut().set_text("second");
        submit_input(&mut app);

        assert_eq!(app.messages().len(), 2);
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err(), "exactly two prompts dispatched");
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
