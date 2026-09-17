//! Read-only views surfaced by [`crate::Workspace::list_projects`].

use std::path::PathBuf;
use std::time::SystemTime;

use crate::target::ProjectKey;

pub use forge_primitives::usage::AccountBudget;

/// One project from the catalog plus its sessions, sorted last-
/// activity descending. `sessions[0]` is the lead. Empty `sessions`
/// means the project has no on-disk history yet.
#[derive(Debug)]
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
    /// path callers should hand to filesystem APIs - `cwd_raw` for
    /// the spawning bucket, `file_index::restart`, the git-context
    /// watcher, etc. Use [`Self::display_path`] for human-readable
    /// rendering instead.
    pub path: PathBuf,
    /// Human-readable rendering of the project's root path (e.g.
    /// `~/Projects/forge`, with `~` left in place rather than
    /// expanded). Display-only - not a path you can `open()`.
    pub display_path: String,
    /// Account `display_name`s this project may spawn under, inherited
    /// from the project's `[[orgs]]` entry. Non-empty (the config
    /// loader enforces).
    pub accounts: Vec<String>,
    /// Fallback `display_name`s inherited from the project's `[[orgs]]`
    /// entry, alongside `accounts`. Empty when the org names none.
    pub fallback_accounts: Vec<String>,
    /// `true` when the project declares `model`. A spawn under a
    /// project without one is refused, so the launchpad's row needs to
    /// tell the two reasons no spawn can run apart: no model to match
    /// on, or no account that could serve it.
    pub has_model: bool,
    pub sessions: Vec<SessionView>,
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
            fallback_accounts: Vec::new(),
            has_model: true,
            sessions,
        }
    }

    /// Variant of [`Self::new_for_test`] that lets the fixture
    /// supply an org + accounts list - needed for launchpad picker
    /// snapshot tests where the account hint column reads from
    /// `accounts[0]`.
    pub fn new_for_test_with_org(
        key: ProjectKey,
        name: impl Into<String>,
        display_path: impl Into<String>,
        org: impl Into<String>,
        accounts: Vec<String>,
        fallback_accounts: Vec<String>,
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
            fallback_accounts,
            has_model: true,
            sessions,
        }
    }
}

/// One account row for the account snapshots: a project-allowed
/// account plus its live rate-limit state, snapshotted so the TUI
/// renders without locking `AccountStateMap`. Produced by
/// [`crate::Workspace::project_accounts_snapshot`] in allow-list order.
#[derive(Clone, Debug)]
pub struct AccountRow {
    /// forge.toml `[[accounts]]` display name.
    pub display_name: String,
    /// `None` when the account is pickable now (not saturated, not bailed).
    /// The reason renders as the row's status tag: a capped window
    /// reads `limit hit`, a blocked probe or a bail reads
    /// `auth failed or expired`.
    pub unusable: Option<forge_gateway::Unusable>,
    /// What this account has left, in whatever terms its backend bills.
    pub budget: AccountBudget,
    /// `true` when the account is in the org's `fallback_accounts` only.
    /// Reads after every primary row, carrying a dim `fallback` suffix.
    pub fallback: bool,
    /// The provider whose backend serves this account.
    pub provider: forge_primitives::account::Provider,
    /// Boot-time loading state, as the launchpad gates on it.
    pub loading: forge_gateway::LoadingState,
}

/// One org's block in the read-only gateway view: its walk order and
/// the live state of every account it names.
#[derive(Debug, Clone)]
pub struct GatewayOrgView {
    /// The `[[orgs]].name` from `forge.toml`.
    pub org: String,
    /// The org's primary pin, in walk order.
    pub accounts: Vec<String>,
    /// The org's fallback pin, in walk order.
    pub fallback_accounts: Vec<String>,
    /// One row per account the org names, primaries first.
    pub rows: Vec<AccountRow>,
}

// The account auth classification is returned by the gateway's account
// state, so it lives in forge-primitives now. Re-exported here so
// `forge_workspace::AccountAuth` and `crate::views::AccountAuth` keep
// resolving.
pub use forge_primitives::account::AccountAuth;

/// One account's place in preflight: what it is called, how far it
/// has got, and how it authenticates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountLoadingRow {
    /// forge.toml `[[accounts]]` display name.
    pub display_name: String,
    pub state: forge_gateway::LoadingState,
    /// The classified outcome of the last failed probe attempt. What
    /// lets a bailed row say `unreachable` when the endpoint is simply
    /// down rather than `auth failed`.
    pub last_error: Option<forge_gateway::UsageFetchStatus>,
    /// Remaining hold-down before the pollers re-probe a failed
    /// account - the server `Retry-After` for a 429, the exponential
    /// schedule otherwise. `None` when nothing is scheduled.
    pub retry_after: Option<std::time::Duration>,
    /// Which repair instruction a bailed row earns.
    pub auth: AccountAuth,
}

/// One session under a project.
#[derive(Clone, Debug)]
pub struct SessionView {
    /// The claude session id the catalog row names, taken from the
    /// transcript file on disk. A row here is a transcript rather than
    /// one of forge's slots, so this stays an id - the same exemption
    /// transcript filenames themselves have.
    pub session: forge_primitives::SessionId,
    /// Display label for the session - the title set via the
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
        session: forge_primitives::SessionId,
        label: impl Into<String>,
        is_open: bool,
        last_activity: Option<SystemTime>,
    ) -> Self {
        Self { session, label: label.into(), is_open, last_activity }
    }
}
