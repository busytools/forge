//! TUI app event loop. The mutable state lives in
//! [`crate::state::app::App`]; this module owns the event-loop runtime
//! that drains [`AppEvent`]s + drives the renderer.

use std::sync::Arc;

use crossterm::event::{Event, KeyEventKind, MouseEvent, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::Backend;
use tokio::sync::mpsc;

use crate::client::Client;

// Re-exports so legacy `use crate::app::App` etc. keep compiling.
// The canonical types live in `state::app`.
pub use crate::state::app::{
    ActiveView as Screen, App, ConnectionState, PendingPermission, Role,
};
use crate::state::app::ActiveView;

/// Every event the run loop handles, regardless of source.
#[derive(Debug)]
#[non_exhaustive]
pub enum AppEvent {
    /// Raw terminal input.
    Term(Event),
    /// WS link came up.
    Connected,
    /// WS link dropped; backoff timer carries seconds until next retry.
    Disconnected {
        /// Seconds until next retry attempt.
        next_retry_secs: u32,
    },
    /// `session.event` notification payload (chat stream chunk).
    SessionFrame(serde_json::Value),
    /// `sessions.messages` historical transcript loaded for the
    /// session we just opened.
    HistoricalLoaded(Vec<serde_json::Value>),
    /// `sessions.list` snapshot loaded.
    SessionListLoaded(Vec<serde_json::Value>),
    /// `sessions.list` failed at startup.
    SessionListLoadFailed(String),
    /// Reverse-RPC `permission.request` arrived.
    PermissionRequest {
        /// JSON-RPC id of the inbound request — must be echoed back.
        rev_id: serde_json::Value,
        /// Original params (`tool_name`, `tool_input`, optional `prompt_id`).
        params: serde_json::Value,
    },
    /// `session.role_assigned` — local role flip.
    RoleChanged(serde_json::Value),
    /// `session.primary_changed` — daemon broadcast.
    PrimaryChanged(serde_json::Value),
    /// `session.closed` — session actor exited.
    SessionClosed(serde_json::Value),
    /// `prompts.expired` — drop matching modal.
    PromptsExpired(serde_json::Value),
    /// `session.current_model` poll reply — model id from CLI's
    /// system/init payload (kept in sync with `session.set_model`).
    CurrentModelSnapshot(Option<String>),
    /// `context.get` poll reply — current-context-window usage.
    ContextUsageSnapshot {
        /// Used context as a percent of the model's window.
        percent: Option<u8>,
    },
    /// `mcp.status` poll reply — list of MCP servers and their state.
    McpStatusSnapshot(serde_json::Value),
    /// External quit signal.
    Quit,
}

/// Run the app event loop until quit.
///
/// # Errors
///
/// Terminal I/O errors propagate.
#[allow(clippy::too_many_lines, reason = "event-handler match needs to stay in one place")]
pub async fn run<B: Backend>(
    terminal: &mut Terminal<B>,
    client: Arc<Client>,
    mut app: App,
    event_tx: mpsc::UnboundedSender<AppEvent>,
    mut events: mpsc::UnboundedReceiver<AppEvent>,
) -> std::io::Result<()> {
    let mut frames = 0_u64;
    loop {
        terminal
            .draw(|f| crate::ui::render(f, &mut app))
            .map_err(|e| std::io::Error::other(format!("draw failed: {e}")))?;
        frames += 1;
        if frames == 1 {
            tracing::info!(
                view = ?app.active_view,
                connection = ?app.connection,
                "first frame drawn"
            );
        }

        let Some(event) = events.recv().await else {
            break;
        };
        match event {
            AppEvent::Quit => break,
            AppEvent::Term(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                if crate::input::handle_key(&mut app, k, &client, &event_tx).await {
                    break;
                }
            }
            AppEvent::Term(Event::Mouse(m)) => {
                handle_mouse(&mut app, m);
            }
            AppEvent::Term(_) => {}

            AppEvent::Connected => {
                app.connection = ConnectionState::Connected;
                if app.active_view == ActiveView::Disconnected {
                    app.active_view = if app.current_session.is_some() {
                        ActiveView::Chat
                    } else {
                        ActiveView::SessionPicker
                    };
                }
            }
            AppEvent::Disconnected { next_retry_secs } => {
                app.connection = ConnectionState::Reconnecting { next_retry_secs };
                app.active_view = ActiveView::Disconnected;
            }

            AppEvent::SessionListLoaded(items) => {
                app.recent_sessions =
                    crate::state::wire_adapter::session_list_to_recent_sessions(&items);
                app.session_list = items;
                let max_idx = app.recent_sessions.len().saturating_sub(1);
                if app.session_picker.selected > max_idx {
                    app.session_picker.selected = max_idx;
                }
                if app.active_view == ActiveView::Connecting {
                    app.active_view = ActiveView::SessionPicker;
                }
            }
            AppEvent::SessionListLoadFailed(message) => {
                app.status_msg = format!("session list load failed: {message}");
                if app.active_view == ActiveView::Connecting {
                    app.active_view = ActiveView::SessionPicker;
                }
            }

            AppEvent::SessionFrame(frame) => {
                if let Some(msg) = frame.get("message").cloned() {
                    // Legacy renderer path: keep raw JSON.
                    app.legacy_messages.push(msg.clone());
                    app.rebuild_rendered_lines();
                    // Lifted renderer path: parse + apply to ChatMessage / existing tool calls.
                    crate::state::wire_adapter::apply_session_event(&mut app, &msg);
                }
            }
            AppEvent::HistoricalLoaded(history) => {
                let live_legacy = std::mem::take(&mut app.legacy_messages);
                app.legacy_messages.clone_from(&history);
                app.legacy_messages.extend(live_legacy);
                app.rebuild_rendered_lines();

                // Lifted: replay historical events in chronological order so
                // tool_use blocks are indexed before their tool_results arrive.
                let live_lifted = std::mem::take(&mut app.messages);
                let live_retained = std::mem::take(&mut app.message_retained_bytes);
                app.tool_call_index.clear();
                for h in &history {
                    crate::state::wire_adapter::apply_session_event(&mut app, h);
                }
                let history_len = app.messages.len();
                app.messages.extend(live_lifted);
                app.message_retained_bytes.resize(history_len, 0);
                app.message_retained_bytes.extend(live_retained);

                app.conv_scroll_back = 0;
            }

            AppEvent::PermissionRequest { rev_id, params } => {
                let prompt_id = params
                    .get("prompt_id")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let tool_use_id = params
                    .get("context")
                    .and_then(|c| c.get("tool_use_id"))
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let attached_inline = if let Some(ref tool_id) = tool_use_id
                    && app.lookup_tool_call(tool_id).is_some()
                {
                    attach_inline_permission(
                        &mut app,
                        &client,
                        tool_id,
                        rev_id.clone(),
                        prompt_id.clone(),
                        &params,
                    )
                } else {
                    false
                };
                if !attached_inline {
                    app.pending_permission =
                        Some(PendingPermission::new(rev_id, params, prompt_id));
                }
            }
            AppEvent::PromptsExpired(p) => {
                let expired_id = p.get("prompt_id").and_then(|v| v.as_str());
                if let (Some(pp), Some(expired_id)) = (&app.pending_permission, expired_id)
                    && pp.prompt_id.as_deref() == Some(expired_id)
                {
                    app.pending_permission = None;
                    app.status_msg = "permission prompt expired".into();
                }
            }

            AppEvent::RoleChanged(p) => {
                if let Some(r) = p.get("role").and_then(|v| v.as_str()) {
                    app.role = match r {
                        "primary" => Role::Primary,
                        "viewer" => Role::Viewer,
                        _ => Role::Vacant,
                    };
                }
            }
            AppEvent::PrimaryChanged(p) => {
                let primary = p
                    .get("primary")
                    .and_then(|v| v.as_str())
                    .map_or_else(|| "<none>".into(), String::from);
                app.status_msg = format!("primary now: {primary}");
            }
            AppEvent::SessionClosed(p) => {
                let sid_closed = p.get("session_id").and_then(|v| v.as_str()).unwrap_or("");
                let reason = p.get("reason").and_then(|v| v.as_str()).unwrap_or("");
                if app.current_session.as_deref() == Some(sid_closed) {
                    app.current_session = None;
                    app.legacy_messages.clear();
                    app.messages.clear();
                    app.message_retained_bytes.clear();
                    app.role = Role::Vacant;
                    app.active_view = ActiveView::SessionPicker;
                    app.status_msg = format!("session closed: {reason}");
                }
            }

            AppEvent::CurrentModelSnapshot(model_id) => {
                app.current_model = model_id
                    .as_deref()
                    .map(crate::state::wire_adapter::current_model_from_id);
            }
            AppEvent::ContextUsageSnapshot { percent } => {
                app.session_usage.context_usage_percent = percent;
            }
            AppEvent::McpStatusSnapshot(value) => {
                app.mcp.servers = crate::state::wire_adapter::json_to_mcp_servers(&value);
            }
        }
    }

    Ok(())
}

/// Build an `InlinePermission` for `tool_id` and attach it to the matching
/// tool-call card. Spawns a translator task that, on user response, calls
/// either `prompts.respond` (if a `prompt_id` is set) or `send_response`
/// (raw reverse-RPC) with `{decision: "allow"|"deny"}`.
///
/// Returns `true` when the inline permission was successfully attached.
/// Caller falls back to the legacy modal when this returns `false`.
fn attach_inline_permission(
    app: &mut App,
    client: &Arc<Client>,
    tool_id: &str,
    rev_id: serde_json::Value,
    prompt_id: Option<String>,
    params: &serde_json::Value,
) -> bool {
    use crate::state::model::{
        PermissionOption, PermissionOptionKind, RequestPermissionOutcome,
    };
    use crate::state::tool_call_info::InlinePermission;

    let Some((mi, bi)) = app.lookup_tool_call(tool_id) else { return false };
    let Some(crate::state::messages::MessageBlock::ToolCall(tc)) =
        app.messages.get_mut(mi).and_then(|m| m.blocks.get_mut(bi))
    else {
        return false;
    };

    let options = vec![
        PermissionOption::new("allow", "Allow", PermissionOptionKind::AllowOnce),
        PermissionOption::new("deny", "Deny", PermissionOptionKind::RejectOnce),
    ];
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    let perm = InlinePermission {
        options,
        display: None,
        response_tx,
        selected_index: 0,
        focused: false,
    };
    tc.pending_permission = Some(perm);
    tc.mark_tool_call_layout_dirty();

    let tool_id_owned = tool_id.to_owned();
    if !app
        .pending_interaction_ids
        .iter()
        .any(|id| id == &tool_id_owned)
    {
        app.pending_interaction_ids.push(tool_id_owned);
    }
    app.rebuild_chat_focus_from_state();

    let translator_client = client.clone();
    let translator_rev_id = rev_id;
    let translator_prompt_id = prompt_id;
    let translator_session_id = params
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(String::from);
    tokio::spawn(async move {
        let Ok(response) = response_rx.await else { return };
        let decision = match response.outcome {
            RequestPermissionOutcome::Selected(selected) => {
                if selected.option_id == "allow" { "allow" } else { "deny" }
            }
            RequestPermissionOutcome::Cancelled => "deny",
        };
        let body = serde_json::json!({"decision": decision});

        let result = match (translator_prompt_id.as_ref(), translator_session_id.as_ref()) {
            (Some(pid), Some(sid)) => translator_client
                .call::<_, serde_json::Value>(
                    "prompts.respond",
                    serde_json::json!({
                        "session_id": sid,
                        "prompt_id": pid,
                        "result": body,
                    }),
                )
                .await
                .map(|_| ())
                .map_err(|e| e.to_string()),
            (Some(_), None) => Err("inline permission missing session_id".to_owned()),
            (None, _) => translator_client
                .send_response(translator_rev_id, body)
                .map_err(|e| e.to_string()),
        };
        if let Err(e) = result {
            tracing::warn!(error = %e, "inline permission response failed");
        }
    });
    true
}

const MOUSE_SCROLL_STEP: u16 = 5;

fn handle_mouse(app: &mut App, m: MouseEvent) {
    if app.active_view != ActiveView::Chat {
        return;
    }
    let step = MOUSE_SCROLL_STEP;
    match m.kind {
        MouseEventKind::ScrollUp => {
            app.conv_scroll_back = app.conv_scroll_back.saturating_add(step);
        }
        MouseEventKind::ScrollDown => {
            app.conv_scroll_back = app.conv_scroll_back.saturating_sub(step);
        }
        _ => {}
    }
}
