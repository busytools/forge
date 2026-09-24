//! The task cluster on [`Workspace`]: the durable task store accessor
//! and its mutation helpers, a second `impl` block beside `crons.rs` so
//! every caller keeps its path.

use forge_primitives::tasks::{Task, TaskId};

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
        self.with_tasks_mut(|tasks| tasks.push(task));
    }

    /// Apply `f` to the task `id` in `project_name`, persist, and report
    /// whether it was found. Open to every session in the project.
    pub(crate) fn update_task(
        &self,
        project_name: &str,
        id: &TaskId,
        f: impl FnOnce(&mut Task),
    ) -> bool {
        self.with_tasks_mut(|tasks| {
            match tasks.iter_mut().find(|t| t.id == *id && t.project_name == project_name) {
                Some(task) => {
                    f(task);
                    task.updated_at = std::time::SystemTime::now();
                    true
                }
                None => false,
            }
        })
    }

    /// Remove the task `id` in `project_name` together with every
    /// descendant, persist, and report whether anything was removed.
    /// Backs `tasks__delete`. The cascade happens here rather than at the
    /// call site so a parent can never be deleted out from under children
    /// that would then have nobody to close them.
    pub(crate) fn remove_task_tree(&self, project_name: &str, id: &TaskId) -> bool {
        self.with_tasks_mut(|tasks| {
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
        })
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
    pub fn seed_test_task(&self, task: forge_primitives::tasks::Task) {
        self.push_task(task);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_primitives::tasks::{Task, TaskId, TaskStatus};
    use tempfile::tempdir;

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

    #[test]
    fn deleting_a_task_takes_its_children() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.push_task(sample_task_with_parent("epic", None, "forge"));
        ws.push_task(sample_task_with_parent("sub-a", Some("epic"), "forge"));
        ws.push_task(sample_task_with_parent("sub-b", Some("sub-a"), "forge"));
        let unrelated = sample_task("other-project", "elsewhere");
        ws.push_task(unrelated);
        assert!(ws.remove_task_tree("forge", &TaskId::from("epic")), "the tree is removed");
        let left = ws.tasks_for_project("forge");
        assert!(left.is_empty(), "children and grandchildren go with the parent");
        assert_eq!(ws.tasks_for_project("elsewhere").len(), 1, "another project is untouched");
    }
}
