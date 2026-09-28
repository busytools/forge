//! `session()`: one session's operational state.

use std::path::{Path, PathBuf};

use forge_primitives::SessionSlot;
use forge_workspace::Workspace;

/// What a view reads about one session.
pub struct SessionState {
    /// The seat this state belongs to.
    pub slot: SessionSlot,
    /// Where forge scans git for this session: a git worker's worktree
    /// path, else the caller's `cwd_raw` unchanged.
    pub scan_cwd: PathBuf,
}

impl SessionState {
    pub(super) fn collect(workspace: &Workspace, slot: &SessionSlot, cwd_raw: &Path) -> Self {
        Self { slot: slot.clone(), scan_cwd: workspace.git_scan_cwd_for_session(slot, cwd_raw) }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use forge_primitives::{SessionSlot, WorkerLiveness};
    use forge_workspace::{WorkerEntry, Workspace};

    use crate::surface::ViewSurface;

    fn stub_workspace() -> Arc<Workspace> {
        let (workspace, _updates) = Workspace::testing_stub();
        workspace
    }

    fn git_worker(slot: SessionSlot) -> WorkerEntry {
        WorkerEntry {
            label: slot.label().to_owned(),
            charter: "charter".to_owned(),
            slot,
            session_id: None,
            status: WorkerLiveness::Running,
            spawned_at: std::time::SystemTime::UNIX_EPOCH,
            spawned_by: SessionSlot::lead("TestOrg", "forge"),
            needs_tag: false,
            is_git_repo_at_spawn: true,
            diagnostic: None,
            kick: None,
        }
    }

    /// Catches a session verb that hands back the caller's own cwd for a
    /// git worker, and one that names a seat other than the one it was
    /// asked about.
    #[test]
    fn session_scan_cwd_agrees_with_the_workspace() {
        let workspace = stub_workspace();
        workspace.seed_test_project("forge", "/tmp/forge-session");
        let surface = ViewSurface::new(Arc::clone(&workspace));
        let project = surface.roster().project_named("forge").expect("seeded project").key.clone();
        let worker = SessionSlot::worker("TestOrg", "forge", "probe-a");
        workspace.insert_live_worker(&project, git_worker(worker.clone()));

        // Both lifecycle cwds - the project root a fresh spawn reports,
        // and the worktree path a resumed one reports - must converge on
        // the worker's worktree.
        for cwd_raw in ["/tmp/forge-session", "/tmp/forge-session/.claude/worktrees/probe-a"] {
            let cwd = Path::new(cwd_raw);
            assert_eq!(
                surface.session(&worker, cwd).scan_cwd,
                workspace.git_scan_cwd_for_session(&worker, cwd),
                "a git worker's scan cwd must be the workspace's own answer for {cwd_raw}"
            );
            assert_eq!(
                surface.session(&worker, cwd).scan_cwd,
                PathBuf::from("/tmp/forge-session/.claude/worktrees/probe-a"),
                "a git worker must scan its worktree, not the cwd it was handed"
            );
        }

        let lead = SessionSlot::lead("TestOrg", "forge");
        let cwd = Path::new("/tmp/elsewhere");
        let state = surface.session(&lead, cwd);
        assert_eq!(
            state.scan_cwd,
            PathBuf::from("/tmp/elsewhere"),
            "a session that is not a git worker must scan the cwd it was handed"
        );
        assert_eq!(state.slot, lead, "the state must name the seat it was asked about");
    }
}
