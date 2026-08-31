//! Read-only views surfaced by [`crate::Workspace::list_projects`].

use std::path::PathBuf;
use std::time::SystemTime;

use crate::target::{ProjectKey, SessionKey};

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

/// One account row for the `/account` picker: a project-allowed
/// account plus its live rate-limit state, snapshotted so the TUI
/// renders without locking `AccountStateMap`. Produced by
/// [`crate::Workspace::project_accounts_snapshot`] in allow-list order.
#[derive(Clone, Debug)]
pub struct AccountRow {
    /// forge.toml `[[accounts]]` display name.
    pub display_name: String,
    /// On-disk config dir seeding `CLAUDE_CONFIG_DIR` for this account.
    pub config_dir: PathBuf,
    /// `true` when this is the session's active account.
    pub is_current: bool,
    /// `true` when the account is pickable now (tier-0, not bailed).
    /// `false` renders the red `rate limited` tag.
    pub usable: bool,
    /// What this account has left, in whatever terms its backend bills.
    pub budget: AccountBudget,
    /// `true` for an `experimental = true` account. The picker renders
    /// these in a separate `EXPERIMENTAL` group with an amber tag; they
    /// are offered globally (regardless of the project's org pin)
    /// because they are excluded from every auto-assignment path.
    pub experimental: bool,
}

/// What an account has left, in the terms its backend bills in.
///
/// Lives on the view rather than on `UsageSnapshot` because the
/// snapshot is persisted and this is not: making the stored type an
/// enum would break every cached row, while a view type can carry the
/// discrimination for free.
///
/// `Unknown` is load-bearing, not a placeholder. Before it existed a
/// missing snapshot collapsed to `(0.0, 0.0)`, so "never probed" and
/// "probed, nothing used" rendered identically as a green 0% - the same
/// class of invented number as a fabricated percentage on an account
/// with no windows.
#[derive(Clone, Debug, PartialEq)]
pub enum AccountBudget {
    /// No usable snapshot: none has landed yet, or the cached one was
    /// written under a different `provider` and no longer describes
    /// this account.
    Unknown,
    /// Plan windows, as percentages of an allowance that resets.
    Subscription {
        five_hour_util: f64,
        /// Binding 7-day utilization: max across the three 7-day windows.
        seven_day_util: f64,
        /// When the account unlocks - `Some` only while it is at its
        /// cap, so the picker shows a reset ETA on limited rows only.
        resets_at: Option<SystemTime>,
    },
    /// Per-key spend in USD over the three periods the backend
    /// pre-computes. No allowance, so no percentage and no reset;
    /// account-wide balance has a different scope and is not carried
    /// here, so a row cannot imply both figures are per-key.
    Api { daily: f64, weekly: f64, monthly: f64 },
}

/// How an account proves who it is, which is the only thing that
/// changes what preflight tells you to do about a failed one.
///
/// Deliberately not [`forge_agent::cloud::oauth_usage::ProbePlan`],
/// whose `BaseUrl` variant carries the bearer: this crosses into a view
/// the TUI renders, so it carries the distinction and none of the
/// secret.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccountAuth {
    /// Credentials in the macOS keychain. Repaired with `claude /login`,
    /// and picked up by the 30 s recovery poll.
    Keychain,
    /// `ANTHROPIC_BASE_URL` in the account's `[accounts.env]`, so its
    /// credential is the `ANTHROPIC_AUTH_TOKEN` beside it and there is
    /// no keychain entry for `/login` to write. The recovery poll skips
    /// these; the 60 s usage poll recovers them.
    BaseUrl,
}

/// One account's place in preflight: what it is called, how far it
/// has got, the config dir a failed one has to be fixed in, and how it
/// authenticates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountLoadingRow {
    /// forge.toml `[[accounts]]` display name.
    pub display_name: String,
    pub state: crate::account::LoadingState,
    /// `CLAUDE_CONFIG_DIR` for this account, which is what preflight
    /// puts in the `/login` line when it will not authenticate.
    pub config_dir: std::path::PathBuf,
    /// Which repair instruction a bailed row earns.
    pub auth: AccountAuth,
}

/// One session under a project.
#[derive(Clone, Debug)]
pub struct SessionView {
    pub session: SessionKey,
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
        session: SessionKey,
        label: impl Into<String>,
        is_open: bool,
        last_activity: Option<SystemTime>,
    ) -> Self {
        Self { session, label: label.into(), is_open, last_activity }
    }
}
