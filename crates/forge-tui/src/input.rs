//! Keybindings.
//!
//! - `q` quits.
//! - `a` / `d` answer the permission modal.
//! - `Esc` closes the permission modal without answering (defaults to deny
//!   on disconnect; user can re-issue).
//! - `Up` / `Down` / `Enter` navigate the session list.
//! - `p` claims primary when in viewer mode.
//
// TODO: branch-coverage tests for each key path (Up / Down / Enter /
// claim / answer_permission) need a mock `Client`. Adding a
// `ClientApi` trait abstraction is heavier than the current parity-
// first phase warrants; the e2e flows are covered indirectly by
// `tests/app_smoke.rs`. Revisit once forge-tui's Client surface
// stabilises.

use std::sync::Arc;

use crossterm::event::KeyCode;
use tokio::sync::mpsc;

use crate::app::{App, AppEvent, Focus, Role};
use crate::client::Client;

/// Handle a key press.
///
/// Returns `Some(true)` to quit; `Some(false)` to keep running with
/// state updated; `None` if the key was unhandled.
///
/// `event_tx` is forwarded to the Enter-on-session-list path so the
/// background subscription pump can post `AppEvent::SessionFrame` back
/// into the app loop. Other paths don't need it.
pub async fn handle_key(
    app: &mut App,
    key: KeyCode,
    client: &Arc<Client>,
    event_tx: &mpsc::UnboundedSender<AppEvent>,
) -> Option<bool> {
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
                    // Subscribe via `Client::subscribe_session` so the
                    // returned mpsc gets pumped through `event_tx` as
                    // `AppEvent::SessionFrame`. Calling
                    // `client.call("session.subscribe", ...)` directly
                    // would issue the daemon RPC but skip the local
                    // mpsc registration — every notification would be
                    // silently dropped by the read loop.
                    let client = client.clone();
                    let event_tx = event_tx.clone();
                    tokio::spawn(async move {
                        match client.subscribe_session(&sid_owned).await {
                            Ok(mut stream) => {
                                use futures_util::StreamExt;
                                while let Some(frame) = stream.next().await {
                                    if event_tx.send(AppEvent::SessionFrame(frame)).is_err() {
                                        break;
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, sid = %sid_owned, "session.subscribe failed");
                            }
                        }
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

    // Round 4 — fix I1. Cross-session routing bug: previously
    // `prompts.respond` used `app.current_session` to identify the
    // session whose prompt is being answered, which routes to the
    // wrong session whenever the user is the primary on multiple
    // sessions and a modal for B fires while they're viewing A.
    // Source the session_id from the prompt's params instead — the
    // daemon's reverse-RPC envelope carries it alongside `prompt_id`,
    // so it always names the session that originated the prompt.
    let session_id_from_params = p
        .params
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(String::from);

    let outcome: Result<(), String> = match (p.prompt_id.as_ref(), session_id_from_params.as_ref())
    {
        (Some(prompt_id), Some(sid)) => {
            // Prompt came from the queue — answer via prompts.respond,
            // routing on the session_id the daemon attached to the
            // envelope rather than the user's currently-viewed session.
            client
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
                .map_err(|e| e.to_string())
        }
        (Some(_), None) => {
            tracing::warn!(
                "answer_permission: queued prompt missing session_id in params; cannot route"
            );
            Err("queued prompt missing session_id".into())
        }
        (None, _) => {
            // Fresh reverse-RPC — answer with the captured rev_id.
            client
                .send_response(p.rev_id.clone(), result)
                .map_err(|e| e.to_string())
        }
    };

    app.focus = Focus::Conversation;
    if let Err(e) = outcome {
        app.status_msg = format!("permission answer failed: {e}");
    }
}
