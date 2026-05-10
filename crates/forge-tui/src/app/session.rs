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

use forge_workspace::SessionKey;

/// Per-session runtime state. Initialised when a session connects;
/// dropped when the session is closed or forge-tui exits.
#[derive(Debug, Default)]
pub struct Session {
    /// The claude-issued session UUID, also used as the map key.
    /// Stored here for symmetry; the map lookup uses the same value.
    pub key: Option<SessionKey>,
}

impl Session {
    #[must_use]
    pub fn new(key: SessionKey) -> Self {
        Self { key: Some(key) }
    }
}
