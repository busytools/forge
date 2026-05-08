//! Read-only views surfaced by [`crate::Workspace::list_projects`].

use std::time::SystemTime;

use crate::target::{ProjectKey, SessionKey};

/// One project from the catalog plus its sessions, sorted last-
/// activity descending. `sessions[0]` is the lead. Empty `sessions`
/// means the project has no on-disk history yet.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ProjectView {
    pub key: ProjectKey,
    /// Human-readable rendering of the project's root path (e.g.
    /// `~/Projects/forge`, with `~` left in place rather than
    /// expanded). Display-only — not a path you can `open()`.
    pub display_path: String,
    pub sessions: Vec<SessionView>,
}

/// One session under a project.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct SessionView {
    pub session: SessionKey,
    /// Display label for the session — the title set via the
    /// session-rename flow if any, otherwise a derivation from the
    /// session id or first message. Phase 2 surfaces this in the
    /// Projects pane.
    pub label: String,
    /// `true` when an Agent for this session is currently in the
    /// workspace pool.
    pub is_open: bool,
    pub last_activity: Option<SystemTime>,
}
