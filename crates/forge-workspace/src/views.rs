//! Read-only views surfaced by [`crate::Workspace::list_projects`].

use std::path::PathBuf;
use std::time::SystemTime;

use crate::target::{ProjectKey, SessionKey};

/// One project from the catalog plus its sessions, sorted last-
/// activity descending. `sessions[0]` is the lead. Empty `sessions`
/// means the project has no on-disk history yet.
#[derive(Clone, Debug)]
pub struct ProjectView {
    pub key: ProjectKey,
    /// The toml `name` field from `forge.toml`. Distinct from `key`,
    /// which is the canonicalised on-disk project key derived from
    /// the project's path. Callers wanting to address a project via
    /// [`crate::SessionTarget::Named`] use this value; callers
    /// keying a HashMap of in-process Agent handles use [`Self::key`].
    pub name: String,
    /// Filesystem-resolved project root (`~` expanded). This is the
    /// path callers should hand to filesystem APIs — `cwd_raw` for
    /// the spawning bucket, `file_index::restart`,
    /// `trust::store::normalize_project_key`, the git-context
    /// watcher, etc. Use [`Self::display_path`] for human-readable
    /// rendering instead.
    pub path: PathBuf,
    /// Human-readable rendering of the project's root path (e.g.
    /// `~/Projects/forge`, with `~` left in place rather than
    /// expanded). Display-only — not a path you can `open()`.
    pub display_path: String,
    pub sessions: Vec<SessionView>,
}

#[cfg(feature = "test-helpers")]
impl ProjectView {
    /// Test-only constructor for cross-crate fixtures (forge-tui's
    /// Projects pane snapshot tests). Behind the `test-helpers`
    /// Cargo feature to keep test-only construction out of the
    /// production API.
    #[must_use]
    pub fn new_for_test(
        key: ProjectKey,
        name: impl Into<String>,
        display_path: impl Into<String>,
        sessions: Vec<SessionView>,
    ) -> Self {
        let display_path = display_path.into();
        Self { key, name: name.into(), path: PathBuf::from(&display_path), display_path, sessions }
    }
}

/// One session under a project.
#[derive(Clone, Debug)]
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

#[cfg(feature = "test-helpers")]
impl SessionView {
    /// Test-only constructor for cross-crate fixtures.
    #[must_use]
    pub fn new_for_test(
        session: SessionKey,
        label: impl Into<String>,
        is_open: bool,
        last_activity: Option<SystemTime>,
    ) -> Self {
        Self { session, label: label.into(), is_open, last_activity }
    }
}
