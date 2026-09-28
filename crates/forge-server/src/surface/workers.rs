//! `workers()`: the live workers, per project.

use std::sync::Arc;

use forge_primitives::SessionSlot;
use forge_workspace::{ProjectKey, WorkerEntry, Workspace};

/// The live workers a view draws under their projects.
pub struct Workers {
    workspace: Arc<Workspace>,
}

/// One live worker as a view reads it, named rather than a tuple.
pub struct WorkerRef {
    /// The project the worker belongs to.
    pub project: ProjectKey,
    /// The worker's label, which is also its slot's label.
    pub label: String,
    /// Whether the project's path was a git repo at spawn, and so
    /// whether the worker runs in a worktree.
    pub is_git_repo_at_spawn: bool,
    /// Whether the worker's on-disk tag write has yet to land.
    pub needs_tag: bool,
}

impl Workers {
    pub(super) fn collect(workspace: Arc<Workspace>) -> Self {
        Self { workspace }
    }

    /// The live workers registered under `project`, empty when it has
    /// none.
    pub fn for_project(&self, project: &ProjectKey) -> Vec<WorkerEntry> {
        self.workspace.list_live_workers(project)
    }

    /// `slot`'s worker registration, or `None` when the slot is not a
    /// live worker - a project lead, or a closed worker.
    pub fn lookup(&self, slot: &SessionSlot) -> Option<WorkerRef> {
        self.workspace.worker_lookup_for_session(slot).map(
            |(project, label, is_git_repo_at_spawn, needs_tag)| WorkerRef {
                project,
                label,
                is_git_repo_at_spawn,
                needs_tag,
            },
        )
    }

    /// Every live worker's slot, across every project.
    pub fn all_keys(&self) -> Vec<SessionSlot> {
        self.workspace.all_live_worker_session_keys()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use forge_primitives::{SessionSlot, WorkerLiveness};
    use forge_workspace::{WorkerEntry, Workspace};

    use crate::surface::ViewSurface;

    fn stub_workspace() -> Arc<Workspace> {
        let (workspace, _updates) = Workspace::testing_stub();
        workspace
    }

    fn worker_row(project: &str, label: &str) -> WorkerEntry {
        WorkerEntry {
            label: label.to_owned(),
            charter: format!("charter for {label}"),
            slot: SessionSlot::worker("TestOrg", project, label),
            session_id: None,
            status: WorkerLiveness::Running,
            spawned_at: std::time::SystemTime::UNIX_EPOCH,
            spawned_by: SessionSlot::lead("TestOrg", project),
            needs_tag: false,
            is_git_repo_at_spawn: false,
            diagnostic: None,
            kick: None,
        }
    }

    /// Catches worker rows that come from anywhere but the registry:
    /// a per-project list that leaks across projects, a lookup that
    /// answers for a lead, or a key set missing a live worker.
    #[test]
    fn worker_reads_agree_with_the_workspace() {
        let workspace = stub_workspace();
        workspace.seed_test_project("forge", "/tmp/forge-workers");
        workspace.seed_test_project("other", "/tmp/forge-workers-other");
        let surface = ViewSurface::new(Arc::clone(&workspace));
        let roster = surface.roster();
        let project = roster.project_named("forge").expect("seeded project").key.clone();
        let elsewhere = roster.project_named("other").expect("seeded project").key.clone();
        workspace.insert_live_worker(&project, worker_row("forge", "probe-a"));

        let workers = surface.workers();

        assert_eq!(
            workers.for_project(&project).len(),
            workspace.list_live_workers(&project).len(),
            "for_project must return the project's own live workers"
        );
        assert_eq!(
            workers.for_project(&project).first().map(|w| w.label.clone()),
            Some("probe-a".to_owned()),
            "the seeded worker must be the row for_project returns"
        );
        assert!(
            workers.for_project(&elsewhere).is_empty(),
            "a project with no workers must list none"
        );

        let seeded = workspace.list_live_workers(&project).first().cloned().expect("seeded row");
        let lookup = workers.lookup(&seeded.slot).expect("a live worker is found by its slot");
        assert_eq!(lookup.label, seeded.label, "lookup must answer with the worker's own label");
        assert_eq!(lookup.project, project, "lookup must answer with the worker's own project");
        assert!(
            workers.lookup(&SessionSlot::lead("TestOrg", "forge")).is_none(),
            "a lead slot must not resolve as a worker"
        );
        assert_eq!(
            workers.all_keys(),
            workspace.all_live_worker_session_keys(),
            "all_keys must be the registry's own key set"
        );
        assert!(
            workers.all_keys().contains(&seeded.slot),
            "all_keys must carry the seeded worker's slot"
        );
    }
}
