//! Per-screen key dispatch.
//!
//! Returns `true` when the loop should quit; `false` otherwise.

use std::sync::Arc;

use crossterm::event::KeyCode;
use tokio::sync::mpsc;

use crate::app::{App, AppEvent, Role, Screen};
use crate::client::Client;

/// Handle a single key press. Modal first, then per-screen dispatch.
///
/// Returns `true` to quit, `false` to keep running.
pub async fn handle_key(
    app: &mut App,
    key: KeyCode,
    client: &Arc<Client>,
    event_tx: &mpsc::UnboundedSender<AppEvent>,
) -> bool {
    if app.pending_permission.is_some() {
        handle_modal_key(app, key, client).await;
        return false;
    }

    match app.screen {
        Screen::Connecting => handle_connecting_key(key),
        Screen::Picker => handle_picker_key(app, key, client, event_tx).await,
        Screen::Conversation => handle_conversation_key(app, key, client).await,
        Screen::Disconnected => handle_disconnected_key(key),
    }
}

fn handle_connecting_key(key: KeyCode) -> bool {
    matches!(key, KeyCode::Char('q' | 'Q'))
}

fn handle_disconnected_key(key: KeyCode) -> bool {
    matches!(key, KeyCode::Char('q' | 'Q'))
}

async fn handle_modal_key(app: &mut App, key: KeyCode, client: &Arc<Client>) {
    match key {
        KeyCode::Char('a' | 'A') => {
            answer_permission(app, client, "allow").await;
        }
        KeyCode::Char('d' | 'D') | KeyCode::Esc => {
            answer_permission(app, client, "deny").await;
        }
        _ => {}
    }
}

async fn handle_picker_key(
    app: &mut App,
    key: KeyCode,
    client: &Arc<Client>,
    event_tx: &mpsc::UnboundedSender<AppEvent>,
) -> bool {
    let row_count = app.session_list.len() + 1; // +1 for "New session" pseudo-row

    match key {
        KeyCode::Char('q' | 'Q') => return true,
        KeyCode::Up => {
            app.picker_cursor = app.picker_cursor.saturating_sub(1);
        }
        KeyCode::Down if app.picker_cursor + 1 < row_count => {
            app.picker_cursor += 1;
        }
        KeyCode::Enter => {
            if app.picker_cursor == 0 {
                spawn_new_session(app, client, event_tx).await;
            } else if let Some(session) = app.session_list.get(app.picker_cursor - 1) {
                if let Some(sid) = session.get("session_id").and_then(|v| v.as_str()) {
                    open_session(app, sid.to_string(), client, event_tx);
                }
            }
        }
        _ => {}
    }
    false
}

async fn handle_conversation_key(
    app: &mut App,
    key: KeyCode,
    client: &Arc<Client>,
) -> bool {
    match key {
        KeyCode::Esc => {
            app.screen = Screen::Picker;
            app.current_session = None;
            app.messages.clear();
            app.role = Role::Vacant;
            app.draft.clear();
        }
        KeyCode::Char(c) => {
            if c == 'q' && app.draft.is_empty() {
                // q on an empty draft = quit; while typing, q is a literal char
                return true;
            }
            app.draft.push(c);
        }
        KeyCode::Backspace => {
            app.draft.pop();
        }
        KeyCode::Enter => {
            send_draft(app, client).await;
        }
        _ => {}
    }
    false
}

fn open_session(
    app: &mut App,
    sid: String,
    client: &Arc<Client>,
    event_tx: &mpsc::UnboundedSender<AppEvent>,
) {
    app.current_session = Some(sid.clone());
    app.messages.clear();
    app.screen = Screen::Conversation;
    app.draft.clear();

    // Subscribe in parallel with the historical fetch.
    let subscribe_client = client.clone();
    let subscribe_tx = event_tx.clone();
    let subscribe_sid = sid.clone();
    tokio::spawn(async move {
        match subscribe_client.subscribe_session(&subscribe_sid).await {
            Ok(mut stream) => {
                use futures_util::StreamExt;
                while let Some(frame) = stream.next().await {
                    if subscribe_tx
                        .send(AppEvent::SessionFrame(frame))
                        .is_err()
                    {
                        break;
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, sid = %subscribe_sid, "session.subscribe failed");
            }
        }
    });

    // Fetch the historical transcript so the conversation view is
    // pre-populated rather than blank until the next assistant turn.
    let history_client = client.clone();
    let history_tx = event_tx.clone();
    tokio::spawn(async move {
        let result: Result<serde_json::Value, _> = history_client
            .call(
                "sessions.messages",
                serde_json::json!({"session_id": sid}),
            )
            .await;
        match result {
            Ok(v) => {
                let messages = v
                    .get("messages")
                    .and_then(|m| m.as_array())
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|entry| entry.get("message").cloned())
                    .collect();
                let _ = history_tx.send(AppEvent::HistoricalLoaded(messages));
            }
            Err(e) => {
                tracing::warn!(error = %e, sid = %sid, "sessions.messages failed");
            }
        }
    });
}

async fn spawn_new_session(
    app: &mut App,
    client: &Arc<Client>,
    event_tx: &mpsc::UnboundedSender<AppEvent>,
) {
    // Minimal default spawn — daemon defaults the binary, etc.
    let result = client
        .call::<_, serde_json::Value>(
            "session.spawn",
            serde_json::json!({"options": {}}),
        )
        .await;
    match result {
        Ok(v) => {
            if let Some(sid) = v.get("session_id").and_then(|v| v.as_str()) {
                open_session(app, sid.to_string(), client, event_tx);
            } else {
                app.status_msg = "session.spawn: no session_id in result".into();
            }
        }
        Err(e) => {
            app.status_msg = format!("session.spawn failed: {}", friendly_error(&e.to_string()));
        }
    }
}

async fn send_draft(app: &mut App, client: &Arc<Client>) {
    let Some(sid) = app.current_session.clone() else {
        return;
    };
    let prompt = std::mem::take(&mut app.draft);
    if prompt.trim().is_empty() {
        return;
    }
    let result = client
        .call::<_, serde_json::Value>(
            "session.send_user_message",
            serde_json::json!({"session_id": sid, "prompt": prompt}),
        )
        .await;
    if let Err(e) = result {
        app.status_msg = format!("send failed: {}", friendly_error(&e.to_string()));
    }
}

async fn answer_permission(app: &mut App, client: &Arc<Client>, decision: &str) {
    let Some(p) = app.pending_permission.take() else {
        return;
    };
    let result = serde_json::json!({"decision": decision});

    let session_id_from_params = p
        .params
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(String::from);

    let outcome: Result<(), String> = match (p.prompt_id.as_ref(), session_id_from_params.as_ref())
    {
        (Some(prompt_id), Some(sid)) => client
            .call::<_, serde_json::Value>(
                "prompts.respond",
                serde_json::json!({
                    "session_id": sid,
                    "prompt_id": prompt_id,
                    "result": result,
                }),
            )
            .await
            .map(|_| ())
            .map_err(|e| e.to_string()),
        (Some(_), None) => Err("queued prompt missing session_id".into()),
        (None, _) => client
            .send_response(p.rev_id.clone(), result)
            .map_err(|e| e.to_string()),
    };

    if let Err(e) = outcome {
        app.status_msg = format!("permission answer failed: {}", friendly_error(&e));
    }
}

/// Map raw daemon error messages to user-facing strings.
/// Strips the JSON-RPC `-32xxx` prefix when present.
fn friendly_error(raw: &str) -> String {
    // Pattern: "daemon error code -32101: <message>"
    if let Some(rest) = raw.strip_prefix("daemon error code ") {
        if let Some((code_str, msg)) = rest.split_once(": ") {
            return match code_str {
                "-32002" => format!("session not found: {msg}"),
                "-32100" => format!("claude binary not found: {msg}"),
                "-32101" => format!("claude exited unexpectedly: {msg}"),
                "-32102" => format!("subprocess error: {msg}"),
                _ => msg.to_string(),
            };
        }
    }
    raw.to_string()
}
