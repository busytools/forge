//! TUI app state machine + event loop.
//!
//! Three input streams converge on the `AppEvent` channel that the loop
//! drains:
//! - Terminal events (keys, resize) via `crossterm::event::EventStream`.
//! - Daemon notifications (`session.event`, `role_assigned`,
//!   `primary_changed`, `prompts.expired`).
//! - Reverse-RPC inbound (`permission.request`, `hook.<kind>`).
//!
//! All reverse-RPC handlers in `main.rs` capture the `rev_id` and forward
//! it through `AppEvent::PermissionRequest` so the keypress handler can
//! later answer via [`crate::client::Client::send_response`].

use std::sync::Arc;

use crossterm::event::{Event, KeyEventKind};
use ratatui::Terminal;
use ratatui::backend::Backend;
use tokio::sync::mpsc;

use crate::client::Client;

/// All events the app loop handles, regardless of source.
#[derive(Debug)]
#[non_exhaustive]
pub enum AppEvent {
    /// Raw terminal event from `crossterm::event::EventStream`.
    Term(crossterm::event::Event),
    /// A `session.event` notification payload.
    SessionFrame(serde_json::Value),
    /// Initial session list snapshot (loaded once at startup).
    SessionListLoaded(Vec<serde_json::Value>),
    /// `sessions.list` failed at startup. Carries the human-readable
    /// error message so the app loop can surface it on the status line
    /// rather than silently rendering an empty list.
    SessionListLoadFailed(String),
    /// A reverse-RPC `permission.request` arrived.
    PermissionRequest {
        /// JSON-RPC id of the inbound request — must be echoed back via
        /// [`crate::client::Client::send_response`] when the user answers.
        rev_id: serde_json::Value,
        /// Wrapped params from the request (the daemon passes `tool_name`,
        /// `tool_input`, optional `prompt_id`).
        params: serde_json::Value,
    },
    /// `role_assigned` notification — local primary/viewer state changed.
    RoleChanged(serde_json::Value),
    /// `primary_changed` notification — daemon reports the primary slot
    /// for some session has been claimed/cleared.
    PrimaryChanged(serde_json::Value),
    /// `session.closed` notification — daemon emits when the session
    /// actor exits (any reason). Carries `{session_id, reason}`.
    SessionClosed(serde_json::Value),
    /// `prompts.expired` notification — drop matching modal.
    PromptsExpired(serde_json::Value),
    /// External quit signal (currently unused; provided for tests).
    Quit,
}

/// Whole-app state. Owned by the event loop; mutated in place.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct App {
    /// Currently subscribed session, if any.
    pub current_session: Option<String>,
    /// Conversation transcript for `current_session` — appended to as
    /// `session.event` frames arrive.
    pub messages: Vec<serde_json::Value>,
    /// Local primary/viewer role for `current_session`.
    pub role: Role,
    /// Active permission modal, if a reverse-RPC is awaiting an answer.
    pub pending_permission: Option<PendingPermission>,
    /// Filesystem-level session list (loaded once at startup).
    pub session_list: Vec<serde_json::Value>,
    /// Current keyboard focus.
    pub focus: Focus,
    /// One-line status message rendered at the bottom of the screen.
    pub status_msg: String,
    /// Cursor position in the session list panel.
    pub session_list_cursor: usize,
}

/// Which UI element currently consumes keys.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum Focus {
    /// Session list panel (default at startup).
    #[default]
    SessionList,
    /// Conversation panel (after picking a session).
    Conversation,
    /// Modal answering a permission request.
    PermissionModal,
}

/// Local primary/viewer role for the currently subscribed session.
#[derive(Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum Role {
    /// No session subscribed yet.
    #[default]
    Vacant,
    /// We are the primary (callbacks/permission requests come to us).
    Primary,
    /// We are a viewer (only see notifications; can claim primary).
    Viewer,
}

/// Snapshot of an outstanding permission request awaiting user input.
#[derive(Debug)]
#[non_exhaustive]
pub struct PendingPermission {
    /// JSON-RPC id of the originating reverse-RPC; required to answer
    /// fresh requests via [`crate::client::Client::send_response`].
    pub rev_id: serde_json::Value,
    /// Original params (`tool_name`, `tool_input`, etc.).
    pub params: serde_json::Value,
    /// Set when the prompt came in via the persisted queue (after
    /// reconnect). Queued prompts are answered via `prompts.respond`
    /// rather than a synchronous reverse-RPC response.
    pub prompt_id: Option<String>,
}

impl PendingPermission {
    /// Construct a `PendingPermission`. Forward-compatible because the
    /// type is `#[non_exhaustive]`; downstream code must use this rather
    /// than the struct literal so future fields are added without
    /// breaking changes.
    #[must_use]
    pub fn new(
        rev_id: serde_json::Value,
        params: serde_json::Value,
        prompt_id: Option<String>,
    ) -> Self {
        Self {
            rev_id,
            params,
            prompt_id,
        }
    }
}

/// Run the app event loop until quit.
///
/// `event_tx` is forwarded into key handlers so background tasks
/// (subscription pumps, etc.) can post `AppEvent`s back into the loop.
///
/// # Errors
///
/// Terminal I/O errors propagate.
pub async fn run<B: Backend>(
    terminal: &mut Terminal<B>,
    client: Arc<Client>,
    event_tx: mpsc::UnboundedSender<AppEvent>,
    mut events: mpsc::UnboundedReceiver<AppEvent>,
) -> std::io::Result<()> {
    let mut app = App::default();

    loop {
        terminal.draw(|f| crate::ui::render(f, &app))?;

        let Some(event) = events.recv().await else {
            break;
        };
        match event {
            AppEvent::Quit => break,
            AppEvent::Term(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                if let Some(quit) =
                    crate::input::handle_key(&mut app, k.code, &client, &event_tx).await
                {
                    if quit {
                        break;
                    }
                }
            }
            AppEvent::Term(_) => {}
            AppEvent::SessionFrame(frame) => {
                if let Some(msg) = frame.get("message").cloned() {
                    app.messages.push(msg);
                }
            }
            AppEvent::SessionListLoaded(items) => {
                app.session_list = items;
                if app.session_list_cursor >= app.session_list.len() {
                    app.session_list_cursor = app.session_list.len().saturating_sub(1);
                }
            }
            AppEvent::SessionListLoadFailed(message) => {
                app.status_msg = format!("session list load failed: {message}");
            }
            AppEvent::PermissionRequest { rev_id, params } => {
                let prompt_id = params
                    .get("prompt_id")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                app.pending_permission = Some(PendingPermission::new(rev_id, params, prompt_id));
                app.focus = Focus::PermissionModal;
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
                // The local `app.role` is owned by `RoleChanged`
                // (from `session.role_assigned`) — `primary_changed`
                // is a session-wide broadcast, not a per-client role
                // update. Surface to the status line so the user can
                // see who is currently primary.
                let primary = p
                    .get("primary")
                    .and_then(|v| v.as_str())
                    .map_or_else(|| "<none>".into(), String::from);
                app.status_msg = format!("primary changed: {primary}");
            }
            AppEvent::SessionClosed(p) => {
                let sid_closed = p.get("session_id").and_then(|v| v.as_str()).unwrap_or("");
                let reason = p.get("reason").and_then(|v| v.as_str()).unwrap_or("");
                if app.current_session.as_deref() == Some(sid_closed) {
                    app.current_session = None;
                    app.role = Role::Vacant;
                    app.focus = Focus::SessionList;
                    app.status_msg = format!("session closed: {reason}");
                }
            }
            AppEvent::PromptsExpired(p) => {
                // Drop the pending modal only if its prompt_id matches
                // the expiry notification — without this, a stray
                // expiry for a sibling session would dismiss an
                // unrelated open modal.
                let expired_id = p.get("prompt_id").and_then(|v| v.as_str());
                if let (Some(pp), Some(expired_id)) = (&app.pending_permission, expired_id) {
                    if pp.prompt_id.as_deref() == Some(expired_id) {
                        app.pending_permission = None;
                        app.focus = Focus::Conversation;
                        app.status_msg = "permission prompt expired".into();
                    }
                }
            }
        }
    }

    Ok(())
}
