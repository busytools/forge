use super::{App, AppStatus, CancelOrigin, ChatMessage, MessageBlock, MessageRole, TextBlock};
use crate::agent::model;
use crate::app::slash;

pub(super) fn submit_input(app: &mut App) {
    if matches!(app.status, AppStatus::Connecting | AppStatus::CommandPending | AppStatus::Error) {
        return;
    }

    // Dismiss any open mention dropdown
    app.mention = None;
    app.slash = None;
    app.subagent = None;

    // No connection yet - can't submit
    let text = app.input().text();
    if text.trim().is_empty() {
        return;
    }
    app.set_prompt_suggestion(None);

    // While a turn is active, keep the current draft text in the input and
    // only request cancellation of the running turn.
    if is_turn_busy(app) {
        match request_cancel(app, CancelOrigin::AutoQueue) {
            Ok(()) => {
                app.pending_auto_submit_after_cancel = true;
                tracing::debug!(
                    target: crate::logging::targets::APP_INPUT,
                    event_name = "submit_deferred_for_cancel",
                    message = "input submit deferred until the active turn is cancelled",
                    outcome = "start",
                );
            }
            Err(message) => {
                app.pending_auto_submit_after_cancel = false;
                tracing::error!(
                    target: crate::logging::targets::APP_INPUT,
                    event_name = "cancel_request_failed",
                    message = "failed to request cancel for deferred submit",
                    outcome = "failure",
                    error_message = %message,
                );
            }
        }
        return;
    }

    app.pending_auto_submit_after_cancel = false;
    app.input_mut().clear();
    app.sync_help_open_with_input();
    dispatch_submission(app, text);
}

fn is_turn_busy(app: &App) -> bool {
    matches!(app.status, AppStatus::Thinking | AppStatus::Running)
        || app.pending_cancel_origin().is_some()
        || app.is_compacting()
}

pub(super) fn request_cancel(app: &mut App, origin: CancelOrigin) -> Result<(), String> {
    if matches!(origin, CancelOrigin::Manual) {
        app.pending_auto_submit_after_cancel = false;
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

pub(super) fn maybe_auto_submit_after_cancel(app: &mut App) {
    if !app.pending_auto_submit_after_cancel {
        return;
    }
    if !matches!(app.status, AppStatus::Ready) || app.pending_cancel_origin().is_some() {
        return;
    }
    if app.input().text().trim().is_empty() {
        app.pending_auto_submit_after_cancel = false;
        return;
    }
    app.pending_auto_submit_after_cancel = false;
    submit_input(app);
}

fn dispatch_submission(app: &mut App, text: String) {
    if slash::try_handle_submit(app, &text) {
        return;
    }
    dispatch_prompt_turn(app, text);
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
    let images = std::mem::take(&mut app.pending_images);

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
        crate::app::events::set_lifecycle_state_in_workspace(
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
    fn submit_input_while_running_keeps_input_and_requests_cancel() {
        let (mut app, mut rx) = app_with_connection();
        app.status = AppStatus::Running;
        app.input_mut().set_text("queued prompt");

        submit_input(&mut app);

        assert_eq!(app.input().text(), "queued prompt");
        assert_eq!(app.pending_cancel_origin(), Some(CancelOrigin::AutoQueue));
        assert!(app.pending_auto_submit_after_cancel);
        assert!(matches!(app.status, AppStatus::Running));
        assert!(app.messages().is_empty());
        let envelope = rx.try_recv().expect("cancel command should be sent");
        assert!(matches!(
            envelope,
            forge_primitives::Command::Cancel { session_id } if session_id == "session-1"
        ));
    }

    #[test]
    fn manual_cancel_promotes_existing_auto_cancel() {
        let (mut app, mut rx) = app_with_connection();
        app.status = AppStatus::Thinking;
        app.pending_auto_submit_after_cancel = true;

        request_cancel(&mut app, CancelOrigin::AutoQueue).expect("auto cancel request");
        request_cancel(&mut app, CancelOrigin::Manual).expect("manual cancel request");

        assert_eq!(app.pending_cancel_origin(), Some(CancelOrigin::Manual));
        assert!(app.cancelled_turn_pending_hint());
        assert!(!app.pending_auto_submit_after_cancel);
        let envelope = rx.try_recv().expect("single cancel command should be sent");
        assert!(matches!(
            envelope,
            forge_primitives::Command::Cancel { session_id } if session_id == "session-1"
        ));
        assert!(rx.try_recv().is_err(), "manual promotion should not send second cancel");
    }

    #[test]
    fn manual_cancel_prevents_later_auto_submit_after_cancel() {
        let (mut app, mut rx) = app_with_connection();
        app.status = AppStatus::Running;
        app.input_mut().set_text("draft");

        submit_input(&mut app);
        assert_eq!(app.pending_cancel_origin(), Some(CancelOrigin::AutoQueue));
        assert!(app.pending_auto_submit_after_cancel);
        let cancel = rx.try_recv().expect("cancel command should be sent");
        assert!(matches!(
            cancel, forge_primitives::Command::Cancel { session_id } if session_id == "session-1"
        ));

        request_cancel(&mut app, CancelOrigin::Manual).expect("manual cancel request");
        assert_eq!(app.pending_cancel_origin(), Some(CancelOrigin::Manual));
        assert!(!app.pending_auto_submit_after_cancel);

        app.status = AppStatus::Ready;
        app.set_pending_cancel_origin(None);
        maybe_auto_submit_after_cancel(&mut app);

        assert_eq!(app.input().text(), "draft");
        assert!(matches!(app.status, AppStatus::Ready));
        assert!(app.messages().is_empty());
        assert!(rx.try_recv().is_err(), "manual cancel should suppress queued prompt submit");
    }

    #[test]
    fn submit_input_with_pending_cancel_keeps_input_and_sends_no_second_cancel() {
        let (mut app, mut rx) = app_with_connection();
        app.status = AppStatus::Running;
        app.input_mut().set_text("draft");

        submit_input(&mut app);
        submit_input(&mut app);

        assert_eq!(app.input().text(), "draft");
        assert_eq!(app.pending_cancel_origin(), Some(CancelOrigin::AutoQueue));
        assert!(app.pending_auto_submit_after_cancel);
        let envelope = rx.try_recv().expect("first cancel command should be sent");
        assert!(matches!(
            envelope, forge_primitives::Command::Cancel { session_id } if session_id == "session-1"
        ));
        assert!(rx.try_recv().is_err(), "second submit should not send extra cancel");
    }

    #[test]
    fn auto_submit_dispatches_draft_once_ready() {
        let (mut app, mut rx) = app_with_connection();
        app.status = AppStatus::Running;
        app.input_mut().set_text("send after cancel");

        submit_input(&mut app);
        assert!(app.pending_auto_submit_after_cancel);
        let cancel = rx.try_recv().expect("cancel command should be sent");
        assert!(matches!(
            cancel, forge_primitives::Command::Cancel { session_id } if session_id == "session-1"
        ));

        app.status = AppStatus::Ready;
        app.set_pending_cancel_origin(None);
        maybe_auto_submit_after_cancel(&mut app);

        assert!(!app.pending_auto_submit_after_cancel);
        assert!(app.input().text().is_empty());
        assert!(matches!(app.status, AppStatus::Thinking));
        assert_eq!(app.messages().len(), 2);
        let prompt = rx.try_recv().expect("prompt command should be sent");
        assert!(matches!(
            prompt,
            forge_primitives::Command::PromptWithImages { session_id, .. } if session_id == "session-1"
        ));
    }

    #[test]
    fn auto_submit_opens_config_only_after_cancel_finishes() {
        let (mut app, mut rx) = app_with_connection();
        let dir = tempfile::tempdir().expect("tempdir");
        app.settings_home_override = Some(dir.path().to_path_buf());
        app.set_cwd_raw(dir.path().to_string_lossy().to_string());
        app.status = AppStatus::Running;
        app.input_mut().set_text("/config");

        submit_input(&mut app);

        assert_eq!(app.active_view, ActiveView::Chat);
        assert_eq!(app.input().text(), "/config");
        assert_eq!(app.pending_cancel_origin(), Some(CancelOrigin::AutoQueue));
        assert!(app.pending_auto_submit_after_cancel);
        let cancel = rx.try_recv().expect("cancel command should be sent");
        assert!(matches!(
            cancel, forge_primitives::Command::Cancel { session_id } if session_id == "session-1"
        ));

        app.status = AppStatus::Ready;
        app.set_pending_cancel_origin(None);
        maybe_auto_submit_after_cancel(&mut app);

        assert!(!app.pending_auto_submit_after_cancel);
        assert_eq!(app.active_view, ActiveView::Config);
        assert!(app.input().text().is_empty());
        assert!(matches!(app.status, AppStatus::Ready));
        assert!(rx.try_recv().is_err(), "config open should not dispatch a prompt turn");
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
}
