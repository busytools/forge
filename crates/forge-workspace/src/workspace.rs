//! The orchestrator.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::Result;
use forge_agent::AgentHandle;
use forge_agent::client::SessionLaunchSettings;
use forge_primitives::SDKSessionInfo;
use parking_lot::Mutex;

use crate::config::{LoadedConfig, load_from_dir};
use crate::error::WorkspaceError;
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
    config_dir: PathBuf,
    config: LoadedConfig,
    /// Catalog snapshot from `userdata::catalog::scan::list_sessions`,
    /// grouped by project key. Read-only after `new` returns.
    catalog: HashMap<ProjectKey, Vec<SDKSessionInfo>>,
    /// Live Agents keyed by session id. `parking_lot::Mutex` so the
    /// public methods can take `&self`.
    pool: Mutex<HashMap<SessionKey, Arc<AgentHandle>>>,
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

        Ok(Self {
            config_dir,
            config,
            catalog,
            pool: Mutex::new(HashMap::new()),
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
            let key = ProjectKey::new(
                forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
                    &project.path.to_string_lossy(),
                )),
            );

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

            views.push(ProjectView {
                key,
                display_path: project.display_path.clone(),
                sessions,
            });
        }
        views
    }

    /// See spec §3. Implemented in Task 5.
    pub async fn get_agent_handle(
        &self,
        _target: SessionTarget,
        _settings: SessionLaunchSettings,
    ) -> Result<Arc<AgentHandle>> {
        unimplemented!("get_agent_handle lands in Task 5")
    }

    /// See spec §3. Implemented in Task 6.
    pub async fn shutdown(self) {
        unimplemented!("shutdown lands in Task 6")
    }
}
