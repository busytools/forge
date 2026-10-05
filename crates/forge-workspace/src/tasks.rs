//! The task cluster on [`Workspace`]: the durable task store accessor
//! and its mutation helpers, a second `impl` block beside `crons.rs` so
//! every caller keeps its path.

use forge_primitives::tasks::{Task, TaskId};

use crate::protocol::SessionUpdate;
use crate::workspace::Workspace;

impl Workspace {
    /// Lock the task list, apply `f`, and persist. Every mutation routes
    /// through here so the in-memory set and the store never diverge.
    pub(crate) fn with_tasks_mut<R>(&self, f: impl FnOnce(&mut Vec<Task>) -> R) -> R {
        let mut tasks = self.tasks.lock();
        let result = f(&mut tasks);
        if let Some(db) = self.db.lock().as_ref()
            && let Err(error) = crate::store::tasks::replace_all(db, &tasks)
        {
            tracing::error!(
                target: "forge_workspace::tasks",
                %error,
                "persisting tasks to the store failed; a task may be lost on restart",
            );
        }
        result
    }

    /// Append a task and persist. Backs `tasks__create`.
    pub(crate) fn push_task(&self, task: Task) {
        let project_name = task.project_name.clone();
        self.with_tasks_mut(|tasks| tasks.push(task));
        self.announce_tasks_changed(&project_name);
    }

    /// Apply `f` to the task `id` in `project_name`, persist, and report
    /// whether it was found. Open to every session in the project.
    pub(crate) fn update_task(
        &self,
        project_name: &str,
        id: &TaskId,
        f: impl FnOnce(&mut Task),
    ) -> bool {
        let changed = self.with_tasks_mut(|tasks| {
            match tasks.iter_mut().find(|t| t.id == *id && t.project_name == project_name) {
                Some(task) => {
                    f(task);
                    task.updated_at = std::time::SystemTime::now();
                    true
                }
                None => false,
            }
        });
        if changed {
            self.announce_tasks_changed(project_name);
        }
        changed
    }

    /// Remove the task `id` in `project_name` together with every
    /// descendant, persist, and report whether anything was removed.
    /// Backs `tasks__delete`. The cascade happens here rather than at the
    /// call site so a parent can never be deleted out from under children
    /// that would then have nobody to close them.
    pub(crate) fn remove_task_tree(&self, project_name: &str, id: &TaskId) -> bool {
        let removed = self.with_tasks_mut(|tasks| {
            let mut doomed = vec![id.clone()];
            let mut cursor = 0;
            while cursor < doomed.len() {
                let current = doomed[cursor].clone();
                cursor += 1;
                for task in tasks.iter() {
                    if task.project_name == project_name
                        && task.parent.as_ref() == Some(&current)
                        && !doomed.contains(&task.id)
                    {
                        doomed.push(task.id.clone());
                    }
                }
            }
            let before = tasks.len();
            tasks.retain(|t| !(t.project_name == project_name && doomed.contains(&t.id)));
            tasks.len() != before
        });
        if removed {
            self.announce_tasks_changed(project_name);
        }
        removed
    }

    /// Tell every view the project's task set moved, as the set the write
    /// just left. It routes on the project's lead seat, which is the seat a
    /// project-scoped section belongs to; a name no project carries has no
    /// seat to route on and announces nothing.
    fn announce_tasks_changed(&self, project_name: &str) {
        let Some(key) = self.lead_slot_for_project(project_name) else {
            tracing::debug!(
                target: "forge_workspace::tasks",
                event_name = "tasks_changed_unroutable",
                project = %project_name,
                "a task write named a project no project carries; nothing was announced",
            );
            return;
        };
        let tasks = self.tasks_for_project(project_name);
        let _ = self.update_tx.send(SessionUpdate::TasksChanged { key, tasks });
    }

    /// The tasks registered for `project_name`. Backs `tasks__list` and
    /// the Inspector TASKS snapshot.
    pub fn tasks_for_project(&self, project_name: &str) -> Vec<Task> {
        self.tasks.lock().iter().filter(|t| t.project_name == project_name).cloned().collect()
    }
}

#[cfg(any(test, feature = "testing"))]
impl Workspace {
    /// Register a task directly, bypassing the MCP create path.
    /// Cross-crate test access to the otherwise `pub(crate)` task store
    /// so forge-tui can exercise the Inspector's `refresh_tasks`
    /// resolution against a seeded task.
    ///
    /// The store write without the announcement: a fixture seeds state, it
    /// does not perform the write a view is owed a frame for.
    pub fn seed_test_task(&self, task: forge_primitives::tasks::Task) {
        self.with_tasks_mut(|tasks| tasks.push(task));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SessionSlot, SessionUpdate};
    use forge_primitives::tasks::{Task, TaskId, TaskStatus};
    use tempfile::tempdir;

    /// The one `TasksChanged` on a test's update stream, as its key and the
    /// set it announces.
    fn next_tasks_changed(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<SessionUpdate>,
    ) -> (SessionSlot, Vec<Task>) {
        match rx.try_recv() {
            Ok(SessionUpdate::TasksChanged { key, tasks }) => (key, tasks),
            other => panic!("expected a TasksChanged on the stream, got {other:?}"),
        }
    }

    fn subjects(tasks: &[Task]) -> Vec<String> {
        tasks.iter().map(|task| task.subject.clone()).collect()
    }

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
            artifact: None,
            estimate: None,
            created_at: std::time::SystemTime::UNIX_EPOCH,
            updated_at: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    fn sample_task_with_parent(id: &str, parent: Option<&str>, project: &str) -> Task {
        Task { parent: parent.map(TaskId::from), ..sample_task(id, project) }
    }

    #[test]
    fn a_task_can_be_updated_by_a_session_that_does_not_own_it() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.push_task(sample_task("t-1", "forge"));
        let changed = ws.update_task("forge", &TaskId::from("t-1"), |t| {
            t.status = TaskStatus::Completed;
        });
        assert!(changed, "any session in the project may move a task");
        assert_eq!(ws.tasks_for_project("forge")[0].status, TaskStatus::Completed);
    }

    #[test]
    fn tasks_for_project_does_not_leak_across_projects() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.push_task(sample_task("t-1", "forge"));
        assert_eq!(ws.tasks_for_project("forge").len(), 1, "listed for its own project");
        assert!(ws.tasks_for_project("other").is_empty(), "scoped by project name");
    }

    #[test]
    fn a_write_persists_to_the_store() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        let db = crate::store::Db::open(&dir.path().join("db.redb")).expect("open db");
        ws.install_db_for_test(db);
        ws.push_task(sample_task("t-1", "forge"));
        let stored =
            crate::store::tasks::list(ws.db.lock().as_ref().expect("db installed")).expect("list");
        assert_eq!(stored.len(), 1, "the in-memory set and the store never diverge");
    }

    /// Deleting is durable, not just in-memory: the tree has to leave the
    /// store too, or the next boot reads it back.
    ///
    /// The two cross-project tasks are what pin the cascade's project
    /// guards, and nothing else reaches them: a same-project task outside
    /// the tree is never a cascade candidate, and its id is never doomed.
    /// One shares the doomed id, so the `retain` would take it without its
    /// own project check; the other is a cross-project child of a doomed id
    /// whose id collides with a task that has to survive here, so the
    /// collection would carry that collision into this project and the
    /// retain would then take the survivor.
    #[test]
    fn deleting_a_task_takes_its_children() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        let db = crate::store::Db::open(&dir.path().join("db.redb")).expect("open db");
        ws.install_db_for_test(db);
        ws.push_task(sample_task_with_parent("epic", None, "forge"));
        ws.push_task(sample_task_with_parent("sub-a", Some("epic"), "forge"));
        ws.push_task(sample_task_with_parent("sub-b", Some("sub-a"), "forge"));
        // A task in the SAME project but outside the tree: the cascade must
        // take the descendants and nothing else.
        ws.push_task(sample_task("sibling", "forge"));
        ws.push_task(sample_task("epic", "elsewhere"));
        ws.push_task(sample_task_with_parent("sibling", Some("epic"), "elsewhere"));
        assert!(ws.remove_task_tree("forge", &TaskId::from("epic")), "the tree is removed");
        let left = ws.tasks_for_project("forge");
        assert_eq!(
            left.iter().map(|t| t.id.clone()).collect::<Vec<_>>(),
            vec![TaskId::from("sibling")],
            "children and grandchildren go with the parent, and nothing else does",
        );
        let stored =
            crate::store::tasks::list(ws.db.lock().as_ref().expect("db installed")).expect("list");
        assert!(
            stored.iter().all(|t| t.project_name != "forge" || t.id == TaskId::from("sibling")),
            "the deletion reached the store, not only the in-memory set: {stored:?}",
        );
        assert_eq!(
            ws.tasks_for_project("elsewhere").len(),
            2,
            "another project keeps both of its tasks, one of them sharing a doomed id",
        );
    }

    /// Every task write announces the project's set on the project's lead
    /// seat, so the inspector's TASKS section pops on the frame that moved it
    /// rather than on the next unrelated read.
    ///
    /// The three writers are pinned together because they are one section's
    /// three doors. Mutants: drop any one emission (the drain is empty), or
    /// announce a set taken before the write (the announced set disagrees
    /// with the store).
    #[test]
    fn every_task_write_announces_the_projects_set() {
        let dir = tempdir().expect("tempdir");
        let (ws, mut rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-tasks");
        let lead = SessionSlot::lead("TestOrg", "proj");

        ws.push_task(sample_task("t-1", "proj"));
        let (key, tasks) = next_tasks_changed(&mut rx);
        assert_eq!(key, lead, "a create routes on the project's lead seat");
        assert_eq!(subjects(&tasks), ["subject t-1"], "and carries the set the core now holds");

        assert!(ws.update_task("proj", &TaskId::from("t-1"), |task| {
            task.status = TaskStatus::Completed;
        }));
        let (key, tasks) = next_tasks_changed(&mut rx);
        assert_eq!(key, lead, "an update routes the same way");
        assert_eq!(tasks.len(), 1, "and announces the whole set");
        assert_eq!(tasks[0].status, TaskStatus::Completed, "the write's own change included");

        assert!(ws.remove_task_tree("proj", &TaskId::from("t-1")));
        let (_, tasks) = next_tasks_changed(&mut rx);
        assert!(tasks.is_empty(), "a delete announces the set it left behind");
    }

    /// A write that changed nothing emits nothing: an update for an id no
    /// task carries and a delete of one nothing holds are both refusals, and
    /// an announcement would be news about a write that never happened.
    ///
    /// Mutant: announce unconditionally, where the refused pair is news the
    /// section never moved for.
    #[test]
    fn a_task_write_that_changed_nothing_announces_nothing() {
        let dir = tempdir().expect("tempdir");
        let (ws, mut rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-tasks-noop");

        assert!(
            !ws.update_task("proj", &TaskId::from("ghost"), |task| {
                task.status = TaskStatus::Completed;
            }),
            "precondition: no task carries the id, so the update is refused",
        );
        assert!(
            !ws.remove_task_tree("proj", &TaskId::from("ghost")),
            "precondition: nothing holds the id, so the delete is refused",
        );

        let announced = rx.try_recv();
        assert!(
            announced.is_err(),
            "a refused update and a refused delete announce nothing, and this was announced: \
             {announced:?}",
        );
    }
}
