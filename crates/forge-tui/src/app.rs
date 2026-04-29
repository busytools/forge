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
                if app.picker_cursor > app.session_list.len() {
                    app.picker_cursor = app.session_list.len();
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
                app.pending_permission =
                    Some(PendingPermission::new(rev_id, params, prompt_id));
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
        }
    }

    Ok(())
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
