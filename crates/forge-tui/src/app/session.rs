//! Per-session state bucket.
//!
//! Phase 2a moves ~50 fields off `App` into this struct. Commit 1
//! (this commit) ships the struct empty — subsequent bucket-migration
//! commits add field groups one bucket at a time, each leaving the
//! tree compiling + tests passing.
//!
//! `App.sessions: HashMap<SessionKey, Session>` holds N sessions;
//! `App.active_session_key` points at the rendered one. Background
//! sessions accumulate state silently while the user is elsewhere
//! (Phase 2 of the side-panes feature; backend prerequisite for the
//! Projects pane UI).

use std::sync::Arc;

use forge_workspace::SessionKey;

use crate::agent::model;
use crate::app::state::messages::ChatMessage;
use crate::app::state::viewport::ChatViewport;

/// Per-session runtime state. Initialised when a session connects;
/// dropped when the session is closed or forge-tui exits.
///
/// `Default` only — `AgentHandle` doesn't derive `Debug` (it owns
/// callback closures), so we can't derive `Debug` here either.
#[derive(Default)]
pub struct Session {
    /// The claude-issued session UUID, also used as the map key.
    /// Stored here for symmetry; the map lookup uses the same value.
    pub key: Option<SessionKey>,
    /// Claude-issued session id (typed wrapper). `None` until the
    /// first `Connected` event from this session's bridge.
    pub session_id: Option<model::SessionId>,
    /// Agent connection handle for this session. `None` while the
    /// session's bridge is starting up.
    pub conn: Option<Arc<forge_agent::AgentHandle>>,
    /// Monotonic session authority epoch — used to ignore stale
    /// async view data after a session reset / reconnect.
    pub session_scope_epoch: u64,
    /// Chat history buffer for this session. Welcome message at
    /// index 0; user/assistant turns appended.
    pub messages: Vec<ChatMessage>,
    /// Cached approximate retained bytes for each message,
    /// parallel to [`Self::messages`].
    pub message_retained_bytes: Vec<usize>,
    /// Rolling total of [`Self::message_retained_bytes`].
    pub retained_history_bytes: usize,
    /// Single owner of all chat layout state: scroll, per-message
    /// heights, prefix sums.
    pub viewport: ChatViewport,
    /// Message index that owns the current main-assistant turn
    /// indicators (spinner, status chips). Cleared on `TurnComplete`.
    pub active_turn_assistant_message_idx: Option<usize>,
}

impl Session {
    #[must_use]
    pub fn new(key: SessionKey) -> Self {
        Self { key: Some(key), ..Self::default() }
    }
}
