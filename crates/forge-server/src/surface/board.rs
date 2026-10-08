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
    pub fn board_rows(&self, project: &str) -> Vec<BoardRow> {
        self.workspace.board_rows(
            project,
            SystemTime::now(),
            forge_workspace::board::DEFAULT_STALE_SECS,
        )
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
            owner: None,
            parent: None,
            waiting_on: None,
            estimate: None,
            rank: None,
            verify: None,
            links: Vec::new(),
            attempt: 0,
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

        let fleet = surface.fleet();
        let row = fleet.iter().find(|row| row.project == "forge").expect("the project's row");
        assert_eq!(row.queue, 1, "the ready unowned row is the queue");
        let _ = SessionSlot::lead("TestOrg", "forge");
    }
}
