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
    /// Retained for Phase 1b's per-account config-dir binding. After
    /// `new` consumes it to load `forge.toml`, no current code path
    /// re-reads it; the field stays on the struct so 1b can introduce
    /// account-scoped re-resolution without restructuring the type.
    #[allow(dead_code)]
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

    /// Hands out the `Arc<AgentHandle>` for the requested session,
    /// spawning the underlying Agent lazily if it isn't already pooled.
    /// Idempotent — repeated calls for the same target return the same
    /// handle (no second subprocess). `settings` only apply to the
    /// spawn; subsequent calls reuse the existing Agent and ignore the
    /// parameter.
    ///
    /// Workspace does not track which handle the caller is "using" —
    /// that's the caller's concern.
    ///
    /// `async` is mandated by the spec contract — Phase 1b will need
    /// to await account-bound resolution before pool insertion.
    #[allow(clippy::unused_async)]
    pub async fn get_agent_handle(
        &self,
        target: SessionTarget,
        settings: SessionLaunchSettings,
    ) -> Result<Arc<AgentHandle>> {
        let session_key = self.resolve_target(&target);

        // Fast path: cache hit
        {
            let pool = self.pool.lock();
            if let Some(existing) = pool.get(&session_key) {
                return Ok(Arc::clone(existing));
            }
        }

        // Slow path: spawn fresh Agent and dispatch the start command.
        let handle = forge_agent::Agent::spawn();
        match &target {
            SessionTarget::Default => {
                let cwd = self
                    .config
                    .default_project()
                    .path
                    .to_string_lossy()
                    .to_string();
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
                return Ok(Arc::clone(existing));
            }
            pool.insert(session_key, Arc::clone(&arc));
        }

        Ok(arc)
    }

    /// Resolves a `SessionTarget` to the `SessionKey` used to look up
    /// the pool. For `Default` with no on-disk session for the project,
    /// returns a project-keyed placeholder (`__fresh__:<project_key>`)
    /// so re-entry stays idempotent against the same fresh-session
    /// intent.
    fn resolve_target(&self, target: &SessionTarget) -> SessionKey {
        match target {
            SessionTarget::Default => {
                let project = self.config.default_project();
                let key = ProjectKey::new(
                    forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
                        &project.path.to_string_lossy(),
                    )),
                );
                if let Some(entries) = self.catalog.get(&key)
                    && let Some(lead) = entries.first()
                {
                    return SessionKey::new(lead.session_id.clone());
                }
                SessionKey::new(format!("__fresh__:{}", key.as_str()))
            }
            SessionTarget::Session(key) => key.clone(),
        }
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
        // Each Arc<AgentHandle> drops here; the subprocess teardown
        // chain (sender drop -> dispatcher exit -> Client drop ->
        // subprocess kill_on_drop) is synchronous and fast in 1a.
        drop(entries);
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
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
        let handle2 = workspace
            .get_agent_handle(SessionTarget::Default, settings)
            .await
            .expect("second");

        assert!(
            Arc::ptr_eq(&handle1, &handle2),
            "expected pool hit for repeated Default target",
        );
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
        assert_eq!(
            workspace.pool.lock().len(),
            2,
            "distinct target adds a pool entry"
        );
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
}
