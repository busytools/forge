//! WORKFLOWS on `App`: entries built from `Workflow` tool_use, the
//! task-id keyed progress / completion writers, and the all-terminal
//! auto-clear.

impl super::App {
    /// Active session's WORKFLOW entries.
    pub fn workflows(&self) -> &[crate::app::state::types::WorkflowEntry] {
        self.active_session().map_or(&[], |s| s.workflows.as_slice())
    }

    /// Mutable accessor for the active session's
    /// WORKFLOWS list. Auto-creates the pre-Connect bucket if
    /// missing.
    pub(crate) fn workflows_mut(&mut self) -> &mut Vec<crate::app::state::types::WorkflowEntry> {
        &mut self.active_bucket_mut().workflows
    }

    /// Insert / refresh a `WorkflowEntry` from a
    /// `Workflow` tool_use's parsed input. Idempotent: a matching
    /// `tool_use_id` refreshes `meta_name` / `meta_description`
    /// without touching `phases` / `status`. Returns true on new
    /// insertion.
    pub fn upsert_workflow_from_tool_input(
        &mut self,
        tool_use_id: &str,
        meta_name: String,
        meta_description: Option<String>,
    ) -> bool {
        // Replay seeds the terminal status, matching
        // `upsert_monitor_from_tool_input`. Nothing can move a replayed
        // entry off its seeded status: the resume walk is fed by
        // `synthesize_replay_messages`, which emits only User /
        // Assistant envelopes, so neither `TaskProgress` nor
        // `TaskUpdated` reaches the walk. Seeded `InProgress` a
        // historical workflow would read as running forever and hold
        // the WORKFLOWS section open for its completed siblings.
        let initial_status = if self.replay_in_progress {
            crate::app::state::types::WorkflowStatus::Completed
        } else {
            crate::app::state::types::WorkflowStatus::InProgress
        };
        let workflows = self.workflows_mut();
        if let Some(existing) = workflows.iter_mut().find(|w| w.tool_use_id == tool_use_id) {
            existing.meta_name = meta_name;
            existing.meta_description = meta_description;
            return false;
        }
        workflows.push(crate::app::state::types::WorkflowEntry {
            tool_use_id: tool_use_id.to_owned(),
            task_id: None,
            meta_name,
            meta_description,
            phases: Vec::new(),
            status: initial_status,
            final_result_summary: None,
            expanded_in_inspector: false,
        });
        true
    }

    /// Stamp `task_id` on a workflow entry (from
    /// `TaskStarted`'s task_id ↔ tool_use_id mapping). No-op when
    /// no entry matches or the entry already has a task_id.
    pub fn stamp_workflow_task_id(&mut self, tool_use_id: &str, task_id: String) {
        if let Some(entry) = self.workflows_mut().iter_mut().find(|w| w.tool_use_id == tool_use_id)
            && entry.task_id.is_none()
        {
            entry.task_id = Some(task_id);
        }
    }

    /// Apply a `workflow_progress` snapshot to the
    /// matching workflow (keyed by `task_id`). The wire snapshot is
    /// monotonic (start → progress → done), so the latest event
    /// authoritatively determines each phase's status.
    pub fn apply_workflow_progress_by_task_id(
        &mut self,
        task_id: &str,
        events: &[forge_primitives::WorkflowProgressEvent],
    ) {
        if let Some(entry) =
            self.workflows_mut().iter_mut().find(|w| w.task_id.as_deref() == Some(task_id))
        {
            entry.apply_workflow_progress(events);
        }
        self.clear_workflows_if_all_terminal();
    }

    /// Transition a workflow into the terminal
    /// `Completed` status (called from `TaskUpdated` terminal
    /// patch). Triggers the all-completed clear.
    pub fn set_workflow_completed_by_task_id(&mut self, task_id: &str) {
        if let Some(entry) =
            self.workflows_mut().iter_mut().find(|w| w.task_id.as_deref() == Some(task_id))
        {
            entry.status = crate::app::state::types::WorkflowStatus::Completed;
        }
        self.clear_workflows_if_all_terminal();
    }

    /// Drain the WORKFLOWS list once every entry has finished -
    /// matches the MONITORS / TODOs all-completed clear shape.
    pub fn clear_workflows_if_all_terminal(&mut self) {
        let workflows = self.workflows_mut();
        if !workflows.is_empty() && workflows.iter().all(|w| !w.is_in_progress()) {
            workflows.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::app::state::tests::make_test_app;
    use pretty_assertions::assert_eq;

    /// The replay seed must not leak into the live path: a workflow
    /// launched now has to render as in progress and keep the WORKFLOWS
    /// section open until the wire says otherwise. Seeding terminal
    /// unconditionally would drain the section at the first status flip
    /// of any sibling, while the workflow was still running.
    #[test]
    fn upsert_workflow_live_path_still_starts_in_progress() {
        let mut app = make_test_app();
        assert!(!app.replay_in_progress, "live default");
        app.upsert_workflow_from_tool_input("tu_live_wf", "nightly-sweep".to_owned(), None);
        let workflows = app.workflows();
        assert_eq!(workflows.len(), 1);
        assert_eq!(
            workflows[0].status,
            crate::app::state::types::WorkflowStatus::InProgress,
            "a live workflow starts in progress",
        );
    }
}
