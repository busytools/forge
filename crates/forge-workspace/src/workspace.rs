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
use tokio::sync::mpsc;

use crate::account::{AccountKey, AccountStateMap};
use crate::config::{LoadedConfig, LoadedProject, load_from_dir};
use crate::domain_session::DomainSession;
use crate::error::WorkspaceError;
use crate::protocol::{Command, DispatchError, PendingInteractionSlot, SessionUpdate};
use crate::session_task::SessionTask;
use crate::spawn;
use crate::state::{
    self, PersistedAccountState, PersistedSelectionState, PersistedState, PersistedUiState,
};
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
    /// Catalog of sessions per project. Seeded from
    /// `userdata::catalog::scan::list_sessions` at `new`; mutated
    /// in-place by [`Workspace::record_connected_session`] each time a
    /// freshly spawned session reaches `Connected`, so the Projects
    /// pane's drilldown stays current without forcing a full disk
    /// re-scan. Held under a Mutex because multiple in-process tasks
    /// (the pane render, the connect-flow event handler) reach for it
    /// across `await` points.
    catalog: Mutex<HashMap<ProjectKey, Vec<SDKSessionInfo>>>,
    /// Live Agents keyed by session id. `parking_lot::Mutex` so the
    /// public methods can take `&self`.
    pool: Mutex<HashMap<SessionKey, PooledAgent>>,
    /// Account picker state. Updated on every spawn; persisted to
    /// `forge-state.toml` after each pick.
    accounts: Mutex<AccountStateMap>,
    /// In-memory mirror of the `[ui]` section in `forge-state.toml`.
    /// Mutated by [`Workspace::set_projects_pane_visible`] (called
    /// from forge-tui when the user presses Ctrl+B); persisted via
    /// the same `state::save` path the account picker uses, so
    /// `[accounts]` / `[selection]` survive UI-only writes.
    persisted_ui: Mutex<PersistedUiState>,
    /// Fan-in [`SessionUpdate`] sender. Cloned and handed to
    /// `bridge_lifecycle` via [`Self::update_sender`] so the existing
    /// AgentEvent translator can dual-emit `ClientEvent` + `SessionUpdate`
    /// during Phases 1-3. Phase 4 retires the dual-emit pattern when
    /// `SessionTask` owns the AgentHandle event drain.
    update_tx: mpsc::UnboundedSender<SessionUpdate>,
    /// Single-take slot holding the matching receiver. [`Self::subscribe`]
    /// pops it on first call; subsequent calls return `None`.
    update_rx_slot: Mutex<Option<mpsc::UnboundedReceiver<SessionUpdate>>>,
    /// Per-session [`Command`] sender map. Populated when
    /// [`Self::get_agent_handle`] spawns the first `SessionTask` for a
    /// key; cleared on [`Self::release_session`] and [`Self::shutdown`].
    command_senders: Mutex<HashMap<SessionKey, mpsc::UnboundedSender<Command>>>,
    /// Shared [`DomainSession`] handles, one per active `SessionTask`.
    /// [`Self::store_pending_interaction`] writes under the same lock
    /// the `SessionTask` actor uses to read+remove.
    domain_handles: Mutex<HashMap<SessionKey, Arc<Mutex<DomainSession>>>>,
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

        // Catalog scan reads against the workspace's canonical
        // `config_dir` (where forge.toml lives). Phase 1b binds each
        // spawn to its own account `config_dir`, but the catalog
        // listing isn't UI-consumed yet — multi-account catalog
        // merge is deferred to a later phase.
        let catalog_entries = forge_agent::userdata::catalog::scan::list_sessions(
            &config_dir,
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

        let (update_tx, update_rx) = mpsc::unbounded_channel::<SessionUpdate>();
        Ok(Self {
            config_dir,
            config,
            catalog: Mutex::new(catalog),
            pool: Mutex::new(HashMap::new()),
            accounts: Mutex::new(accounts),
            persisted_ui: Mutex::new(persisted.ui),
            update_tx,
            update_rx_slot: Mutex::new(Some(update_rx)),
            command_senders: Mutex::new(HashMap::new()),
            domain_handles: Mutex::new(HashMap::new()),
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

            let catalog = self.catalog.lock();
            let sessions: Vec<SessionView> = catalog
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
            drop(catalog);

            views.push(ProjectView {
                key,
                name: project.name.clone(),
                path: project.path.clone(),
                display_path: project.display_path.clone(),
                sessions,
            });
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
    pub async fn get_agent_handle(
        self: &Arc<Self>,
        target: SessionTarget,
        settings: SessionLaunchSettings,
    ) -> Result<Arc<AgentHandle>> {
        self.get_agent_handle_with_spawn_key(target, settings, None).await
    }

    /// Like [`Self::get_agent_handle`] but threads a synthetic
    /// `spawn_key` onto the spawned `SessionTask`. The first
    /// `AgentEvent::Connected` arriving on the task drives a
    /// `SessionUpdate::KeyRenamed { from: spawn_key, to: real_key }`
    /// emit before the matching `Connected` so TUI re-keys its
    /// `UiSession` map atomically. `None` for re-entrant callers (the
    /// pooled handle path) where no key migration is needed.
    #[allow(clippy::unused_async)]
    pub async fn get_agent_handle_with_spawn_key(
        self: &Arc<Self>,
        target: SessionTarget,
        settings: SessionLaunchSettings,
        spawn_key: Option<SessionKey>,
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
        self.persist_state();

        // Slow path: spawn fresh Agent bound to the picked account's
        // config_dir. The Agent stores it as a typed field; every
        // in-process accessor (oauth, settings, catalog scans) reads
        // it from there, and the spawned `claude` subprocess
        // inherits it as `CLAUDE_CONFIG_DIR` so each session reads/
        // writes the right account's user-data tree.
        let handle = forge_agent::Agent::spawn(account_dir.clone(), Some(account_key.0.clone()));
        // Project-rooted targets (`Default` / `Named`) resume the
        // project's lead session when the on-disk catalog has one,
        // and fall back to a fresh session in that project's cwd
        // otherwise. Keeps the pool key (lead's session id from the
        // catalog) consistent with the running session id when a
        // resume happens — no more stale lead-id pool entries for a
        // freshly spawned session.
        match &target {
            SessionTarget::Default => {
                let project = self.config.default_project();
                let cwd = project.path.to_string_lossy().to_string();
                if let Some(lead) = self.try_lead_session_id_for(project) {
                    handle.resume_or_new_session(lead.as_str().to_owned(), cwd, settings)?;
                } else {
                    handle.new_session(cwd, settings)?;
                }
            }
            SessionTarget::Named(name) => {
                let project = self.find_project_by_name(name)?;
                let cwd = project.path.to_string_lossy().to_string();
                if let Some(lead) = self.try_lead_session_id_for(project) {
                    handle.resume_or_new_session(lead.as_str().to_owned(), cwd, settings)?;
                } else {
                    handle.new_session(cwd, settings)?;
                }
            }
            SessionTarget::Session(key) => {
                // `claude --resume` indexes by project key derived from
                // the subprocess cwd, so we must spawn in the session's
                // original cwd. Source it from the catalog; if the
                // catalog has no record (or no cwd) the session can't
                // be resumed cleanly anyway — pass through and let the
                // bridge surface ConnectionFailed.
                let cwd = self.session_cwd_for(key).unwrap_or_default();
                handle.resume_session(key.as_str().to_owned(), cwd, settings)?;
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
                session_key.clone(),
                PooledAgent { handle: Arc::clone(&arc), account: account_key },
            );
        }

        // Phase 1: spawn the per-session `SessionTask` actor. Idempotent
        // — a second `get_agent_handle` call for the same key reuses the
        // existing task. Insert under the command_senders lock so a
        // concurrent caller that lost the pool race also loses this
        // race (and drops its `cmd_tx` at end-of-scope).
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Command>();
        let needs_spawn = {
            let mut senders = self.command_senders.lock();
            if senders.contains_key(&session_key) {
                false
            } else {
                senders.insert(session_key.clone(), cmd_tx);
                true
            }
        };
        if needs_spawn {
            // If the workspace already holds a pre-spawn domain
            // handle for this key (e.g. registered by
            // `connect::create_app` for the pre-Connect bucket), reuse
            // it and stamp the live `Arc<AgentHandle>` onto its `conn`
            // slot rather than overwriting the entry. The TUI's
            // accessors keep reading from the same `Arc<Mutex<…>>`
            // they were given pre-spawn — no second `domain_session_for`
            // round-trip after the spawn lands.
            let domain = {
                let mut handles = self.domain_handles.lock();
                if let Some(existing) = handles.get(&session_key).cloned() {
                    existing.lock().conn = Some(Arc::clone(&arc));
                    existing
                } else {
                    let fresh = Arc::new(Mutex::new(DomainSession::new(
                        session_key.clone(),
                        Some(Arc::clone(&arc)),
                    )));
                    handles.insert(session_key.clone(), Arc::clone(&fresh));
                    fresh
                }
            };
            let task = SessionTask {
                key: session_key,
                handle: Arc::clone(&arc),
                command_rx: cmd_rx,
                domain,
                update_tx: self.update_tx.clone(),
                spawn_key,
                connected_once: false,
                workspace: Arc::downgrade(self),
            };
            tokio::spawn(task.run());
        }

        Ok(arc)
    }

    /// Read the persisted Projects-pane visibility preference.
    /// Default `true` if `forge-state.toml` is missing, unparseable,
    /// or omits the `[ui]` section.
    #[must_use]
    pub fn projects_pane_visible(&self) -> bool {
        self.persisted_ui.lock().projects_pane_visible
    }

    /// Update + atomically persist the Projects-pane visibility
    /// preference. Writes `forge-state.toml` via the existing
    /// `state::save` atomic-rename path, preserving the
    /// `[accounts]` and `[selection]` sections from the live
    /// `AccountStateMap`. Best-effort — on I/O failure the new
    /// value stays in memory but won't survive restart.
    pub fn set_projects_pane_visible(&self, visible: bool) {
        {
            let mut ui = self.persisted_ui.lock();
            ui.projects_pane_visible = visible;
        }
        self.persist_state();
    }

    /// Read the persisted Inspector-pane visibility preference.
    /// Default `true` if `forge-state.toml` is missing, unparseable,
    /// or omits the field.
    #[must_use]
    pub fn inspector_pane_visible(&self) -> bool {
        self.persisted_ui.lock().inspector_pane_visible
    }

    /// Update + atomically persist the Inspector-pane visibility
    /// preference. Same persistence path as
    /// [`Self::set_projects_pane_visible`].
    pub fn set_inspector_pane_visible(&self, visible: bool) {
        {
            let mut ui = self.persisted_ui.lock();
            ui.inspector_pane_visible = visible;
        }
        self.persist_state();
    }

    /// Snapshot the live in-memory state (account picker + UI
    /// preferences) into a [`PersistedState`] and write it to
    /// `forge-state.toml`. Both [`Self::get_agent_handle`] (account
    /// picker mutation) and [`Self::set_projects_pane_visible`] (UI
    /// toggle) flow through here, keeping the on-disk file the
    /// single source of truth. Best-effort — `state::save` logs and
    /// returns on failure so a transient write error never
    /// propagates back into the calling path.
    fn persist_state(&self) {
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
            let ui = self.persisted_ui.lock();
            PersistedState {
                accounts: persisted_accounts,
                selection: PersistedSelectionState { round_robin_next },
                ui: PersistedUiState {
                    projects_pane_visible: ui.projects_pane_visible,
                    inspector_pane_visible: ui.inspector_pane_visible,
                },
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
        let project_key =
            ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
                &project.path.to_string_lossy(),
            )));
        self.try_lead_session_id_for(project)
            .unwrap_or_else(|| SessionKey::new(format!("__fresh__:{}", project_key.as_str())))
    }

    /// Return the project's lead (most-recent) session id when the
    /// on-disk catalog has one, else `None`. Drives the resume-first
    /// behaviour in [`Self::get_agent_handle`]: project-rooted targets
    /// (`Default` / `Named`) resume the lead when it exists and fall
    /// back to a fresh session otherwise.
    fn try_lead_session_id_for(&self, project: &LoadedProject) -> Option<SessionKey> {
        let key = ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(
            Some(&project.path.to_string_lossy()),
        ));
        let catalog = self.catalog.lock();
        let entries = catalog.get(&key)?;
        let lead = entries.first()?;
        Some(SessionKey::new(lead.session_id.clone()))
    }

    /// Locate a `ProjectView`-like (`LoadedProject`) by `name` from
    /// `forge.toml`. Returns `None` when no project carries that name.
    /// Used by the spawn handlers to resolve the project's path / cwd
    /// before emitting `SessionUpdate::Spawning`.
    pub(crate) fn find_project_view_by_name(&self, name: &str) -> Option<LoadedProject> {
        self.config.projects.iter().find(|p| p.name == name).cloned()
    }

    /// Locate the parent project of a given `session_id` by walking
    /// the catalog. Used by `Command::SpawnSession` to seed the
    /// spawning bucket's cwd from the session's owning project before
    /// the agent boots.
    pub(crate) fn find_project_for_session(
        &self,
        session_key: &SessionKey,
    ) -> Option<LoadedProject> {
        let catalog = self.catalog.lock();
        let owning_project_key = catalog.iter().find_map(|(project_key, entries)| {
            if entries.iter().any(|e| e.session_id == session_key.as_str()) {
                Some(project_key.clone())
            } else {
                None
            }
        })?;
        drop(catalog);
        for project in &self.config.projects {
            let key =
                ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(
                    Some(&project.path.to_string_lossy()),
                ));
            if key == owning_project_key {
                return Some(project.clone());
            }
        }
        None
    }

    /// Internal accessor for the SessionUpdate fan-in sender. Used
    /// by `spawn.rs` to emit `Spawning` / `ConnectionFailed` /
    /// `FatalError` from the App-level handlers.
    pub(crate) fn update_tx(&self) -> &mpsc::UnboundedSender<SessionUpdate> {
        &self.update_tx
    }

    /// Look up a session's recorded cwd by id. `claude --resume`
    /// indexes by the project key derived from the subprocess's
    /// working directory, so every explicit-resume code path must
    /// spawn the subprocess in the session's original cwd or the
    /// resume hits "No conversation found with session ID …" even
    /// when the `.jsonl` exists. Returns `None` when the catalog has
    /// no entry for `session_id` or the entry lacks a cwd.
    #[must_use]
    pub fn session_cwd_for(&self, session_id: &SessionKey) -> Option<String> {
        self.catalog
            .lock()
            .values()
            .flatten()
            .find(|info| info.session_id == session_id.as_str())
            .and_then(|info| info.cwd.clone())
    }

    /// Insert (or update) a session entry under the project that
    /// owns `cwd`. Called by the forge-tui connect-flow after every
    /// `Connected` migration so newly spawned sessions surface in the
    /// Projects-pane drilldown immediately, without forcing a full
    /// disk re-scan. The entry is placed at the head of the
    /// project's session list to match the most-recent-first ordering
    /// the scan produces.
    pub fn record_connected_session(&self, cwd: &str, session_id: &str, summary: Option<String>) {
        let key = ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(
            Some(cwd),
        ));
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        let entry = SDKSessionInfo {
            session_id: session_id.to_owned(),
            summary: summary.unwrap_or_else(|| "new session".to_owned()),
            last_modified: now_ms,
            file_size: None,
            custom_title: None,
            first_prompt: None,
            git_branch: None,
            cwd: Some(cwd.to_owned()),
            tag: None,
            created_at: None,
        };
        let mut catalog = self.catalog.lock();
        let entries = catalog.entry(key).or_default();
        entries.retain(|e| e.session_id != session_id);
        entries.insert(0, entry);
    }

    /// Single-take fan-in receiver for [`SessionUpdate`]s. Returns
    /// `None` on subsequent calls. forge-tui's main event loop owns
    /// the returned `mpsc::UnboundedReceiver` and reads `SessionUpdate`
    /// envelopes directly (Phase 4 retired the legacy `ClientEvent`
    /// channel; this is now the only event source the App consumes).
    pub fn subscribe(&self) -> Option<mpsc::UnboundedReceiver<SessionUpdate>> {
        self.update_rx_slot.lock().take()
    }

    /// Clone the [`SessionUpdate`] sender. TUI-side async tasks
    /// (plugin inventory refresh, usage refresh, slash executors,
    /// service-status check, the input-submit cancel-emit path) hold
    /// a clone so they can forward state into the App's event loop
    /// the same way the workspace's `SessionTask`s do. Cloned at App
    /// construction time and stored on `App.update_tx`.
    #[must_use]
    pub fn update_sender(&self) -> mpsc::UnboundedSender<SessionUpdate> {
        self.update_tx.clone()
    }

    /// Borrow the workspace-side [`DomainSession`] for `key`.
    ///
    /// Returns the [`Arc`]-cloned mutex protecting the domain bucket.
    /// Callers `.lock()` to read or mutate. `None` when no
    /// `SessionTask` is registered for `key` (e.g., the session was
    /// closed or hasn't been spawned yet).
    ///
    /// Callers should hold the lock for the shortest scope possible
    /// — concurrent reducers and the per-session `SessionTask`
    /// share this mutex.
    #[must_use]
    pub fn domain_session_for(&self, key: &SessionKey) -> Option<Arc<Mutex<DomainSession>>> {
        self.domain_handles.lock().get(key).cloned()
    }

    /// Whether the session at `key` currently has a live agent
    /// handle stamped onto its [`DomainSession`]. Encapsulates the
    /// presence check so callers don't need to peek at
    /// `DomainSession.conn` directly — the field layout is a
    /// workspace internal.
    #[must_use]
    pub fn has_agent_for(&self, key: &SessionKey) -> bool {
        self.domain_session_for(key).is_some_and(|d| d.lock().conn.is_some())
    }

    /// Route a [`Command`]. Per-session commands (`cmd.key() ==
    /// Some(key)`) fan out to the matching `SessionTask`. App-level
    /// commands (`cmd.key() == None` — `SpawnProject`,
    /// `SpawnSession`, `StartDefault`) route to the workspace's own
    /// handler.
    ///
    /// Test fallback: when no `SessionTask` is registered for `key`
    /// but a `DomainSession` carries a stub `AgentHandle` (e.g., the
    /// `Workspace::testing_stub` path), the command runs
    /// synchronously against that handle. This keeps `#[test]`-flavor
    /// unit tests (no tokio runtime) able to observe the
    /// `forge_primitives::Command` emitted on the stub's channel
    /// without spinning up an async actor.
    ///
    /// # Errors
    ///
    /// Returns [`DispatchError::UnknownSession`] when no `SessionTask`
    /// is registered for the requested key (e.g., the session was
    /// just closed), or [`DispatchError::SessionClosed`] when the
    /// task's command receiver has been dropped.
    pub fn dispatch(self: &Arc<Self>, cmd: Command) -> Result<(), DispatchError> {
        if let Some(key) = cmd.key() {
            let key = key.clone();
            let senders = self.command_senders.lock();
            if let Some(sender) = senders.get(&key) {
                return sender.send(cmd).map_err(|_| DispatchError::SessionClosed(key));
            }
            drop(senders);
            // In production this branch returns `UnknownSession`: a
            // per-session `Command` arrives before a `SessionTask`
            // exists, which is the contract violation the error
            // signals. Under `cfg(any(test, feature = "testing"))`,
            // tests that wire a stub `AgentHandle` directly onto a
            // `DomainSession` (without a running tokio runtime to host
            // a real `SessionTask`) get a synchronous fallback so the
            // command still reaches the stub. Gating this here means
            // the fallback is structurally unreachable in production —
            // a future refactor can't open the race window silently.
            #[cfg(any(test, feature = "testing"))]
            {
                let Some(handle) = self.agent_handle_for(&key) else {
                    return Err(DispatchError::UnknownSession(key));
                };
                let sid = self.domain_handles.lock().get(&key).and_then(|d| {
                    d.lock().session_id.as_ref().map(std::string::ToString::to_string)
                });
                crate::session_task::execute_command_via_handle(&handle, &key, sid.as_deref(), cmd)
                    .map_err(|_| DispatchError::SessionClosed(key))
            }
            #[cfg(not(any(test, feature = "testing")))]
            {
                let _ = cmd;
                Err(DispatchError::UnknownSession(key))
            }
        } else {
            // App-level commands: route into `spawn::*` handlers. The
            // spawn handlers run on a tokio task because they await
            // `get_agent_handle_with_spawn_key` (which blocks the
            // sender if we ran it inline).
            let workspace = Arc::clone(self);
            match cmd {
                Command::SpawnProject { project_name, launch_settings } => {
                    tokio::spawn(spawn::handle_spawn_project(
                        workspace,
                        project_name,
                        launch_settings,
                    ));
                }
                Command::SpawnSession { session_id, launch_settings } => {
                    tokio::spawn(spawn::handle_spawn_session(
                        workspace,
                        session_id,
                        launch_settings,
                    ));
                }
                Command::StartDefault { project_name, launch_settings } => {
                    tokio::spawn(spawn::handle_start_default(
                        workspace,
                        project_name,
                        launch_settings,
                    ));
                }
                other => {
                    tracing::warn!(
                        target: "forge_workspace",
                        command = ?other,
                        "unexpected App-level command (no key but not a spawn variant); ignored",
                    );
                }
            }
            Ok(())
        }
    }

    /// Park an oneshot in
    /// `DomainSession.pending_interactions[tool_id]`. Called from
    /// `bridge_lifecycle` when an `AgentEvent::PermissionRequest` /
    /// `QuestionRequest` / `ElicitationRequest` arrives.
    ///
    /// No-op when no `SessionTask` is registered for `key` (e.g.,
    /// the session was just closed) — the oneshot is dropped and the
    /// caller's forwarder task observes a closed receiver, which
    /// surfaces as an `oneshot::Recv` error in the existing
    /// permission/question response forwarder paths.
    pub fn store_pending_interaction(
        &self,
        key: &SessionKey,
        tool_id: String,
        slot: PendingInteractionSlot,
    ) {
        let Some(domain) = self.domain_handles.lock().get(key).cloned() else {
            tracing::warn!(
                target: "forge_workspace",
                key = %key.as_str(),
                tool_id = %tool_id,
                "store_pending_interaction: no domain handle for key (session may be closed)",
            );
            return;
        };
        let mut guard = domain.lock();
        guard.pending_interactions.insert(tool_id, slot);
    }

    /// Set the `session_id` field on the workspace's `DomainSession`
    /// for `key`. No-op when no domain handle is registered for
    /// `key`. Used by `App::set_session_id` to stamp the
    /// claude-issued UUID once the first `Connected` event fires —
    /// the workspace consults this when routing `AgentHandle` calls
    /// that take a session_id.
    pub fn set_session_id_in_domain(
        &self,
        key: &SessionKey,
        value: Option<forge_primitives::SessionId>,
    ) {
        let Some(domain) = self.domain_handles.lock().get(key).cloned() else {
            return;
        };
        domain.lock().session_id = value;
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
    pub async fn shutdown(&self) {
        // Drop command senders first so every SessionTask sees its
        // command channel close and exits cleanly.
        let _ = self.command_senders.lock().drain().collect::<Vec<_>>();
        let _ = self.domain_handles.lock().drain().collect::<Vec<_>>();
        let entries: Vec<_> = self.pool.lock().drain().collect();
        // Each (SessionKey, PooledAgent) drops here; the
        // subprocess teardown chain (sender drop -> dispatcher exit
        // -> Client drop -> subprocess kill_on_drop) is synchronous
        // and fast in 1a.
        drop(entries);
    }

    /// Release a single session's pool entry. Drops the workspace's
    /// `Arc<AgentHandle>` for that key so the underlying `claude`
    /// subprocess exits once the consumer (forge-tui's bucket) also
    /// drops its reference. No-op when the key isn't in the pool.
    /// Called from forge-tui's per-row "close" action.
    pub fn release_session(&self, session_key: &SessionKey) {
        let removed = self.pool.lock().remove(session_key);
        drop(removed);
        let _ = self.command_senders.lock().remove(session_key);
        let _ = self.domain_handles.lock().remove(session_key);
    }

    // ---- Refresh helpers (workspace → agent) ----
    //
    // These five methods are query-style: TUI says "re-emit state X
    // for `key`" and the payload returns via `SessionUpdate`. They
    // bypass the [`Command`] envelope because they don't carry
    // mutation state, and they need the workspace-side
    // `Arc<AgentHandle>` lookup rather than per-session task routing.

    /// Request a fresh status snapshot for `key`. The bridge replies
    /// asynchronously via [`SessionUpdate::StatusSnapshot`].
    ///
    /// # Errors
    ///
    /// Returns [`DispatchError::UnknownSession`] when no agent is
    /// registered for `key` (e.g., the session was just closed) or
    /// when the bridge hasn't stamped a `session_id` yet.
    pub fn refresh_status_snapshot(&self, key: &SessionKey) -> Result<(), DispatchError> {
        let (handle, sid) = self.handle_and_session_id(key)?;
        handle.get_status_snapshot(sid).map_err(|_| DispatchError::SessionClosed(key.clone()))
    }

    /// Request a fresh OAuth credentials snapshot for `key`. The
    /// bridge replies via [`SessionUpdate::OauthCredentialsSnapshot`].
    ///
    /// # Errors
    ///
    /// See [`Self::refresh_status_snapshot`].
    pub fn refresh_oauth_credentials_snapshot(
        &self,
        key: &SessionKey,
    ) -> Result<(), DispatchError> {
        let (handle, sid) = self.handle_and_session_id(key)?;
        handle
            .get_oauth_credentials_snapshot(sid)
            .map_err(|_| DispatchError::SessionClosed(key.clone()))
    }

    /// Request a fresh context-usage snapshot for `key`. The bridge
    /// replies via [`SessionUpdate::ContextUsageSnapshot`].
    ///
    /// # Errors
    ///
    /// See [`Self::refresh_status_snapshot`].
    pub fn refresh_context_usage(&self, key: &SessionKey) -> Result<(), DispatchError> {
        let (handle, sid) = self.handle_and_session_id(key)?;
        handle.get_context_usage(sid).map_err(|_| DispatchError::SessionClosed(key.clone()))
    }

    /// Reload session plugins for `key`. The bridge replies via
    /// [`SessionUpdate::RuntimeReloadCompleted`] / `RuntimeReloadFailed`.
    ///
    /// # Errors
    ///
    /// See [`Self::refresh_status_snapshot`].
    pub fn reload_plugins(&self, key: &SessionKey) -> Result<(), DispatchError> {
        let (handle, sid) = self.handle_and_session_id(key)?;
        handle.reload_plugins(sid).map_err(|_| DispatchError::SessionClosed(key.clone()))
    }

    /// Request a fresh MCP server snapshot for `key`. The bridge
    /// replies via [`SessionUpdate::McpSnapshot`].
    ///
    /// # Errors
    ///
    /// See [`Self::refresh_status_snapshot`].
    pub fn refresh_mcp_snapshot(&self, key: &SessionKey) -> Result<(), DispatchError> {
        let (handle, sid) = self.handle_and_session_id(key)?;
        handle.get_mcp_snapshot(sid).map_err(|_| DispatchError::SessionClosed(key.clone()))
    }

    // ---- Direct-accessor facades (workspace owns the bridge call) ----

    /// Resolve the auto-memory path the bridge would consult for
    /// `cwd`, scoped to `key`'s configured account. Returns `None`
    /// when no agent is registered for `key`.
    #[must_use]
    pub fn project_memory_path(&self, key: &SessionKey, cwd: &std::path::Path) -> Option<PathBuf> {
        let handle = self.agent_handle_for(key)?;
        Some(handle.project_memory_path(cwd))
    }

    /// Snapshot the bridge's settings documents for `key` at `cwd`.
    /// Returns `None` when no agent is registered for `key`.
    #[must_use]
    pub fn settings_documents(
        &self,
        key: &SessionKey,
        cwd: &std::path::Path,
    ) -> Option<forge_agent::userdata::settings::SettingsDocuments> {
        let handle = self.agent_handle_for(key)?;
        Some(handle.settings_documents(cwd))
    }

    /// Persist a settings document via the bridge for `key`.
    ///
    /// # Errors
    ///
    /// Returns `Err` with a human-readable message when no agent is
    /// registered for `key` or when the bridge's I/O fails.
    pub fn write_settings_document(
        &self,
        key: &SessionKey,
        target: &forge_agent::userdata::settings::SettingsTarget,
        document: &serde_json::Value,
    ) -> Result<(), String> {
        let handle = self
            .agent_handle_for(key)
            .ok_or_else(|| "no agent registered for session".to_owned())?;
        handle.write_settings_document(target, document).map_err(|err| err.to_string())
    }

    /// Resolve the agent's configured config_dir for `key`. Returns
    /// `None` when no agent is registered for `key`.
    #[must_use]
    pub fn config_dir_for(&self, key: &SessionKey) -> Option<PathBuf> {
        let handle = self.agent_handle_for(key)?;
        Some(handle.config_dir())
    }

    /// Fetch the OAuth usage payload via the bridge bound to `key`.
    ///
    /// # Errors
    ///
    /// Returns `Err` with a human-readable message when no agent is
    /// registered for `key`; otherwise propagates the bridge's
    /// `OauthUsageError`.
    pub async fn oauth_usage(
        &self,
        key: &SessionKey,
    ) -> Result<forge_agent::cloud::oauth_usage::OauthUsage, String> {
        let handle = self
            .agent_handle_for(key)
            .ok_or_else(|| "Bridge connection required for OAuth usage fetch.".to_owned())?;
        handle.oauth_usage().await.map_err(|err| err.to_string())
    }

    /// Scan the project at `cwd` and return a git-diff snapshot for
    /// the Inspector pane's GIT section. Delegates to
    /// [`forge_agent::env::git_diff::scan`] — exists as a workspace
    /// method so the TUI never depends on `forge-agent` directly
    /// (see CLAUDE.md placement guide).
    ///
    /// Infallible: scanner errors collapse to
    /// `GitDiffView::NoRepo` with structured WARN logs. Renderer
    /// treats the returned snapshot as authoritative regardless of
    /// which variant came back.
    pub async fn scan_git_diff(
        &self,
        cwd: &std::path::Path,
    ) -> forge_agent::env::git_diff::GitDiffSnapshot {
        forge_agent::env::git_diff::scan(cwd).await
    }

    /// Probe the local `claude --version` and the latest published
    /// version on npm in parallel, returning both via
    /// [`forge_agent::env::cli_version::CliVersionInfo`]. Used by
    /// the bottom-left account panel to render the forge + claude
    /// version rows and the `↑ vX.Y.Z` update indicator.
    ///
    /// Infallible: each probe collapses to `None` on its half of the
    /// struct when it fails (binary missing, no network, parse
    /// failure, timeout); structured WARN logs surface the failure
    /// without breaking the renderer.
    pub async fn fetch_cli_version_info(&self) -> forge_agent::env::cli_version::CliVersionInfo {
        forge_agent::env::cli_version::fetch_info().await
    }

    /// Borrow the [`Arc<AgentHandle>`] registered against `key`.
    /// Workspace-internal helper — surfaces a sometimes-`None` to keep
    /// the early-init / disconnected branches explicit.
    fn agent_handle_for(&self, key: &SessionKey) -> Option<Arc<AgentHandle>> {
        let pool = self.pool.lock();
        if let Some(pooled) = pool.get(key) {
            return Some(Arc::clone(&pooled.handle));
        }
        drop(pool);
        // Fall back to `domain_handles[key].conn` for pre-Connect /
        // testing-stub callers that never went through the pool path.
        let domain = self.domain_handles.lock().get(key).cloned()?;
        domain.lock().conn.clone()
    }

    /// Resolve `(handle, session_id_string)` for `key`. Both must be
    /// available; missing either surfaces as `UnknownSession` for
    /// uniform error handling.
    fn handle_and_session_id(
        &self,
        key: &SessionKey,
    ) -> Result<(Arc<AgentHandle>, String), DispatchError> {
        let handle =
            self.agent_handle_for(key).ok_or_else(|| DispatchError::UnknownSession(key.clone()))?;
        let sid = self
            .domain_handles
            .lock()
            .get(key)
            .and_then(|d| d.lock().session_id.as_ref().map(std::string::ToString::to_string))
            .ok_or_else(|| DispatchError::UnknownSession(key.clone()))?;
        Ok((handle, sid))
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

impl Workspace {
    /// Register a fresh `DomainSession` for `key` under this workspace.
    /// `handle` is `None` for pre-spawn / pre-Connect domains (filled
    /// in later when the spawn handler runs); `Some` for test fixtures
    /// that wire a stub handle up front.
    ///
    /// `DomainSession` carries workspace-internal routing metadata
    /// (`conn` / `session_id` / `pending_interactions`). TUI's per-
    /// session operational state lives on `UiSession`; the workspace
    /// does not read or write those fields.
    ///
    /// Returns the inserted `Arc<Mutex<DomainSession>>` so callers
    /// can seed fields on the same handle they just registered.
    pub fn register_domain_session(
        &self,
        key: SessionKey,
        handle: Option<Arc<forge_agent::AgentHandle>>,
    ) -> Arc<Mutex<DomainSession>> {
        let domain = Arc::new(Mutex::new(DomainSession::new(key.clone(), handle)));
        self.domain_handles.lock().insert(key, Arc::clone(&domain));
        domain
    }

    /// Migrate an existing `DomainSession` registration from `from` to
    /// `to`. Used by the TUI's `set_session_id` migration when the
    /// pre-Connect synthetic key gets replaced with the real
    /// claude-issued session id. No-op when `from` is not registered.
    pub fn rekey_domain_session(&self, from: &SessionKey, to: SessionKey) {
        let mut handles = self.domain_handles.lock();
        if let Some(domain) = handles.remove(from) {
            handles.insert(to, domain);
        }
    }

    /// Migrate this workspace's per-`SessionTask` registrations from
    /// `from` to `to`. Called by the per-session task actor on
    /// `Connected` / `SessionReplaced` when the pool key it was
    /// registered under differs from the real claude-issued session
    /// UUID — i.e., the `/new` / `/resume` / first-Connect-of-a-fresh-
    /// project paths where the pool key was a placeholder (a previous
    /// session's id or `__fresh__:<project_key>`) and the actual
    /// session UUID isn't known until the bridge fires `init`.
    ///
    /// Atomically moves the entries in `pool`, `command_senders`, and
    /// `domain_handles` (and rewrites the moved `DomainSession.key`
    /// field). Without this migration, `Workspace::dispatch`'s key
    /// lookup falls off the end with `UnknownSession` for every
    /// `Command::Prompt` / `Cancel` / etc. after a session-replace —
    /// the SessionTask is still alive at the old key but the TUI's
    /// `active_session_key` has flipped to the new one.
    ///
    /// No-op when `from == to` or when `from` is not registered.
    pub fn migrate_session_task(&self, from: &SessionKey, to: &SessionKey) {
        if from == to {
            return;
        }
        // Lock order matches `get_agent_handle`'s insertion order:
        // pool → command_senders → domain_handles.
        let mut pool = self.pool.lock();
        let mut senders = self.command_senders.lock();
        let mut handles = self.domain_handles.lock();
        if let Some(pooled) = pool.remove(from) {
            pool.insert(to.clone(), pooled);
        }
        if let Some(sender) = senders.remove(from) {
            senders.insert(to.clone(), sender);
        }
        if let Some(domain) = handles.remove(from) {
            domain.lock().key = to.clone();
            handles.insert(to.clone(), domain);
        }
    }
}

#[cfg(feature = "testing")]
impl Workspace {
    /// Construct a stub `AgentHandle` plus the matching
    /// `Receiver<forge_primitives::Command>` that drains every command
    /// dispatched to it. Tests use this to wire `App.set_active_conn`
    /// without spinning up a real subprocess; the bridge underneath is
    /// `forge_agent::Agent::testing_stub` — same shape as before, now
    /// reachable from forge-tui via `forge_workspace::Workspace::*`
    /// so the TUI crate no longer needs a direct `forge-agent` dep.
    ///
    /// The returned `Receiver` carries `forge_primitives::Command`
    /// because that's what the bridge's dispatcher accepts; this is
    /// distinct from [`crate::protocol::Command`] (the workspace's
    /// outer envelope) that wraps these primitives under a
    /// `SessionKey`.
    #[must_use]
    pub fn testing_stub_handle()
    -> (forge_agent::AgentHandle, mpsc::UnboundedReceiver<forge_primitives::Command>) {
        forge_agent::Agent::testing_stub()
    }

    /// Register a fresh testing-stub agent against `key`'s
    /// `DomainSession`. Returns the matching
    /// `forge_primitives::Command` receiver so tests can assert on
    /// the commands the workspace routes through it (the same shape
    /// as `testing_stub_handle()`, but with the handle installed in
    /// one step so TUI test code doesn't have to touch
    /// `AgentHandle` directly).
    ///
    /// Auto-creates a `DomainSession` for `key` when none is
    /// registered yet; otherwise overwrites the existing
    /// `DomainSession.conn` slot.
    #[must_use]
    pub fn install_testing_stub(
        &self,
        key: &SessionKey,
    ) -> mpsc::UnboundedReceiver<forge_primitives::Command> {
        let (handle, rx) = forge_agent::Agent::testing_stub();
        let arc = Arc::new(handle);
        let domain = self
            .domain_session_for(key)
            .unwrap_or_else(|| self.register_domain_session(key.clone(), None));
        domain.lock().conn = Some(arc);
        rx
    }

    /// Convenience alias for [`Self::register_domain_session`] used
    /// by tests that previously named it `_for_test`. Always wraps
    /// the handle in `Some(...)`; production callers that need
    /// `None` (pre-spawn) use [`Self::register_domain_session`]
    /// directly.
    pub fn register_domain_session_for_test(
        &self,
        key: SessionKey,
        handle: Arc<forge_agent::AgentHandle>,
    ) -> Arc<Mutex<DomainSession>> {
        self.register_domain_session(key, Some(handle))
    }

    /// Convenience alias for [`Self::rekey_domain_session`].
    pub fn rekey_domain_session_for_test(&self, from: &SessionKey, to: SessionKey) {
        self.rekey_domain_session(from, to);
    }

    /// Construct an empty `Workspace` for use in unit tests. Skips
    /// the on-disk `forge.toml` load + catalog scan that
    /// [`Workspace::new`] performs; the returned workspace carries an
    /// empty project list and an empty pool. Tests register a domain
    /// session via [`Self::register_domain_session_for_test`] before
    /// exercising any code path that needs one.
    ///
    /// Returns the workspace alongside the `SessionUpdate` receiver
    /// (single-take slot — subsequent `subscribe()` calls on the
    /// returned workspace will return `None`).
    #[must_use]
    pub fn testing_stub() -> (Arc<Self>, mpsc::UnboundedReceiver<SessionUpdate>) {
        let (update_tx, update_rx) = mpsc::unbounded_channel::<SessionUpdate>();
        let workspace = Self {
            config_dir: PathBuf::from("/tmp/forge-testing-stub"),
            config: LoadedConfig::empty_for_test(),
            catalog: Mutex::new(HashMap::new()),
            pool: Mutex::new(HashMap::new()),
            accounts: Mutex::new(AccountStateMap::empty_for_test()),
            persisted_ui: Mutex::new(PersistedUiState::default()),
            update_tx,
            update_rx_slot: Mutex::new(None),
            command_senders: Mutex::new(HashMap::new()),
            domain_handles: Mutex::new(HashMap::new()),
        };
        (Arc::new(workspace), update_rx)
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
        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
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
        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
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
        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
        let handle = workspace
            .get_agent_handle(SessionTarget::Default, SessionLaunchSettings::default())
            .await
            .expect("default");

        // Pool has one entry going in.
        assert_eq!(workspace.pool.lock().len(), 1);
        // Workspace pool + spawned `SessionTask.handle` +
        // `DomainSession.conn` (Phase 2 owned by the workspace's
        // `domain_handles` map) + this test all hold the Arc →
        // strong_count == 4.
        assert_eq!(Arc::strong_count(&handle), 4);

        // Shutdown consumes self and must return. Drops `command_senders`,
        // which closes each `SessionTask`'s command channel; the spawned
        // task then exits and drops its `handle` clone.
        workspace.shutdown().await;

        // The spawned `SessionTask` exits asynchronously after its
        // command channel closes; yield to let it run to completion
        // so the final `handle` drop is observable in `strong_count`.
        for _ in 0..16 {
            tokio::task::yield_now().await;
            if Arc::strong_count(&handle) == 1 {
                break;
            }
        }
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

        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
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

        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
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
        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
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
        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));

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
        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
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

    // ---- Refresh + facade tests ----
    //
    // Use a synthetically-registered `DomainSession` so the test
    // doesn't need a real subprocess. The stub handle's command
    // dispatcher captures whatever the workspace pushes through it,
    // which is what we assert on.

    /// Wire one `DomainSession` for `key` against a fresh testing
    /// stub. Returns the workspace, the matching primitives command
    /// receiver, and the registered key.
    fn ws_with_stub_session()
    -> (Arc<Workspace>, mpsc::UnboundedReceiver<forge_primitives::Command>, SessionKey) {
        let (workspace, _update_rx) = Workspace::testing_stub();
        let key = SessionKey::from_str_for_test("refresh-test");
        let rx = workspace.install_testing_stub(&key);
        // Stamp a session_id so refresh paths don't bail on
        // "no session_id yet".
        if let Some(domain) = workspace.domain_session_for(&key) {
            domain.lock().session_id =
                Some(forge_primitives::SessionId::new(key.as_str().to_owned()));
        }
        (workspace, rx, key)
    }

    #[test]
    fn refresh_status_snapshot_dispatches_get_status_snapshot() {
        let (workspace, mut rx, key) = ws_with_stub_session();
        workspace.refresh_status_snapshot(&key).expect("dispatch");
        let cmd = rx.try_recv().expect("queued");
        assert!(matches!(cmd, forge_primitives::Command::GetStatusSnapshot { .. }));
    }

    #[test]
    fn refresh_context_usage_dispatches_get_context_usage() {
        let (workspace, mut rx, key) = ws_with_stub_session();
        workspace.refresh_context_usage(&key).expect("dispatch");
        let cmd = rx.try_recv().expect("queued");
        assert!(matches!(cmd, forge_primitives::Command::GetContextUsage { .. }));
    }

    #[test]
    fn refresh_oauth_credentials_dispatches() {
        let (workspace, mut rx, key) = ws_with_stub_session();
        workspace.refresh_oauth_credentials_snapshot(&key).expect("dispatch");
        let cmd = rx.try_recv().expect("queued");
        assert!(matches!(cmd, forge_primitives::Command::GetOauthCredentialsSnapshot { .. }));
    }

    #[test]
    fn reload_plugins_dispatches() {
        let (workspace, mut rx, key) = ws_with_stub_session();
        workspace.reload_plugins(&key).expect("dispatch");
        let cmd = rx.try_recv().expect("queued");
        assert!(matches!(cmd, forge_primitives::Command::ReloadPlugins { .. }));
    }

    #[test]
    fn refresh_mcp_snapshot_dispatches() {
        let (workspace, mut rx, key) = ws_with_stub_session();
        workspace.refresh_mcp_snapshot(&key).expect("dispatch");
        let cmd = rx.try_recv().expect("queued");
        assert!(matches!(cmd, forge_primitives::Command::GetMcpSnapshot { .. }));
    }

    #[test]
    fn refresh_status_snapshot_unknown_session_errors() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        let key = SessionKey::from_str_for_test("never-registered");
        let err = workspace.refresh_status_snapshot(&key).expect_err("unknown session");
        assert!(matches!(err, DispatchError::UnknownSession(_)));
    }

    /// `dispatch(Command::Cancel)` falls back to synchronous direct
    /// dispatch when no SessionTask is registered. This is the path
    /// TUI unit tests rely on — `set_active_conn` installs a stub
    /// handle but never spawns a task, and tests need to observe
    /// the primitive command on the rx.
    #[test]
    fn dispatch_falls_back_to_direct_when_no_session_task() {
        let (workspace, mut rx, key) = ws_with_stub_session();
        workspace.dispatch(Command::Cancel { key }).expect("dispatch");
        let cmd = rx.try_recv().expect("queued");
        assert!(matches!(cmd, forge_primitives::Command::Cancel { .. }));
    }

    // ---- Session-task rekey tests ----
    //
    // Regression coverage for the `/new` prompt-stuck bug: the
    // `SessionTask` was registered under the pool key from
    // `resolve_target` (usually the previous session's id), but TUI's
    // `active_session_key` flips to the real session UUID on
    // `SessionUpdate::SessionReplaced`. Without `migrate_session_task`,
    // `Command::Prompt { key: new_uuid }` fell off `dispatch`'s key
    // lookup with `UnknownSession`. These tests pin the routing.

    /// Seed `command_senders`, `pool`, and `domain_handles` at `key`
    /// against a fresh stub handle so `Workspace::dispatch` will
    /// route through the `SessionTask`-style fast path (rather than
    /// the test-only direct fallback). Returns the routed-command
    /// receiver so the test can assert on what flows through.
    fn install_fake_session_task(
        workspace: &Arc<Workspace>,
        key: &SessionKey,
    ) -> mpsc::UnboundedReceiver<Command> {
        let (handle, _agent_rx) = Workspace::testing_stub_handle();
        let arc = Arc::new(handle);
        workspace.pool.lock().insert(
            key.clone(),
            PooledAgent { handle: Arc::clone(&arc), account: AccountKey("test".to_owned()) },
        );
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Command>();
        workspace.command_senders.lock().insert(key.clone(), cmd_tx);
        let domain = workspace.register_domain_session(key.clone(), Some(arc));
        domain.lock().session_id = Some(forge_primitives::SessionId::new(key.as_str()));
        cmd_rx
    }

    /// Direct unit test on `migrate_session_task`: each map (`pool`,
    /// `command_senders`, `domain_handles`) moves from `from` to `to`,
    /// and the migrated `DomainSession.key` field is rewritten.
    #[test]
    fn migrate_session_task_moves_all_three_maps() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        let from = SessionKey::from_str_for_test("old-pool-key");
        let to = SessionKey::from_str_for_test("real-session-uuid");
        let _cmd_rx = install_fake_session_task(&workspace, &from);

        assert!(workspace.command_senders.lock().contains_key(&from));
        assert!(workspace.pool.lock().contains_key(&from));
        assert!(workspace.domain_handles.lock().contains_key(&from));

        workspace.migrate_session_task(&from, &to);

        assert!(!workspace.command_senders.lock().contains_key(&from));
        assert!(workspace.command_senders.lock().contains_key(&to));
        assert!(!workspace.pool.lock().contains_key(&from));
        assert!(workspace.pool.lock().contains_key(&to));
        assert!(!workspace.domain_handles.lock().contains_key(&from));
        let domain = workspace.domain_handles.lock().get(&to).cloned().expect("migrated");
        assert_eq!(domain.lock().key.as_str(), to.as_str());
    }

    /// Regression for the `/new` prompt-stuck bug. A `Command::Prompt`
    /// dispatched against the new key must route through the
    /// migrated `command_senders` entry, not fall off with
    /// `UnknownSession`. Mirrors what happens inside the SessionTask
    /// after `AgentEvent::SessionReplaced`.
    #[test]
    fn dispatch_after_migrate_routes_to_new_key() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        let from = SessionKey::from_str_for_test("pre-new-session");
        let to = SessionKey::from_str_for_test("post-new-session");
        let mut cmd_rx = install_fake_session_task(&workspace, &from);

        // Before migrate: dispatch at `to` fails because no entry
        // exists, dispatch at `from` succeeds.
        let result = workspace.dispatch(Command::Cancel { key: to.clone() });
        assert!(matches!(result, Err(DispatchError::UnknownSession(_))));

        workspace.dispatch(Command::Cancel { key: from.clone() }).expect("from routes");
        assert!(matches!(cmd_rx.try_recv(), Ok(Command::Cancel { .. })));

        // Migrate, then re-test.
        workspace.migrate_session_task(&from, &to);

        let result = workspace.dispatch(Command::Cancel { key: from.clone() });
        assert!(
            matches!(result, Err(DispatchError::UnknownSession(_))),
            "old key must not route after migration"
        );

        workspace.dispatch(Command::Cancel { key: to }).expect("new key routes");
        let cmd = cmd_rx.try_recv().expect("queued on migrated channel");
        assert!(matches!(cmd, Command::Cancel { .. }));
    }

    /// `migrate_session_task` with `from == to` is a no-op. Guards
    /// the typical case where the pool key already equals the real
    /// session UUID (e.g., resuming an existing lead session).
    #[test]
    fn migrate_session_task_no_op_when_from_equals_to() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        let key = SessionKey::from_str_for_test("same-key");
        let _cmd_rx = install_fake_session_task(&workspace, &key);

        workspace.migrate_session_task(&key, &key);

        assert!(workspace.command_senders.lock().contains_key(&key));
        assert!(workspace.pool.lock().contains_key(&key));
        assert!(workspace.domain_handles.lock().contains_key(&key));
    }

    /// `migrate_session_task` on an unregistered source is a no-op.
    #[test]
    fn migrate_session_task_no_op_when_from_unregistered() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        let from = SessionKey::from_str_for_test("never-registered");
        let to = SessionKey::from_str_for_test("destination");

        workspace.migrate_session_task(&from, &to);

        assert!(!workspace.command_senders.lock().contains_key(&from));
        assert!(!workspace.command_senders.lock().contains_key(&to));
        assert!(!workspace.pool.lock().contains_key(&to));
        assert!(workspace.domain_session_for(&to).is_none());
    }
}
