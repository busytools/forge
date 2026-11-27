//! `board()` and `fleet()`: the task board as a view reads it.
//!
//! Both forward the workspace's own answer - the marks are derived in ONE
//! place (forge-workspace's board module), and this surface passes them
//! through rather than recomputing anything, so no two surfaces can draw
//! a row differently.

use std::time::SystemTime;

use forge_workspace::board::{BoardRow, FleetRow};

use crate::surface::ViewSurface;

impl ViewSurface {
    /// `project`'s live rows, with their derived facts.
    ///
    /// Terminal rows are not here: a finished row is history, and the bulk
    /// board is work in play. They leave the moment they end, and the
    /// closed view is [`ViewSurface::archived_rows`].
    pub fn board_rows(&self, project: &str) -> Vec<BoardRow> {
        self.workspace
            .board_rows(project, SystemTime::now(), forge_workspace::board::DEFAULT_STALE_SECS)
            .into_iter()
            .filter(|row| !row.task.status.is_terminal())
            .collect()
    }

    /// `project`'s closed rows: the terminal rows still in the live set,
    /// then the archive. One read, because the toggle that opens it wants
    /// everything that has ended rather than the storage boundary between
    /// them; a restored row is still terminal and stays here, where the
    /// reopen door can reach it.
    pub fn archived_rows(&self, project: &str) -> Vec<forge_primitives::tasks::Task> {
        let mut rows: Vec<_> = self
            .workspace
            .tasks_for_project(project)
            .into_iter()
            .filter(|task| task.status.is_terminal())
            .collect();
        rows.extend(self.workspace.archived_tasks_for_project(project));
        rows
    }

    /// One row per project, for the fleet page.
    pub fn fleet(&self) -> Vec<FleetRow> {
        self.workspace.fleet_rows()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use forge_primitives::SessionSlot;
    use forge_primitives::tasks::{Task, TaskId, TaskStatus};
    use forge_workspace::Workspace;

    use crate::surface::ViewSurface;

    fn sample_task(id: &str, project: &str) -> Task {
        Task {
            id: TaskId::from(id),
            project_name: project.to_owned(),
            subject: format!("subject {id}"),
            active_form: None,
            detail: None,
            status: TaskStatus::Pending,
            kind: forge_primitives::tasks::TaskKind::Finite,
            owner: None,
            parent: None,
            waiting_on: None,
            request: None,
            estimate: None,
            cadence: None,
            rank: None,
            verify: None,
            attested: None,
            reserved: None,
            links: Vec::new(),
            evidence: None,
            version: 0,
            archived_at: None,
            created_at: std::time::SystemTime::UNIX_EPOCH,
            updated_at: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    /// Catches a board read that recomputes marks instead of forwarding
    /// the workspace's own answer, or that forgets the project scoping.
    #[test]
    fn the_board_verbs_forward_the_workspaces_own_answer() {
        let (workspace, _updates) = Workspace::testing_stub();
        workspace.seed_test_project("forge", "/tmp/forge-surface-board");
        workspace.seed_test_task(sample_task("t-1", "forge"));
        let surface = ViewSurface::new(Arc::clone(&workspace));

        let rows = surface.board_rows("forge");
        assert_eq!(rows.len(), 1, "the seeded row is on the board");
        assert!(rows[0].marks.ready, "a pending unowned row is ready");
        assert!(surface.board_rows("other").is_empty(), "and another project sees none");

        // A terminal row leaves the bulk board at once and is reachable
        // through the closed read instead.
        workspace
            .seed_test_task(Task { status: TaskStatus::Completed, ..sample_task("t-2", "forge") });
        assert_eq!(surface.board_rows("forge").len(), 1, "a finished row is not live work");
        let closed = surface.archived_rows("forge");
        assert_eq!(closed.len(), 1, "the closed read carries it: {closed:?}");
        assert_eq!(closed[0].id, TaskId::from("t-2"));
        assert!(closed[0].archived_at.is_none(), "it is closed, not archived yet");

        let fleet = surface.fleet();
        let row = fleet.iter().find(|row| row.project == "forge").expect("the project's row");
        assert_eq!(row.queue, 1, "the ready unowned row is the queue");
        let _ = SessionSlot::lead("TestOrg", "forge");
    }
}
