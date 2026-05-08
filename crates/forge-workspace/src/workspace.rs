//! The orchestrator. Implementation fans out across Tasks 4–6.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use forge_agent::AgentHandle;
use forge_agent::client::SessionLaunchSettings;

use crate::config::LoadedConfig;
use crate::error::WorkspaceError;
use crate::target::SessionTarget;
use crate::views::ProjectView;

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
    // Concrete fields land in Tasks 3–6. Public API surface is
    // sketched here so consumers can import `Workspace` from this
    // crate immediately.
    _config: LoadedConfig,
    _config_dir: PathBuf,
}

impl Workspace {
    /// See spec §3 for full contract. Implemented in Task 4.
    pub async fn new(_config_dir: PathBuf) -> Result<Self, WorkspaceError> {
        unimplemented!("Workspace::new lands in Task 4")
    }

    /// See spec §3. Implemented in Task 4.
    #[must_use]
    pub fn list_projects(&self) -> Vec<ProjectView> {
        unimplemented!("list_projects lands in Task 4")
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
