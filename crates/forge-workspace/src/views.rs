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
    /// Name of the org this project belongs to (from
    /// `[[orgs]].name` in `forge.toml`). Drives the org-grouping
    /// in the Projects pane tree render.
    pub org: String,
    /// Filesystem-resolved project root (`~` expanded). This is the
    /// path callers should hand to filesystem APIs — `cwd_raw` for
    /// the spawning bucket, `file_index::restart`, the git-context
    /// watcher, etc. Use [`Self::display_path`] for human-readable
    /// rendering instead.
    pub path: PathBuf,
    /// Human-readable rendering of the project's root path (e.g.
    /// `~/Projects/forge`, with `~` left in place rather than
    /// expanded). Display-only — not a path you can `open()`.
    pub display_path: String,
    /// Account `display_name`s this project may spawn under, inherited
    /// from the project's `[[orgs]]` entry. Non-empty (the config
    /// loader enforces). The launchpad picker reads the first entry
    /// as the row's account hint via [`Self::primary_account_hint`].
    pub accounts: Vec<String>,
    pub sessions: Vec<SessionView>,
}

impl ProjectView {
    /// Lowercased first allowed account — the "account hint" rendered
    /// as a dim column in the launchpad picker (e.g. `(personal)`,
    /// `(granite)`). The actual spawn picker resolves which account
    /// the session lands under at the moment of spawn; this is just a
    /// visual cue for the user.
    ///
    /// Empty accounts → `"unknown"`.
    pub fn primary_account_hint(&self) -> String {
        self.accounts.first().map_or_else(|| "unknown".to_owned(), |a| a.to_lowercase())
    }
}

#[cfg(feature = "test-helpers")]
impl ProjectView {
    /// Test-only constructor for cross-crate fixtures (forge-tui's
    /// Projects pane snapshot tests). Behind the `test-helpers`
    /// Cargo feature to keep test-only construction out of the
    /// production API.
    pub fn new_for_test(
        key: ProjectKey,
        name: impl Into<String>,
        display_path: impl Into<String>,
        sessions: Vec<SessionView>,
    ) -> Self {
        let display_path = display_path.into();
        Self {
            key,
            name: name.into(),
            org: "Test".to_owned(),
            path: PathBuf::from(&display_path),
            display_path,
            accounts: Vec::new(),
            sessions,
        }
    }

    /// Variant of [`Self::new_for_test`] that lets the fixture
    /// supply an org + accounts list — needed for launchpad picker
    /// snapshot tests where the account hint column reads from
    /// `accounts[0]`.
    pub fn new_for_test_with_org(
        key: ProjectKey,
        name: impl Into<String>,
        display_path: impl Into<String>,
        org: impl Into<String>,
        accounts: Vec<String>,
        sessions: Vec<SessionView>,
    ) -> Self {
        let display_path = display_path.into();
        Self {
            key,
            name: name.into(),
            org: org.into(),
            path: PathBuf::from(&display_path),
            display_path,
            accounts,
            sessions,
        }
    }
}

/// One session under a project.
#[derive(Clone, Debug)]
pub struct SessionView {
    pub session: SessionKey,
    /// Display label for the session — the title set via the
    /// session-rename flow if any, otherwise a derivation from the
    /// session id or first message. Rendered in the Projects pane.
    pub label: String,
    /// `true` when an Agent for this session is currently in the
    /// workspace pool.
    pub is_open: bool,
    pub last_activity: Option<SystemTime>,
}

#[cfg(feature = "test-helpers")]
impl SessionView {
    /// Test-only constructor for cross-crate fixtures.
    pub fn new_for_test(
        session: SessionKey,
        label: impl Into<String>,
        is_open: bool,
        last_activity: Option<SystemTime>,
    ) -> Self {
        Self { session, label: label.into(), is_open, last_activity }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::ProjectKey;

    #[test]
    fn primary_account_hint_lowercases_first_account() {
        let view = ProjectView {
            key: ProjectKey::new("test".to_owned()),
            name: "forge".to_owned(),
            org: "Busytools".to_owned(),
            path: PathBuf::from("/tmp/forge"),
            display_path: "~/Projects/forge".to_owned(),
            accounts: vec!["Personal".to_owned(), "Granite".to_owned()],
            sessions: Vec::new(),
        };
        assert_eq!(view.primary_account_hint(), "personal");
    }

    #[test]
    fn primary_account_hint_returns_unknown_when_empty() {
        let view = ProjectView {
            key: ProjectKey::new("test".to_owned()),
            name: "forge".to_owned(),
            org: "Busytools".to_owned(),
            path: PathBuf::from("/tmp/forge"),
            display_path: "~/Projects/forge".to_owned(),
            accounts: Vec::new(),
            sessions: Vec::new(),
        };
        assert_eq!(view.primary_account_hint(), "unknown");
    }
}
