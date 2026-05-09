//! The orchestrator.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use forge_agent::AgentHandle;
use forge_agent::client::SessionLaunchSettings;
use forge_primitives::SDKSessionInfo;
use parking_lot::Mutex;

use crate::account::{AccountKey, AccountStateMap};
use crate::config::{LoadedConfig, LoadedProject, load_from_dir};
use crate::error::WorkspaceError;
use crate::state::{self, PersistedAccountState, PersistedSelectionState, PersistedState};
use crate::target::{ProjectKey, SessionKey, SessionTarget};
use crate::views::{ProjectView, SessionView};

/// Multi-session orchestrator. Owns the project catalog snapshot
/// loaded from `<config_dir>/forge.toml` and the pool of currently
/// spawned [`forge_agent::Agent`] handles, one per active session.
///
/// Construct via [`Workspace::new`]; consume via
/// [`Workspace::get_agent_handle`]; drain on exit via
/// [`Workspace::shutdown`]. See spec at
/// `~/.claude-subspace/plans/2026-05-09-forge-tui-phase-1a-workspace-design.md`
/// for the full contract.
pub struct Workspace {
    /// Retained so error messages can reference the `forge.toml`
    /// path the workspace was constructed from, and so Phase 1b's
    /// per-account config-dir binding can re-resolve from here.
    config_dir: PathBuf,
    config: LoadedConfig,
    /// Catalog snapshot from `userdata::catalog::scan::list_sessions`,
    /// grouped by project key. Read-only after `new` returns.
    catalog: HashMap<ProjectKey, Vec<SDKSessionInfo>>,
    /// Live Agents keyed by session id. `parking_lot::Mutex` so the
    /// public methods can take `&self`.
    pool: Mutex<HashMap<SessionKey, PooledAgent>>,
    /// Account picker state. Updated on every spawn; persisted to
    /// `forge-state.toml` after each pick.
    accounts: Mutex<AccountStateMap>,
}

/// Pool entry wrapping the live `Arc<AgentHandle>` together with
/// the account it was spawned against. The account binding is
/// retained so Phase 4+ can surface "which credential pool is this
/// session running on" in the UI, and so tests can verify the
/// picker's choice round-trips through the pool.
#[derive(Clone)]
pub(crate) struct PooledAgent {
    pub handle: Arc<AgentHandle>,
    /// Which account this session is bound to. Phase 1b uses this
    /// for test-only inspection; Phase 4+ surfaces it.
    #[allow(dead_code)] // surfaced in Phase 4+; tests read via pool_accounts_for_test
    pub account: AccountKey,
}

impl Workspace {
    /// Builds a Workspace, runs the catalog scan, and loads
    /// `<config_dir>/forge.toml`. Errors if `forge.toml` is missing,
    /// malformed, or has no project marked `default = true`. No
    /// Agents are spawned on success.
    pub async fn new(config_dir: PathBuf) -> Result<Self, WorkspaceError> {
        let config = load_from_dir(&config_dir)?;

        // Catalog scan honours `$CLAUDE_CONFIG_DIR` from the process
        // environment rather than `config_dir` — `forge_agent::userdata::
        // catalog::scan::list_sessions` resolves via `forge_sdk::projects_dir`.
        // In production this is fine: the launching profile's env matches
        // the config_dir we received. For test isolation it leaks the
        // developer's real catalog, which is why the existing tests assert
        // only properties (is_open: false, project list shape) that don't
        // depend on catalog content.
        let catalog_entries = forge_agent::userdata::catalog::scan::list_sessions(
            None, // every project in the catalog
            None, // no limit
            0,
        )
        .await;

        // Group sessions by project key derived from each session's cwd.
        // Sessions without a cwd are skipped — they can't be associated
        // with a project view.
        let mut catalog: HashMap<ProjectKey, Vec<SDKSessionInfo>> = HashMap::new();
        for entry in catalog_entries {
            if let Some(cwd) = entry.cwd.as_deref() {
                let key = ProjectKey::new(
                    forge_agent::userdata::catalog::scan::project_key_for_directory(Some(cwd)),
                );
                catalog.entry(key).or_default().push(entry);
            }
        }
        // The catalog scan returns entries sorted by `last_modified`
        // descending; the per-project Vec inherits that ordering thanks
        // to push order being preserved.

        // Load picker state from forge-state.toml (missing/malformed
        // → empty defaults; never blocks startup) and seed
        // `AccountStateMap` from the account list parsed out of
        // `forge.toml`. Phase 1b's `get_agent_handle` consults this
        // map on every spawn; the chosen account's `config_dir`
        // becomes the spawn's `CLAUDE_CONFIG_DIR` override.
        let persisted = state::load_or_default(&config_dir);
        let persisted_last_used: HashMap<String, Option<SystemTime>> = persisted
            .accounts
            .iter()
            .map(|(name, acct)| {
                let parsed = acct.last_used_at.as_deref().and_then(parse_rfc3339);
                (name.clone(), parsed)
            })
            .collect();

        let accounts = AccountStateMap::new(
            &config.accounts,
            config.selection.policy.clone(),
            persisted.selection.round_robin_next,
            &persisted_last_used,
        );

        Ok(Self {
            config_dir,
            config,
            catalog,
            pool: Mutex::new(HashMap::new()),
            accounts: Mutex::new(accounts),
        })
    }

    /// Every project listed in `forge.toml`, each carrying its catalog
    /// sessions sorted by last-activity descending — `sessions[0]` is
    /// the lead. Empty `sessions` means the project has nothing on disk
    /// yet; the project still surfaces in the returned Vec.
    #[must_use]
    pub fn list_projects(&self) -> Vec<ProjectView> {
        let open_sessions: std::collections::HashSet<SessionKey> =
            self.pool.lock().keys().cloned().collect();

        let mut views = Vec::with_capacity(self.config.projects.len());
        for project in &self.config.projects {
            let key =
                ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(
                    Some(&project.path.to_string_lossy()),
                ));

            let sessions: Vec<SessionView> = self
                .catalog
                .get(&key)
                .map(|entries| {
                    entries
                        .iter()
                        .map(|info| {
                            let session = SessionKey::new(info.session_id.clone());
                            SessionView {
                                session: session.clone(),
                                label: info.summary.clone(),
                                is_open: open_sessions.contains(&session),
                                last_activity: Some(
                                    UNIX_EPOCH + Duration::from_millis(info.last_modified),
                                ),
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();

            views.push(ProjectView { key, display_path: project.display_path.clone(), sessions });
        }
        views
    }

    /// Validate that `name` matches a project in `forge.toml`. Used
    /// by callers (e.g. forge-tui's main) to fail fast on an unknown
    /// CLI positional arg before TUI setup, rather than surface the
    /// same error later via [`Self::get_agent_handle`].
    pub fn validate_project_name(&self, name: &str) -> Result<(), WorkspaceError> {
        self.find_project_by_name(name).map(|_| ())
    }

    /// Hands out the `Arc<AgentHandle>` for the requested session,
    /// spawning the underlying Agent lazily if it isn't already pooled.
    /// Idempotent — repeated calls for the same target return the same
    /// handle (no second subprocess). `settings` only apply to the
    /// spawn; subsequent calls reuse the existing Agent and ignore the
    /// parameter.
    ///
    /// Each fresh spawn consults the account picker (LRU or
    /// round-robin per `forge.toml`) and exports
    /// `CLAUDE_CONFIG_DIR` to the spawned `claude` subprocess so it
    /// reads/writes the picked account's config dir. Picker state is
    /// persisted to `forge-state.toml` after every pick.
    ///
    /// Workspace does not track which handle the caller is "using" —
    /// that's the caller's concern.
    ///
    /// `async` is mandated by the spec contract — Phase 2+ work
    /// (live-account refresh, oauth probing) will need to await
    /// before the picker decision lands. Phase 1b's body is
    /// synchronous; the `unused_async` allow keeps the contract.
    #[allow(clippy::unused_async)]
    pub async fn get_agent_handle(
        &self,
        target: SessionTarget,
        settings: SessionLaunchSettings,
    ) -> Result<Arc<AgentHandle>> {
        let session_key = self.resolve_target(&target)?;

        // Fast path: cache hit
        {
            let pool = self.pool.lock();
            if let Some(existing) = pool.get(&session_key) {
                return Ok(Arc::clone(&existing.handle));
            }
        }

        // Pick an account via policy. Holds accounts lock briefly;
        // released before we touch disk in `persist_account_state`.
        let (account_key, account_dir) = {
            let mut accounts = self.accounts.lock();
            let now = SystemTime::now();
            accounts.pick_next(now)
        };

        // Persist forge-state.toml AFTER releasing the accounts
        // lock. `state::save` is best-effort — it logs and returns
        // on failure rather than propagating, so a transient I/O
        // hiccup doesn't break the spawn path.
        self.persist_account_state();

        // Build per-spawn env override. The spawned `claude`
        // subprocess reads `CLAUDE_CONFIG_DIR` to decide which
        // user-data tree (oauth tokens, projects history, settings)
        // to use, so each session can be bound to a different
        // account on the same workstation.
        let mut extra_env = std::collections::HashMap::new();
        extra_env.insert("CLAUDE_CONFIG_DIR".to_owned(), account_dir.to_string_lossy().to_string());

        // Slow path: spawn fresh Agent and dispatch the start command.
        let handle = forge_agent::Agent::spawn_with_env(extra_env);
        match &target {
            SessionTarget::Default => {
                let cwd = self.config.default_project().path.to_string_lossy().to_string();
                handle.new_session(cwd, settings)?;
            }
            SessionTarget::Named(name) => {
                let project = self.find_project_by_name(name)?;
                let cwd = project.path.to_string_lossy().to_string();
                handle.new_session(cwd, settings)?;
            }
            SessionTarget::Session(key) => {
                handle.resume_session(key.as_str().to_owned(), settings)?;
            }
        }

        let arc = Arc::new(handle);

        // Insert: race-safe via "if absent" semantics. If a concurrent
        // caller raced us to the spawn, theirs wins the pool slot and
        // ours drops at end-of-scope (subprocess killed via Client's
        // existing Drop). Acceptable for single-user scope; forge-tui's
        // startup is the only caller in 1a and never races itself.
        {
            let mut pool = self.pool.lock();
            if let Some(existing) = pool.get(&session_key) {
                return Ok(Arc::clone(&existing.handle));
            }
            pool.insert(
                session_key,
                PooledAgent { handle: Arc::clone(&arc), account: account_key },
            );
        }

        Ok(arc)
    }

    /// Snapshot the current `AccountStateMap` into a
    /// [`PersistedState`] and write it to `forge-state.toml`.
    /// Best-effort — `state::save` logs and returns on failure so
    /// a transient write error never propagates back into the
    /// spawn path.
    fn persist_account_state(&self) {
        let snapshot = {
            let accounts = self.accounts.lock();
            let mut persisted_accounts: HashMap<String, PersistedAccountState> = HashMap::new();
            for (key, state) in &accounts.by_key {
                persisted_accounts.insert(
                    key.0.clone(),
                    PersistedAccountState { last_used_at: state.last_used_at.map(format_rfc3339) },
                );
            }
            // Only persist `round_robin_next` when the policy is
            // round-robin; LRU sessions don't track a cursor and
            // serialising a stale value would mislead a future
            // policy switch.
            let round_robin_next =
                matches!(accounts.policy, crate::config::SelectionPolicy::RoundRobin)
                    .then_some(accounts.round_robin_next);
            PersistedState {
                accounts: persisted_accounts,
                selection: PersistedSelectionState { round_robin_next },
            }
        };
        state::save(&self.config_dir, &snapshot);
    }

    /// Resolves a `SessionTarget` to the `SessionKey` used to look up
    /// the pool. For project-rooted targets (`Default` / `Named`) with
    /// no on-disk session for the project, returns a project-keyed
    /// placeholder (`__fresh__:<project_key>`) so re-entry stays
    /// idempotent against the same fresh-session intent. For `Named`
    /// with no matching project, returns
    /// [`WorkspaceError::ProjectNotFound`].
    fn resolve_target(&self, target: &SessionTarget) -> Result<SessionKey, WorkspaceError> {
        match target {
            SessionTarget::Default => Ok(self.lead_session_key_for(self.config.default_project())),
            SessionTarget::Named(name) => {
                let project = self.find_project_by_name(name)?;
                Ok(self.lead_session_key_for(project))
            }
            SessionTarget::Session(key) => Ok(key.clone()),
        }
    }

    /// Look up a project by `name` from `forge.toml`. Returns
    /// [`WorkspaceError::ProjectNotFound`] when no project carries
    /// that name.
    fn find_project_by_name(&self, name: &str) -> Result<&LoadedProject, WorkspaceError> {
        self.config.projects.iter().find(|project| project.name == name).ok_or_else(|| {
            WorkspaceError::ProjectNotFound {
                name: name.to_owned(),
                path: self.config_dir.join("forge.toml"),
            }
        })
    }

    /// Map a project to the `SessionKey` of its lead (most-recent)
    /// session, or to a `__fresh__:<project_key>` placeholder when the
    /// project has nothing on disk yet.
    fn lead_session_key_for(&self, project: &LoadedProject) -> SessionKey {
        let key = ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(
            Some(&project.path.to_string_lossy()),
        ));
        if let Some(entries) = self.catalog.get(&key)
            && let Some(lead) = entries.first()
        {
            return SessionKey::new(lead.session_id.clone());
        }
        SessionKey::new(format!("__fresh__:{}", key.as_str()))
    }

    /// Graceful shutdown of every pooled Agent. Drains the pool, then
    /// drops each `Arc<AgentHandle>` so the underlying
    /// `forge_sdk::Client` kills its `claude` subprocess via its
    /// existing `Drop` impl when the last reference goes away.
    ///
    /// In 1a forge-tui drops its handle reference before calling
    /// shutdown, so Workspace is the sole owner of every pool entry
    /// and dropping it triggers the subprocess shutdown chain (sender
    /// drop -> dispatcher exit -> Client drop -> subprocess
    /// kill_on_drop). Phase 2+ callers that hold cloned handles
    /// across shutdown will need to release them for the kill-chain
    /// to fire promptly. Synchronous and fast in 1a; the async
    /// signature is preserved so a future "send shutdown signal,
    /// await acknowledgement" body can slot in without restructuring
    /// the call sites.
    #[allow(clippy::unused_async)]
    pub async fn shutdown(self) {
        let entries: Vec<_> = self.pool.lock().drain().collect();
        // Each (SessionKey, PooledAgent) drops here; the
        // subprocess teardown chain (sender drop -> dispatcher exit
        // -> Client drop -> subprocess kill_on_drop) is synchronous
        // and fast in 1a.
        drop(entries);
    }
}

#[cfg(test)]
impl Workspace {
    pub fn pool_len_for_test(&self) -> usize {
        self.pool.lock().len()
    }

    pub fn pool_accounts_for_test(&self) -> Vec<String> {
        self.pool.lock().values().map(|p| p.account.0.clone()).collect()
    }
}

/// Parse an RFC 3339 timestamp into a [`SystemTime`]. Returns
/// `None` on parse error so callers can fall back to "never used".
/// Negative epochs (timestamps before 1970-01-01) are clamped to
/// `UNIX_EPOCH` rather than rejected — they're nonsensical for the
/// "last used" field but shouldn't block startup.
fn parse_rfc3339(s: &str) -> Option<SystemTime> {
    use time::OffsetDateTime;
    OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok().map(|odt| {
        let nanos = odt.unix_timestamp_nanos();
        if nanos < 0 {
            SystemTime::UNIX_EPOCH
        } else {
            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
            let nanos_u64 = nanos as u64;
            SystemTime::UNIX_EPOCH + std::time::Duration::from_nanos(nanos_u64)
        }
    })
}

/// Format a [`SystemTime`] as an RFC 3339 UTC string. Failure
/// (extreme out-of-range values) yields an empty string rather
/// than panicking — the persisted file stays well-formed even if
/// a single field misformats.
fn format_rfc3339(t: SystemTime) -> String {
    use time::OffsetDateTime;
    let odt: OffsetDateTime = t.into();
    odt.format(&time::format_description::well_known::Rfc3339).unwrap_or_default()
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn make_workspace_dir() -> tempfile::TempDir {
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join("forge.toml"),
            r#"
[[projects]]
name = "forge"
path = "~/Projects/forge"
default = true

[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"
"#,
        )
        .expect("write forge.toml");
        dir
    }

    #[tokio::test]
    async fn get_agent_handle_default_is_idempotent() {
        let dir = make_workspace_dir();
        let workspace = Workspace::new(dir.path().to_owned()).await.expect("new");
        let settings = SessionLaunchSettings::default();

        let handle1 = workspace
            .get_agent_handle(SessionTarget::Default, settings.clone())
            .await
            .expect("first");
        let handle2 =
            workspace.get_agent_handle(SessionTarget::Default, settings).await.expect("second");

        assert!(Arc::ptr_eq(&handle1, &handle2), "expected pool hit for repeated Default target");
        assert_eq!(workspace.pool.lock().len(), 1);
    }

    #[tokio::test]
    async fn distinct_targets_pool_distinct_entries() {
        let dir = make_workspace_dir();
        let workspace = Workspace::new(dir.path().to_owned()).await.expect("new");
        let settings = SessionLaunchSettings::default();

        let _ = workspace
            .get_agent_handle(SessionTarget::Default, settings.clone())
            .await
            .expect("default");
        let _ = workspace
            .get_agent_handle(SessionTarget::Default, settings.clone())
            .await
            .expect("default again");
        assert_eq!(workspace.pool.lock().len(), 1, "Default is idempotent");

        let other = SessionKey::from_str_for_test("dual-test-other");
        let _ = workspace
            .get_agent_handle(SessionTarget::Session(other), settings)
            .await
            .expect("session");
        assert_eq!(workspace.pool.lock().len(), 2, "distinct target adds a pool entry");
    }

    #[tokio::test]
    async fn shutdown_drains_pool() {
        let dir = make_workspace_dir();
        let workspace = Workspace::new(dir.path().to_owned()).await.expect("new");
        let handle = workspace
            .get_agent_handle(SessionTarget::Default, SessionLaunchSettings::default())
            .await
            .expect("default");

        // Pool has one entry going in.
        assert_eq!(workspace.pool.lock().len(), 1);
        // Workspace + this test both hold the Arc → strong_count == 2.
        assert_eq!(Arc::strong_count(&handle), 2);

        // Shutdown consumes self and must return.
        workspace.shutdown().await;

        // After shutdown, only our local clone remains. If shutdown
        // didn't actually release Workspace's reference, this fails.
        assert_eq!(Arc::strong_count(&handle), 1);
    }

    #[tokio::test]
    async fn get_agent_handle_named_project_resolves() {
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join("forge.toml"),
            r#"
[[projects]]
name = "forge"
path = "~/Projects/forge"
default = true

[[projects]]
name = "dotfiles"
path = "~/Projects/dotfiles"

[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"
"#,
        )
        .expect("write forge.toml");

        let workspace = Workspace::new(dir.path().to_owned()).await.expect("new");
        let _ = workspace
            .get_agent_handle(SessionTarget::Default, SessionLaunchSettings::default())
            .await
            .expect("default");
        let _ = workspace
            .get_agent_handle(
                SessionTarget::Named("dotfiles".to_owned()),
                SessionLaunchSettings::default(),
            )
            .await
            .expect("named");
        assert_eq!(workspace.pool.lock().len(), 2);
    }

    #[tokio::test]
    async fn get_agent_handle_named_unknown_errors() {
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join("forge.toml"),
            r#"
[[projects]]
name = "forge"
path = "~/Projects/forge"
default = true

[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"
"#,
        )
        .expect("write forge.toml");

        let workspace = Workspace::new(dir.path().to_owned()).await.expect("new");
        let result = workspace
            .get_agent_handle(
                SessionTarget::Named("nonexistent".to_owned()),
                SessionLaunchSettings::default(),
            )
            .await;
        let Err(err) = result else { panic!("unknown project name should error") };
        let err_string = format!("{err}");
        assert!(
            err_string.contains("nonexistent"),
            "error should mention the project name; got: {err_string}"
        );
    }

    fn make_workspace_dir_with_two_accounts() -> tempfile::TempDir {
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join("forge.toml"),
            r#"
[[projects]]
name = "forge"
path = "~/Projects/forge"
default = true

[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"

[[accounts]]
display_name = "Granite"
config_dir = "~/.claude-granite"
"#,
        )
        .expect("write forge.toml");
        dir
    }

    #[tokio::test]
    async fn pool_records_picked_account() {
        let dir = make_workspace_dir_with_two_accounts();
        let workspace = Workspace::new(dir.path().to_owned()).await.expect("new");
        let _ = workspace
            .get_agent_handle(SessionTarget::Default, SessionLaunchSettings::default())
            .await
            .expect("default");
        let bound = workspace.pool_accounts_for_test();
        assert_eq!(bound.len(), 1);
        // LRU tie-break is alphabetical on never-used: Granite < Subspace.
        assert_eq!(bound[0], "Granite");
    }

    #[tokio::test]
    async fn second_spawn_picks_other_account_under_lru() {
        let dir = make_workspace_dir_with_two_accounts();
        let workspace = Workspace::new(dir.path().to_owned()).await.expect("new");

        // First spawn → Granite (alphabetical tie-break).
        let _ = workspace
            .get_agent_handle(SessionTarget::Default, SessionLaunchSettings::default())
            .await
            .expect("first");

        // Second spawn (different target so pool key differs) → Subspace
        // (Granite was just used, so it's no longer LRU).
        let other = SessionKey::from_str_for_test("dual-account-test-other");
        let _ = workspace
            .get_agent_handle(SessionTarget::Session(other), SessionLaunchSettings::default())
            .await
            .expect("second");

        let bound = workspace.pool_accounts_for_test();
        assert_eq!(bound.len(), 2);
        assert!(bound.contains(&"Granite".to_owned()));
        assert!(bound.contains(&"Subspace".to_owned()));
    }

    #[tokio::test]
    async fn forge_state_toml_persists_after_spawn() {
        let dir = make_workspace_dir_with_two_accounts();
        let workspace = Workspace::new(dir.path().to_owned()).await.expect("new");
        let _ = workspace
            .get_agent_handle(SessionTarget::Default, SessionLaunchSettings::default())
            .await
            .expect("spawn");

        // forge-state.toml should now exist with last_used_at populated.
        let state_path = dir.path().join("forge-state.toml");
        let content = std::fs::read_to_string(&state_path).expect("state file written");
        assert!(content.contains("last_used_at"));
        assert!(content.contains("Granite") || content.contains("Subspace"));
    }
}
