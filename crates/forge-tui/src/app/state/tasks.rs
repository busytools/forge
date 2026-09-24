//! The Inspector TASKS snapshot: the active project's live task list
//! (`mcp__forge__tasks`), scoped to what the active session can act on.
//!
//! Refreshed on the ~1s ticker beside [`App::refresh_forge_crons`], so
//! the render reads a cheap cached `Vec` instead of resolving the
//! project and locking the workspace every frame. The row shape the
//! renderer reads is [`TaskRow`], resolved here rather than per frame.

use forge_primitives::tasks::{Task, TaskId, TaskStatus};

use super::App;

/// One TASKS row, resolved at refresh time.
pub struct TaskRow {
    pub id: TaskId,
    pub subject: String,
    /// The in-progress wording for a running row, else the subject.
    pub display: String,
    pub status: TaskStatus,
    pub owner_label: Option<String>,
    pub artifact: Option<String>,
    pub estimate: Option<String>,
    /// Lead rows: how many children are done, of how many. `None` for a
    /// worker row and for a top-level row with no children.
    pub rollup: Option<(usize, usize)>,
    /// Worker rows: the parent's subject. `None` when there is no parent
    /// or the parent is gone.
    pub breadcrumb: Option<String>,
}

/// Resolve `task`'s row against every task in the project, which is what
/// supplies its children's rollup and its parent's subject.
fn build_task_row(task: &Task, all: &[Task]) -> TaskRow {
    let display = match (&task.active_form, task.status) {
        (Some(active_form), TaskStatus::InProgress) => active_form.clone(),
        _ => task.subject.clone(),
    };
    let rollup = if task.parent.is_none() {
        let children: Vec<&Task> =
            all.iter().filter(|c| c.parent.as_ref() == Some(&task.id)).collect();
        (!children.is_empty()).then(|| {
            (children.iter().filter(|c| c.status == TaskStatus::Completed).count(), children.len())
        })
    } else {
        None
    };
    let breadcrumb = task.parent.as_ref().and_then(|id| {
        all.iter()
            .find(|parent| parent.id == *id && parent.project_name == task.project_name)
            .map(|parent| parent.subject.clone())
    });
    TaskRow {
        id: task.id.clone(),
        subject: task.subject.clone(),
        display,
        status: task.status,
        owner_label: task.owner.as_ref().map(|owner| owner.label().to_owned()),
        artifact: task.artifact.clone(),
        estimate: task.estimate.clone(),
        rollup,
        breadcrumb,
    }
}

impl App {
    /// Recompute the active session's task snapshot from the workspace.
    /// Called on the ~1s ticker so the Inspector reads a cheap cached
    /// `Vec` instead of locking the workspace every render. Scopes by the
    /// active tab's stamped project name, then by session kind: a lead
    /// takes the top-level rows with their rollups, a worker its own
    /// rows.
    pub fn refresh_tasks(&mut self) {
        let own_role = self.active_session_team_role();
        let all = match (self.active_project_name(), self.workspace.as_ref()) {
            (Some(name), Some(ws)) => ws.tasks_for_project(&name),
            _ => Vec::new(),
        };
        let mut scoped: Vec<Task> = match own_role {
            // A lead's campaign board: top-level rows, rolled up.
            None => all.iter().filter(|t| t.parent.is_none()).cloned().collect(),
            Some(label) => all
                .iter()
                .filter(|t| t.owner.as_ref().is_some_and(|o| o.label() == label))
                .cloned()
                .collect(),
        };
        // Running, then blocked, then pending, then completed. Stable, so
        // tasks of one status keep the order they were declared in.
        scoped.sort_by_key(|t| status_rank(t.status));
        // The detail overlay draws whatever the store holds, so an id the
        // store no longer has - a task another session deleted, or a
        // project this tab has left - closes it. Left open it would swallow
        // every key and click over nothing at all.
        if let Some(open) = self.task_detail.as_ref()
            && !all.iter().any(|t| t.id == *open)
        {
            self.task_detail = None;
        }
        self.ui_task_rows = scoped.iter().map(|t| build_task_row(t, &all)).collect();
        self.forge_tasks = scoped;
        self.forge_project_tasks = all;
    }
}

/// Where a status sorts in the TASKS section.
fn status_rank(status: TaskStatus) -> u8 {
    match status {
        TaskStatus::InProgress => 0,
        TaskStatus::Blocked => 1,
        TaskStatus::Pending => 2,
        TaskStatus::Completed => 3,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::App;
    use forge_primitives::tasks::{Task, TaskId, TaskStatus};
    use forge_workspace::SessionSlot;

    /// The project every fixture here builds on: seeded into the app's
    /// workspace and stamped on each task, so `active_project_name`
    /// resolves it the way a live session's would.
    const PROJECT: &str = "myproj";
    const PATH: &str = "/tmp/tui-tasks-myproj";
    /// The worker label the worker-scoped tests focus.
    const WORKER: &str = "agents-merge";

    fn owned_task(id: &str, subject: &str, owner: Option<&str>) -> Task {
        Task {
            id: TaskId::from(id),
            project_name: PROJECT.to_owned(),
            subject: subject.to_owned(),
            active_form: None,
            detail: None,
            status: TaskStatus::Pending,
            owner: owner.map(|label| SessionSlot::worker("TestOrg", PROJECT, label)),
            parent: None,
            artifact: None,
            estimate: None,
            created_at: std::time::SystemTime::UNIX_EPOCH,
            updated_at: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    fn task(id: &str, owner: Option<&str>) -> Task {
        owned_task(id, id, owner)
    }

    fn task_with_parent(id: &str, parent: Option<&str>) -> Task {
        Task { parent: parent.map(TaskId::from), ..owned_task(id, id, Some(WORKER)) }
    }

    pub(crate) fn app_with_tasks(tasks: Vec<Task>) -> App {
        let app = App::test_default();
        let ws = app.workspace.clone().expect("test workspace");
        ws.seed_test_project(PROJECT, PATH);
        for task in tasks {
            ws.seed_test_task(task);
        }
        app
    }

    /// A lead's view of a project whose store holds nothing, refreshed -
    /// so a render of it is the empty-store case.
    pub(crate) fn app_with_no_task_rows() -> App {
        let mut app = app_with_tasks(Vec::new());
        app.focus_lead_session();
        app.refresh_tasks();
        app
    }

    fn seeded_project_key(ws: &forge_workspace::Workspace) -> forge_workspace::ProjectKey {
        ws.list_projects().into_iter().find(|v| v.name == PROJECT).expect("seeded project").key
    }

    /// `n` top-level rows this worker owns, ids `t-1..t-n` and subjects
    /// `task 0..task n-1`, refreshed onto a lead's view. Past the five
    /// the old cap allowed, so the render fixtures can tell the two
    /// behaviours apart.
    pub(crate) fn app_with_task_rows(n: usize) -> App {
        let mut app = app_with_tasks(
            (0..n)
                .map(|i| owned_task(&format!("t-{}", i + 1), &format!("task {i}"), Some(WORKER)))
                .collect(),
        );
        app.focus_lead_session();
        app.refresh_tasks();
        app
    }

    /// Seven rows, one of them completed: over the old cap, which is the
    /// case that used to hide every finished row.
    pub(crate) fn app_with_task_rows_mixed_status() -> App {
        let mut tasks: Vec<Task> = (0..6)
            .map(|i| owned_task(&format!("t-{}", i + 1), &format!("pending {i}"), Some(WORKER)))
            .collect();
        tasks.push(Task {
            status: TaskStatus::Completed,
            ..owned_task("t-done", "the finished one", Some(WORKER))
        });
        let mut app = app_with_tasks(tasks);
        app.focus_lead_session();
        app.refresh_tasks();
        app
    }

    /// A worker's view of a parent task and one row under it, so the
    /// section renders its one dim parent line.
    pub(crate) fn app_with_task_rows_with_parent() -> App {
        let mut app = app_with_tasks(vec![
            task_with_parent("epic", None),
            task_with_parent("sub-a", Some("epic")),
        ]);
        app.focus_worker_session(WORKER);
        app.refresh_tasks();
        app
    }

    /// Three rows sharing `subject`: one running, one completed, one
    /// pending - so a single render shows the wrap and the truncation
    /// side by side.
    pub(crate) fn app_with_task_rows_with_subject(subject: &str) -> App {
        let mut app = app_with_tasks(vec![
            Task { status: TaskStatus::InProgress, ..owned_task("t-1", subject, Some(WORKER)) },
            Task { status: TaskStatus::Completed, ..owned_task("t-2", subject, Some(WORKER)) },
            Task { status: TaskStatus::Pending, ..owned_task("t-3", subject, Some(WORKER)) },
        ]);
        app.focus_lead_session();
        app.refresh_tasks();
        app
    }

    impl App {
        /// Focus the fixture project's lead session, so `refresh_tasks`
        /// resolves the project and takes a lead's scope.
        fn focus_lead_session(&mut self) {
            let key = SessionSlot::lead("TestOrg", PROJECT);
            self.sessions
                .insert(key.clone(), crate::app::session::UiSession::new(key.clone(), PROJECT));
            self.active_session_key = Some(key);
        }

        /// Focus a worker of the fixture project. The live-worker row is
        /// what gives the slot a `team_role`, which is the scope
        /// `refresh_tasks` narrows a worker's view by.
        fn focus_worker_session(&mut self, label: &str) {
            let ws = self.workspace.clone().expect("test workspace");
            ws.insert_live_worker(&seeded_project_key(&ws), worker_entry(label));
            let key = SessionSlot::worker("TestOrg", PROJECT, label);
            self.sessions
                .insert(key.clone(), crate::app::session::UiSession::new(key.clone(), PROJECT));
            self.active_session_key = Some(key);
        }

        /// Drop the worker's live row, so nothing resolves its slot any
        /// more - a despawned worker whose tasks are still in flight.
        fn mark_worker_dead(&mut self, label: &str) {
            let ws = self.workspace.clone().expect("test workspace");
            let key = SessionSlot::worker("TestOrg", PROJECT, label);
            ws.remove_worker_by_session_key(&key).expect("the worker was live");
        }
    }

    fn worker_entry(label: &str) -> forge_workspace::WorkerEntry {
        forge_workspace::WorkerEntry {
            label: label.to_owned(),
            charter: "merge peers and workers".to_owned(),
            slot: SessionSlot::worker("TestOrg", PROJECT, label),
            session_id: None,
            status: forge_primitives::WorkerLiveness::Running,
            spawned_at: std::time::SystemTime::UNIX_EPOCH,
            spawned_by: SessionSlot::lead("TestOrg", PROJECT),
            needs_tag: false,
            is_git_repo_at_spawn: false,
            diagnostic: None,
            kick: None,
        }
    }

    #[test]
    fn a_worker_sees_only_its_own_rows() {
        let mut app = app_with_tasks(vec![
            task("t-1", Some(WORKER)),
            task("t-2", Some("slack-delivery")),
            task("t-3", None),
        ]);
        app.focus_worker_session(WORKER);
        app.refresh_tasks();
        assert_eq!(app.forge_tasks.len(), 1, "one worker's rows, not its siblings'");
        assert_eq!(app.forge_tasks[0].id, TaskId::from("t-1"));
    }

    #[test]
    fn a_lead_sees_top_level_rows_with_their_rollup() {
        let mut app = app_with_tasks(vec![
            task_with_parent("epic", None),
            task_with_parent("sub-a", Some("epic")),
            task_with_parent("sub-b", Some("epic")),
        ]);
        app.focus_lead_session();
        app.refresh_tasks();
        let rows = &app.forge_tasks;
        assert_eq!(rows.len(), 1, "the lead sees the epic, not its children");
        assert_eq!(app.ui_task_rows[0].rollup, Some((0, 2)), "with 0 of 2 done");
    }

    #[test]
    fn an_empty_store_leaves_the_section_absent() {
        let mut app = app_with_tasks(Vec::new());
        app.focus_lead_session();
        app.refresh_tasks();
        assert!(app.forge_tasks.is_empty(), "nothing to render suppresses the whole section");
    }

    #[test]
    fn a_child_whose_parent_is_gone_carries_no_breadcrumb() {
        // A parent removed while its children survive.
        let mut app = app_with_tasks(vec![task_with_parent("orphan", Some("deleted-parent"))]);
        app.focus_worker_session(WORKER);
        app.refresh_tasks();
        assert_eq!(app.ui_task_rows.len(), 1, "the orphan still renders");
        assert_eq!(
            app.ui_task_rows[0].breadcrumb, None,
            "a gone parent renders as absent, not as an id or a blank",
        );
    }

    #[test]
    fn a_row_owned_by_a_dead_session_still_renders() {
        // A despawned worker's rows are still live work.
        let mut app = app_with_tasks(vec![task("t-1", Some(WORKER))]);
        // Control: the owner resolves, and its row carries its label.
        app.focus_worker_session(WORKER);
        app.refresh_tasks();
        assert_eq!(
            app.ui_task_rows[0].owner_label.as_deref(),
            Some(WORKER),
            "a live owner's row carries its label",
        );

        // ... and now the owner is gone.
        app.mark_worker_dead(WORKER);
        app.focus_lead_session();
        app.refresh_tasks();
        assert_eq!(app.forge_tasks.len(), 1, "a dead owner's row is not hidden");
        assert_eq!(
            app.ui_task_rows[0].owner_label.as_deref(),
            Some(WORKER),
            "the owner renders as the label it was",
        );
    }

    #[test]
    fn a_worker_row_carries_its_parents_subject_as_a_breadcrumb() {
        let mut app = app_with_tasks(vec![
            task_with_parent("epic", None),
            task_with_parent("sub-a", Some("epic")),
        ]);
        app.focus_worker_session(WORKER);
        app.refresh_tasks();
        assert_eq!(app.ui_task_rows.len(), 2, "the worker sees the rows it owns");
        let child = app.ui_task_rows.iter().find(|r| r.id == TaskId::from("sub-a"));
        assert_eq!(
            child.expect("the child row").breadcrumb.as_deref(),
            Some("epic"),
            "the parent's subject names the breadcrumb",
        );
    }

    #[test]
    fn a_running_row_shows_its_active_form_and_a_finished_one_rolls_up() {
        let mut app = app_with_tasks(vec![
            Task {
                active_form: Some("Merging peers".to_owned()),
                status: TaskStatus::InProgress,
                ..owned_task("epic", "Merge peers", Some(WORKER))
            },
            Task { status: TaskStatus::Completed, ..task_with_parent("sub-a", Some("epic")) },
        ]);
        app.focus_lead_session();
        app.refresh_tasks();
        assert_eq!(
            app.ui_task_rows[0].display, "Merging peers",
            "a running row renders the wording it is running as",
        );
        assert_eq!(app.ui_task_rows[0].rollup, Some((1, 1)), "one of one child is done");
    }

    #[test]
    fn an_open_detail_closes_when_its_task_leaves_the_store() {
        let mut app = app_with_task_rows(3);
        // `t-2` is in the store here; the id stands for a task another
        // session deletes between ticks.
        app.task_detail = Some(TaskId::from("t-2"));
        app.refresh_tasks();
        assert_eq!(
            app.task_detail,
            Some(TaskId::from("t-2")),
            "a task the store still holds keeps its detail open",
        );

        app.task_detail = Some(TaskId::from("deleted-elsewhere"));
        app.refresh_tasks();
        assert_eq!(
            app.task_detail, None,
            "an overlay drawing nothing must not go on swallowing every key and click",
        );
    }

    #[test]
    fn a_refresh_with_no_active_project_clears_the_caches() {
        let mut app = app_with_tasks(vec![task("t-1", Some(WORKER))]);
        app.focus_lead_session();
        app.refresh_tasks();
        assert_eq!(app.forge_tasks.len(), 1, "the fixture populated the cache");
        app.active_session_key = None;
        app.refresh_tasks();
        assert!(app.forge_tasks.is_empty(), "no active session leaves nothing to render");
        assert!(app.ui_task_rows.is_empty(), "and no rows either");
    }
}
