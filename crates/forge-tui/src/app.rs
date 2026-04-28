//! TUI app state machine + event loop.
//!
//! `App` is the single source of truth for what's rendered. The run
//! loop drains [`AppEvent`]s from one channel that three sources feed:
//! terminal input, daemon notifications, and reverse-RPC requests.
//!
//! Screen transitions happen inside the run loop in response to events.
//! E.g. `SessionListLoaded` flips us from `Screen::Connecting` to
//! `Screen::Picker`; `Enter` on a picker row flips to
//! `Screen::Conversation` after issuing `session.subscribe`.

use std::sync::Arc;

use crossterm::event::{Event, KeyEventKind, MouseEvent, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::Backend;
use tokio::sync::mpsc;

use crate::client::Client;

/// Top-level UI screen. Each screen owns its own layout + input rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Screen {
    /// WS handshake in progress; initial picker load pending.
    #[default]
    Connecting,
    /// Picking a session from the list (or starting a new one).
    Picker,
    /// Watching/driving a subscribed session.
    Conversation,
    /// WS dropped; retry overlay.
    Disconnected,
}

/// Connection state — drives the footer connection glyph.
/// Independent of [`Screen`] because the user can still be reading
/// chat history while we reconnect underneath.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ConnectionState {
    /// Initial handshake.
    #[default]
    Connecting,
    /// Live link to the daemon.
    Connected,
    /// Backoff retry pending.
    Reconnecting {
        /// Seconds until the next retry attempt.
        next_retry_secs: u32,
    },
    /// Gave up retrying or the user dismissed.
    Disconnected,
}

/// Local primary/viewer role for the currently subscribed session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Role {
    /// No session subscribed.
    #[default]
    Vacant,
    /// We hold primary; permission/hook requests come to us.
    Primary,
    /// Someone else holds primary; we read but don't answer.
    Viewer,
}

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
    /// session we just opened. Replaces `app.messages` to seed the
    /// conversation view before live events start arriving.
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

/// Snapshot of an outstanding permission request awaiting user input.
#[derive(Debug)]
#[non_exhaustive]
pub struct PendingPermission {
    /// JSON-RPC id of the originating reverse-RPC.
    pub rev_id: serde_json::Value,
    /// Original params from the request.
    pub params: serde_json::Value,
    /// Set when the prompt came in via the daemon's queue (after
    /// reconnect). Queued prompts answer via `prompts.respond` rather
    /// than a synchronous reverse-RPC response.
    pub prompt_id: Option<String>,
}

impl PendingPermission {
    /// Construct a `PendingPermission`.
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

/// Top-level mutable state. Owned by the run loop.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct App {
    // Foreground
    /// Active screen.
    pub screen: Screen,
    /// Connection state for the footer glyph.
    pub connection: ConnectionState,

    // Footer display
    /// Daemon URL (e.g. `ws://127.0.0.1:7373/`).
    pub daemon_url: String,
    /// Working directory at startup.
    pub cwd: String,

    // Picker
    /// Sessions returned by `sessions.list` for `cwd`.
    pub session_list: Vec<serde_json::Value>,
    /// Selected row in the picker (0 = "New session" pseudo-row).
    pub picker_cursor: usize,

    // Conversation
    /// Currently subscribed session id.
    pub current_session: Option<String>,
    /// Conversation transcript — appended by `SessionFrame`.
    pub messages: Vec<serde_json::Value>,
    /// Input draft buffer.
    pub draft: String,
    /// Local primary/viewer role for `current_session`.
    pub role: Role,
    /// Distance from the bottom of the conversation body, in lines.
    /// `0` = pinned to bottom (auto-tail). Larger = scrolled further
    /// back into history. Render clamps to actual max.
    pub conv_scroll_back: u16,
    /// Cached pre-built styled lines for the current `messages`. Saves
    /// the per-keypress rebuild cost when scrolling a large transcript
    /// (1000+ messages = thousands of allocations per arrow event
    /// otherwise). Invalidated whenever `messages` changes.
    pub rendered_lines: Vec<ratatui::text::Line<'static>>,

    // Modal
    /// Active permission modal, if any.
    pub pending_permission: Option<PendingPermission>,

    // Misc
    /// One-line status/toast message.
    pub status_msg: String,
}

impl App {
    /// Construct an `App` with the daemon URL + cwd captured at startup
    /// for footer display.
    #[must_use]
    pub fn new(daemon_url: String, cwd: String) -> Self {
        Self {
            daemon_url,
            cwd,
            ..Self::default()
        }
    }

    /// Rebuild the conversation render cache from `self.messages`.
    /// Call after any mutation of `messages`. Cheap to call on a fresh
    /// session (no messages yet); expensive once but the cost is paid
    /// once per turn rather than once per keypress.
    pub fn rebuild_rendered_lines(&mut self) {
        self.rendered_lines = crate::ui::conversation::build_lines(&self.messages);
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
            .draw(|f| crate::ui::render(f, &app))
            .map_err(|e| std::io::Error::other(format!("draw failed: {e}")))?;
        frames += 1;
        if frames == 1 {
            tracing::info!(
                screen = ?app.screen,
                connection = ?app.connection,
                "first frame drawn"
            );
        }

        let Some(event) = events.recv().await else {
            break;
        };
        tracing::debug!(?event, ?app.screen, "event");
        match event {
            AppEvent::Quit => break,
            AppEvent::Term(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                if crate::input::handle_key(&mut app, k.code, &client, &event_tx).await {
                    break;
                }
            }
            AppEvent::Term(Event::Mouse(m)) => {
                handle_mouse(&mut app, m);
            }
            AppEvent::Term(_) => {}

            AppEvent::Connected => {
                app.connection = ConnectionState::Connected;
                if app.screen == Screen::Disconnected {
                    // Reconnected — return to the conversation if we
                    // had one; otherwise back to the picker.
                    app.screen = if app.current_session.is_some() {
                        Screen::Conversation
                    } else {
                        Screen::Picker
                    };
                }
            }
            AppEvent::Disconnected { next_retry_secs } => {
                app.connection = ConnectionState::Reconnecting { next_retry_secs };
                app.screen = Screen::Disconnected;
            }

            AppEvent::SessionListLoaded(items) => {
                app.session_list = items;
                if app.picker_cursor > app.session_list.len() {
                    app.picker_cursor = app.session_list.len();
                }
                if app.screen == Screen::Connecting {
                    app.screen = Screen::Picker;
                }
            }
            AppEvent::SessionListLoadFailed(message) => {
                app.status_msg = format!("session list load failed: {message}");
                if app.screen == Screen::Connecting {
                    app.screen = Screen::Picker;
                }
            }

            AppEvent::SessionFrame(frame) => {
                if let Some(msg) = frame.get("message").cloned() {
                    app.messages.push(msg);
                    app.rebuild_rendered_lines();
                    // No tail-follow logic needed — scroll-from-bottom
                    // model means "0" is always the live tail.
                }
            }
            AppEvent::HistoricalLoaded(history) => {
                // Seed the transcript view. If live events have already
                // arrived (race between subscribe and history fetch),
                // they're appended after.
                let live = std::mem::take(&mut app.messages);
                app.messages = history;
                app.messages.extend(live);
                app.rebuild_rendered_lines();
                // Pin to bottom so the user sees the most recent turns.
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
                if let (Some(pp), Some(expired_id)) = (&app.pending_permission, expired_id) {
                    if pp.prompt_id.as_deref() == Some(expired_id) {
                        app.pending_permission = None;
                        app.status_msg = "permission prompt expired".into();
                    }
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
                    app.messages.clear();
                    app.role = Role::Vacant;
                    app.screen = Screen::Picker;
                    app.status_msg = format!("session closed: {reason}");
                }
            }
        }
    }

    Ok(())
}

const MOUSE_SCROLL_STEP: u16 = 5;

fn handle_mouse(app: &mut App, m: MouseEvent) {
    if app.screen != Screen::Conversation {
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
