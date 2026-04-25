//! Keybindings.
//!
//! - `q` quits.
//! - `a` / `d` answer the permission modal.
//! - `Esc` closes the permission modal without answering (defaults to deny
//!   on disconnect; user can re-issue).
//! - `Up` / `Down` / `Enter` navigate the session list.
//! - `p` claims primary when in viewer mode.

use std::sync::Arc;

use crossterm::event::KeyCode;

use crate::app::{App, Focus, Role};
use crate::client::Client;

/// Handle a key press.
///
/// Returns `Some(true)` to quit; `Some(false)` to keep running with
/// state updated; `None` if the key was unhandled.
pub async fn handle_key(app: &mut App, key: KeyCode, client: &Arc<Client>) -> Option<bool> {
    if let Focus::PermissionModal = app.focus {
        match key {
            KeyCode::Char('a') => {
                answer_permission(app, client, "allow").await;
                return Some(false);
            }
            KeyCode::Char('d') => {
                answer_permission(app, client, "deny").await;
                return Some(false);
            }
            KeyCode::Esc => {
                app.focus = Focus::Conversation;
                return Some(false);
            }
            _ => return None,
        }
    }

    match key {
        KeyCode::Char('q') => Some(true),
        KeyCode::Char('p') if app.role == Role::Viewer => {
            if let Some(sid) = app.current_session.clone() {
                let result = client
                    .call::<_, serde_json::Value>(
                        "session.claim_primary",
                        serde_json::json!({"session_id": sid}),
                    )
                    .await;
                if let Err(e) = result {
                    app.status_msg = format!("claim failed: {e}");
                }
            }
            Some(false)
        }
        KeyCode::Up if app.focus == Focus::SessionList => {
            app.session_list_cursor = app.session_list_cursor.saturating_sub(1);
            Some(false)
        }
        KeyCode::Down if app.focus == Focus::SessionList => {
            if app.session_list_cursor + 1 < app.session_list.len() {
                app.session_list_cursor += 1;
            }
            Some(false)
        }
        KeyCode::Enter if app.focus == Focus::SessionList => {
            if let Some(s) = app.session_list.get(app.session_list_cursor) {
                if let Some(sid) = s.get("session_id").and_then(|v| v.as_str()) {
                    let sid_owned = sid.to_string();
                    app.current_session = Some(sid_owned.clone());
                    app.focus = Focus::Conversation;
                    // Subscribe (fire-and-forget — the App receives events via
                    // the channel routed through the WS read loop).
                    let client = client.clone();
                    tokio::spawn(async move {
                        let _ = client
                            .call::<_, serde_json::Value>(
                                "session.subscribe",
                                serde_json::json!({"session_id": sid_owned}),
                            )
                            .await;
                    });
                }
            }
            Some(false)
        }
        _ => None,
    }
}

async fn answer_permission(app: &mut App, client: &Arc<Client>, decision: &str) {
    let Some(p) = app.pending_permission.take() else {
        return;
    };
    let result = serde_json::json!({"decision": decision});

    let outcome: Result<(), String> = if let Some(prompt_id) = p.prompt_id.as_ref() {
        // Prompt came from the queue — answer via prompts.respond.
        client
            .call::<_, serde_json::Value>(
                "prompts.respond",
                serde_json::json!({
                    "session_id": app.current_session.clone().unwrap_or_default(),
                    "prompt_id": prompt_id,
                    "result": result,
                }),
            )
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    } else {
        // Fresh reverse-RPC — answer with the captured rev_id.
        client
            .send_response(p.rev_id.clone(), result)
            .map_err(|e| e.to_string())
    };

    app.focus = Focus::Conversation;
    if let Err(e) = outcome {
        app.status_msg = format!("permission answer failed: {e}");
    }
}
