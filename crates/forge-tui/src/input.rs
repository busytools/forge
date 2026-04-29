//! Per-screen key dispatch.
//!
//! Returns `true` when the loop should quit; `false` otherwise.

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::mpsc;

use crate::app::{App, AppEvent, Role, Screen};
use crate::client::Client;

/// Handle a single key press. Modal first, then per-screen dispatch.
///
/// Returns `true` to quit, `false` to keep running.
pub async fn handle_key(
    app: &mut App,
    key: KeyEvent,
    client: &Arc<Client>,
    event_tx: &mpsc::UnboundedSender<AppEvent>,
) -> bool {
    if app.pending_permission.is_some() {
        handle_modal_key(app, key.code, client).await;
        return false;
    }

    match app.active_view {
        Screen::Connecting => handle_connecting_key(key.code),
        Screen::SessionPicker => handle_picker_key(app, key.code, client, event_tx).await,
        Screen::Chat => handle_conversation_key(app, key, client).await,
        Screen::Disconnected => handle_disconnected_key(key.code),
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
    let count = app.recent_sessions.len();

    match key {
        KeyCode::Char('q' | 'Q') => return true,
        KeyCode::Up => {
            app.session_picker.selected = app.session_picker.selected.saturating_sub(1);
        }
        KeyCode::Down if app.session_picker.selected + 1 < count => {
            app.session_picker.selected += 1;
        }
        KeyCode::Esc => {
            spawn_new_session(app, client, event_tx).await;
        }
        KeyCode::Enter => {
            if let Some(session) = app.recent_sessions.get(app.session_picker.selected) {
                let sid = session.session_id.clone();
                open_session(app, sid, client, event_tx);
            }
        }
        _ => {}
    }
    false
}

async fn handle_conversation_key(
    app: &mut App,
    key: KeyEvent,
    client: &Arc<Client>,
) -> bool {
    const PAGE_STEP: u16 = 10;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Esc => {
            // Esc closes help/todo overlays before backing out of the chat.
            if app.help_open {
                app.help_open = false;
                return false;
            }
            if app.show_todo_panel {
                app.show_todo_panel = false;
                return false;
            }
            app.active_view = Screen::SessionPicker;
            app.current_session = None;
            app.legacy_messages.clear();
            app.messages.clear();
            app.rendered_lines.clear();
            app.role = Role::Vacant;
            app.input.clear();
            app.draft.clear();
            app.tool_call_index.clear();
            app.conv_scroll_back = 0;
        }
        // F1 toggles the lifted help overlay.
        KeyCode::F(1) => {
            app.help_open = !app.help_open;
        }
        // Ctrl+T toggles the lifted todo panel (no-op when there are no todos).
        KeyCode::Char('t' | 'T') if ctrl && !app.todos.is_empty() => {
            app.show_todo_panel = !app.show_todo_panel;
        }
        // Scroll-from-bottom: `conv_scroll_back` = lines back from live
        // tail. Higher = older content; `0` is the live tail.
        KeyCode::PageUp => {
            app.conv_scroll_back = app.conv_scroll_back.saturating_add(PAGE_STEP);
        }
        KeyCode::PageDown => {
            app.conv_scroll_back = app.conv_scroll_back.saturating_sub(PAGE_STEP);
        }
        // Trackpad arrives as Up/Down arrow keys with mouse capture off.
        // 3 lines per arrow event so a fast swipe travels visibly —
        // Ghostty/iTerm only deliver ~5-10 events per swipe.
        KeyCode::Up => {
            app.conv_scroll_back = app.conv_scroll_back.saturating_add(3);
        }
        KeyCode::Down => {
            app.conv_scroll_back = app.conv_scroll_back.saturating_sub(3);
        }
        KeyCode::Home => {
            app.conv_scroll_back = u16::MAX; // render clamps to top
        }
        KeyCode::End => {
            app.conv_scroll_back = 0;
        }
        KeyCode::Char(c) if !ctrl => {
            if c == 'q' && app.input.is_empty() {
                // q on empty input = quit; while typing, q is a literal char.
                return true;
            }
            app.input.textarea_insert_char(c);
        }
        KeyCode::Backspace => {
            app.input.textarea_delete_char_before();
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
    app.legacy_messages.clear();
    app.messages.clear();
    app.tool_call_index.clear();
    app.active_view = Screen::Chat;
    app.input.clear();
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
    let history_sid = sid.clone();
    tokio::spawn(async move {
        let result: Result<serde_json::Value, _> = history_client
            .call(
                "sessions.messages",
                serde_json::json!({"session_id": history_sid}),
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
                tracing::warn!(error = %e, sid = %history_sid, "sessions.messages failed");
            }
        }
    });

    spawn_footer_poller(client.clone(), event_tx.clone(), sid);
}

/// Footer poller: every ~1s, fetch `session.current_model` +
/// `context.get` + `mcp.status` and forward to the `AppEvent` handler.
/// The poller dies when its `event_tx` is dropped (i.e. when the TUI
/// exits) — graceful by design.
fn spawn_footer_poller(
    client: Arc<Client>,
    event_tx: mpsc::UnboundedSender<AppEvent>,
    sid: String,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if !poll_current_model(&client, &event_tx, &sid).await
                || !poll_context_usage(&client, &event_tx, &sid).await
                || !poll_mcp_status(&client, &event_tx, &sid).await
            {
                return;
            }
        }
    });
}

async fn poll_current_model(
    client: &Arc<Client>,
    event_tx: &mpsc::UnboundedSender<AppEvent>,
    sid: &str,
) -> bool {
    match client
        .call::<_, serde_json::Value>(
            "session.current_model",
            serde_json::json!({"session_id": sid}),
        )
        .await
    {
        Ok(v) => {
            let model_id = v
                .get("model")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            event_tx
                .send(AppEvent::CurrentModelSnapshot(model_id))
                .is_ok()
        }
        Err(e) => {
            tracing::warn!(error = %e, sid = %sid, "session.current_model failed");
            true
        }
    }
}

async fn poll_context_usage(
    client: &Arc<Client>,
    event_tx: &mpsc::UnboundedSender<AppEvent>,
    sid: &str,
) -> bool {
    match client
        .call::<_, serde_json::Value>("context.get", serde_json::json!({"session_id": sid}))
        .await
    {
        Ok(v) => {
            let percent = crate::state::wire_adapter::json_to_context_usage_percent(&v);
            event_tx
                .send(AppEvent::ContextUsageSnapshot { percent })
                .is_ok()
        }
        Err(e) => {
            tracing::warn!(error = %e, sid = %sid, "context.get failed");
            true
        }
    }
}

async fn poll_mcp_status(
    client: &Arc<Client>,
    event_tx: &mpsc::UnboundedSender<AppEvent>,
    sid: &str,
) -> bool {
    match client
        .call::<_, serde_json::Value>("mcp.status", serde_json::json!({"session_id": sid}))
        .await
    {
        Ok(v) => event_tx.send(AppEvent::McpStatusSnapshot(v)).is_ok(),
        Err(e) => {
            tracing::warn!(error = %e, sid = %sid, "mcp.status failed");
            true
        }
    }
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
    let prompt = app.input.text();
    if prompt.trim().is_empty() {
        return;
    }
    app.input.clear();
    app.draft.clear();
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
    if let Some(rest) = raw.strip_prefix("daemon error code ")
        && let Some((code_str, msg)) = rest.split_once(": ")
    {
        return match code_str {
            "-32002" => format!("session not found: {msg}"),
            "-32100" => format!("claude binary not found: {msg}"),
            "-32101" => format!("claude exited unexpectedly: {msg}"),
            "-32102" => format!("subprocess error: {msg}"),
            _ => msg.to_string(),
        };
    }
    raw.to_string()
}
