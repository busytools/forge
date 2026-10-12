//! The issue mirror: rows become issues in the project's own tracker.
//!
//! One-way by design, with one write back. Forge files a row once - an
//! epic as a parent issue, its tasks as sub-issues, every body carrying
//! an idempotency marker - and reads issue state back on a slow cadence
//! so a close in GitHub shows on the board. The write back is a close:
//! when a row ends - completed, its tree closed, or the row gone - its
//! issue closes too, or every done row leaks an open issue. Everything
//! else stays one-way: two-way sync is the support tax of every tool
//! that tried it.
//!
//! The real gate is the probe (`gh` present, a repo, issues enabled);
//! `issues = false` on a project turns it off, and the default is on,
//! because the user's ask is that everything is filed where it can be.

use std::path::Path;
use std::time::SystemTime;

use forge_agent::env::gh_issue::{CloseReason, IssueRef, IssueRow, IssueSupport};
use forge_primitives::tasks::{LinkKind, TaskId, TaskLink, TaskStatus};

use crate::workspace::Workspace;

/// The tracker as the mirror drives it. A seam so tests watch filing
/// without a `gh` binary, and so a second tracker is an implementation
/// rather than a rewrite.
#[async_trait::async_trait]
pub trait IssueBackend: Send + Sync {
    async fn probe(&self, cwd: &Path) -> IssueSupport;
    async fn file(
        &self,
        cwd: &Path,
        title: &str,
        body: &str,
        parent: Option<u64>,
    ) -> Option<IssueRef>;
    async fn issues(&self, cwd: &Path) -> Vec<IssueRow>;
    /// Close one issue with `reason`. Answers whether it closed.
    async fn close(&self, cwd: &Path, number: u64, reason: CloseReason) -> bool;
}

/// The real backend: `gh`, run from the project's own directory.
pub struct GhBackend;

#[async_trait::async_trait]
impl IssueBackend for GhBackend {
    async fn probe(&self, cwd: &Path) -> IssueSupport {
        forge_agent::env::gh_issue::probe_issues(cwd).await
    }

    async fn file(
        &self,
        cwd: &Path,
        title: &str,
        body: &str,
        parent: Option<u64>,
    ) -> Option<IssueRef> {
        forge_agent::env::gh_issue::create_issue(cwd, title, body, parent).await
    }

    async fn issues(&self, cwd: &Path) -> Vec<IssueRow> {
        forge_agent::env::gh_issue::list_issues(cwd).await
    }

    async fn close(&self, cwd: &Path, number: u64, reason: CloseReason) -> bool {
        forge_agent::env::gh_issue::close_issue(cwd, number, reason).await
    }
}

/// The marker every filed body carries, so a retry can find-before-create.
pub fn marker(project: &str, id: &TaskId) -> String {
    format!("<!-- forge-task:{project}:{} -->", id.as_str())
}

/// The task id a body's marker names, when the marker names `project`.
fn marker_id(project: &str, body: &str) -> Option<TaskId> {
    let prefix = format!("<!-- forge-task:{project}:");
    let after = body.find(&prefix)? + prefix.len();
    let rest = &body[after..];
    let end = rest.find(" -->")?;
    Some(TaskId::from(&rest[..end]))
}

/// The issue link a row already carries, if any.
fn issue_link(task: &forge_primitives::tasks::Task) -> Option<&TaskLink> {
    task.links.iter().find(|link| link.kind == LinkKind::Issue)
}

/// The issue number in a link's target, for state matching and nesting.
fn link_number(link: &TaskLink) -> Option<u64> {
    link.target.rsplit('/').next()?.parse().ok()
}

impl Workspace {
    /// One pass of the mirror over every opted-in project: file what has
    /// no issue yet - roots first, so a child can nest under its
    /// parent's - then read issue states back onto the links.
    pub async fn issue_pass(&self, backend: &dyn IssueBackend, now: SystemTime) {
        for view in self.list_projects() {
            let Some(loaded) = self.config.projects.iter().find(|p| p.name == view.name) else {
                continue;
            };
            if !loaded.issues {
                continue;
            }
            if backend.probe(&view.path).await != IssueSupport::Supported {
                continue;
            }
            let rows = self.board_rows(&view.name, now, crate::board::DEFAULT_STALE_SECS);
            // Roots first: a child's sub-issue create needs its parent's
            // number, and a pass that files both needs the order.
            for row in &rows {
                if row.task.parent.is_some() || issue_link(&row.task).is_some() {
                    continue;
                }
                self.file_row(backend, &view.name, &view.path, &row.task, None, now).await;
            }
            for row in &rows {
                let Some(parent) = &row.task.parent else { continue };
                if issue_link(&row.task).is_some() {
                    continue;
                }
                let parent_number = self
                    .tasks_for_project(&view.name)
                    .iter()
                    .find(|task| task.id == *parent)
                    .and_then(issue_link)
                    .and_then(link_number);
                self.file_row(backend, &view.name, &view.path, &row.task, parent_number, now).await;
            }
            // Read state back; a reconcile is metadata, not movement, so
            // it never touches `updated_at`.
            let issues = backend.issues(&view.path).await;
            if issues.is_empty() {
                continue;
            }
            for row in &rows {
                let Some(link) = issue_link(&row.task) else { continue };
                let Some(number) = link_number(link) else { continue };
                let Some(issue) = issues.iter().find(|issue| issue.number == number) else {
                    continue;
                };
                if link.state.as_deref() == Some(issue.state.as_str()) {
                    continue;
                }
                self.set_link_state(&view.name, &row.task.id, &link.target, &issue.state);
            }
            // Close what ended. The marker in the body is the durable
            // record of a filing, so a row that is gone is still findable
            // through its issue; without this every done row leaks an
            // open issue.
            let Some(archived) = self.archived_rows(&view.name) else {
                // Skipping the closes beats misreading an archived row as
                // a gone one: the pass runs again in a few minutes.
                continue;
            };
            for issue in &issues {
                if issue.state != "open" {
                    continue;
                }
                let Some(id) = marker_id(&view.name, &issue.body) else { continue };
                let ended = match rows.iter().find(|row| row.task.id == id) {
                    Some(row) => match row.task.status {
                        TaskStatus::Completed => CloseReason::Completed,
                        // A failed or canceled row reads as not planned,
                        // like a row that is gone.
                        TaskStatus::Failed | TaskStatus::Canceled => CloseReason::NotPlanned,
                        // The row has not ended: its issue stays open.
                        _ => continue,
                    },
                    // A closed tree's rows live in the archive, each
                    // closing by its own end.
                    None => match archived.iter().find(|(at, _)| at == &id) {
                        Some((_, TaskStatus::Completed)) => CloseReason::Completed,
                        // A failed or canceled row in a closed tree, and
                        // any row the archive does not know: gone.
                        Some(_) | None => CloseReason::NotPlanned,
                    },
                };
                if !backend.close(&view.path, issue.number, ended).await {
                    continue;
                }
                // The board reads the close now rather than a pass later.
                if let Some(row) = rows.iter().find(|row| row.task.id == id)
                    && let Some(link) = issue_link(&row.task)
                {
                    self.set_link_state(&view.name, &id, &link.target, "closed");
                }
            }
        }
    }

    /// File one row and record the link, body carrying the marker.
    async fn file_row(
        &self,
        backend: &dyn IssueBackend,
        project: &str,
        cwd: &Path,
        task: &forge_primitives::tasks::Task,
        parent: Option<u64>,
        now: SystemTime,
    ) {
        let body =
            format!("{}\n\n{}", task.detail.as_deref().unwrap_or(""), marker(project, &task.id));
        let Some(filed) = backend.file(cwd, &task.subject, &body, parent).await else {
            return;
        };
        let id = task.id.clone();
        let target = filed.url.clone();
        let state = filed.state.clone();
        let _ = self.update_task(project, &id, forge_primitives::tasks::By::System, |row| {
            row.links.push(TaskLink {
                kind: LinkKind::Issue,
                label: Some(format!("#{}", filed.number)),
                target: target.clone(),
                state: Some(state.clone()),
                added_at: now,
            });
        });
    }

    /// `project`'s archived rows with their own ends, which is where a
    /// completed tree's rows end up. `None` when the store cannot be
    /// read: an unreadable archive is not an empty one, and the caller
    /// must not close a closed tree's rows as gone.
    fn archived_rows(&self, project: &str) -> Option<Vec<(TaskId, TaskStatus)>> {
        let guard = self.db.lock();
        let db = guard.as_ref()?;
        match crate::store::task_archive::list_for_project(db, project) {
            Ok(rows) => Some(rows.iter().map(|task| (task.id.clone(), task.status)).collect()),
            Err(error) => {
                tracing::warn!(
                    target: "forge_workspace::issues",
                    event_name = "task_archive_unreadable",
                    project = %project,
                    %error,
                    "the task archive could not be read; this pass closes no mirror issues",
                );
                None
            }
        }
    }

    /// Write one link's read state, without stamping the row as moved.
    pub(crate) fn set_link_state(&self, project: &str, id: &TaskId, target: &str, state: &str) {
        let changed = self.with_tasks_mut(|tasks| {
            let Some(task) =
                tasks.iter_mut().find(|task| task.id == *id && task.project_name == project)
            else {
                return false;
            };
            let Some(link) = task.links.iter_mut().find(|link| link.target == target) else {
                return false;
            };
            if link.state.as_deref() == Some(state) {
                return false;
            }
            link.state = Some(state.to_owned());
            true
        });
        if changed {
            self.announce_tasks_changed(project);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use super::*;
    use forge_primitives::tasks::{By, Task, TaskStatus};
    use tempfile::tempdir;

    const PROJECT: &str = "proj";

    fn epoch(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[derive(Default)]
    struct MockBackend {
        supported: bool,
        files: Mutex<Vec<(String, String, Option<u64>)>>,
        states: Mutex<Vec<(u64, String)>>,
        bodies: Mutex<Vec<(u64, String)>>,
        closes: Mutex<Vec<(u64, CloseReason)>>,
        refuse: Mutex<bool>,
    }

    #[async_trait::async_trait]
    impl IssueBackend for MockBackend {
        async fn probe(&self, _cwd: &Path) -> IssueSupport {
            if self.supported { IssueSupport::Supported } else { IssueSupport::Unsupported }
        }

        async fn file(
            &self,
            _cwd: &Path,
            title: &str,
            body: &str,
            parent: Option<u64>,
        ) -> Option<IssueRef> {
            let mut files = self.files.lock().expect("lock");
            let number = 100 + files.len() as u64;
            files.push((title.to_owned(), body.to_owned(), parent));
            // A filed issue is open until something says otherwise.
            self.states.lock().expect("lock").push((number, "open".to_owned()));
            self.bodies.lock().expect("lock").push((number, body.to_owned()));
            Some(IssueRef {
                number,
                url: format!("https://example.invalid/issues/{number}"),
                state: "open".to_owned(),
            })
        }

        async fn issues(&self, _cwd: &Path) -> Vec<IssueRow> {
            let states = self.states.lock().expect("lock").clone();
            let bodies = self.bodies.lock().expect("lock").clone();
            states
                .iter()
                .map(|(number, state)| IssueRow {
                    number: *number,
                    state: state.clone(),
                    body: bodies
                        .iter()
                        .find(|(at, _)| at == number)
                        .map(|(_, body)| body.clone())
                        .unwrap_or_default(),
                })
                .collect()
        }

        async fn close(&self, _cwd: &Path, number: u64, reason: CloseReason) -> bool {
            self.closes.lock().expect("lock").push((number, reason));
            !*self.refuse.lock().expect("lock")
        }
    }

    fn workspace_with_project(issues: bool) -> (tempfile::TempDir, std::sync::Arc<Workspace>) {
        let dir = tempdir().expect("tempdir");
        let forge_dir = dir.path().join("forge");
        std::fs::create_dir_all(&forge_dir).expect("forge dir");
        std::fs::write(
            forge_dir.join("forge.toml"),
            format!(
                r#"
[[orgs]]
name = "TestOrg"
accounts = ["Stargate"]

[[orgs.projects]]
name = "proj"
path = "/tmp/tp-issues"
model = "claude-sonnet-5"
issues = {issues}

[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#
            ),
        )
        .expect("write forge.toml");
        let workspace =
            std::sync::Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("workspace"));
        (dir, workspace)
    }

    fn row(id: &str, parent: Option<&str>) -> Task {
        Task {
            id: TaskId::from(id),
            project_name: PROJECT.to_owned(),
            subject: format!("subject {id}"),
            active_form: None,
            detail: Some("why".to_owned()),
            status: forge_primitives::tasks::TaskStatus::Pending,
            owner: None,
            parent: parent.map(TaskId::from),
            waiting_on: None,
            estimate: None,
            rank: None,
            verify: None,
            links: Vec::new(),
            attempt: 0,
            archived_at: None,
            created_at: epoch(0),
            updated_at: epoch(0),
        }
    }

    #[tokio::test]
    async fn a_root_row_is_filed_once_with_its_marker() {
        let (_dir, ws) = workspace_with_project(true);
        ws.seed_test_task(row("epic", None));
        let backend = MockBackend { supported: true, ..Default::default() };

        ws.issue_pass(&backend, epoch(100)).await;
        let files = backend.files.lock().expect("lock").clone();
        assert_eq!(files.len(), 1, "one issue: {files:?}");
        assert_eq!(files[0].0, "subject epic", "titled with the row");
        assert!(
            files[0].1.contains("<!-- forge-task:proj:epic -->"),
            "the body carries the marker: {}",
            files[0].1,
        );
        let stored = ws.tasks_for_project(PROJECT);
        let link = stored[0]
            .links
            .iter()
            .find(|link| link.kind == LinkKind::Issue)
            .expect("the link is recorded");
        assert_eq!(link.label.as_deref(), Some("#100"));
        assert_eq!(link.state.as_deref(), Some("open"));

        ws.issue_pass(&backend, epoch(200)).await;
        assert_eq!(
            backend.files.lock().expect("lock").len(),
            1,
            "a row that already has an issue is not filed again",
        );
    }

    #[tokio::test]
    async fn a_child_nests_under_its_parents_issue() {
        let (_dir, ws) = workspace_with_project(true);
        ws.seed_test_task(row("epic", None));
        ws.seed_test_task(row("sub-a", Some("epic")));
        let backend = MockBackend { supported: true, ..Default::default() };

        ws.issue_pass(&backend, epoch(100)).await;
        let files = backend.files.lock().expect("lock").clone();
        assert_eq!(files.len(), 2, "both rows filed: {files:?}");
        let (child_title, _, child_parent) = files
            .iter()
            .find(|(title, _, _)| title == "subject sub-a")
            .expect("the child was filed");
        assert_eq!(child_title, "subject sub-a");
        assert_eq!(*child_parent, Some(100), "the child nests under the epic's issue");
    }

    #[tokio::test]
    async fn a_project_with_issues_off_files_nothing() {
        let (_dir, ws) = workspace_with_project(false);
        ws.seed_test_task(row("epic", None));
        let backend = MockBackend { supported: true, ..Default::default() };

        ws.issue_pass(&backend, epoch(100)).await;
        assert!(backend.files.lock().expect("lock").is_empty(), "opted out");
    }

    /// A reconcile is metadata, not movement: the link's state moves and
    /// the row's `updated_at` does not, so the no-movement clock is not
    /// reset by reading GitHub.
    #[tokio::test]
    async fn reconcile_updates_a_links_state_without_moving_the_row() {
        let (_dir, ws) = workspace_with_project(true);
        ws.seed_test_task(row("epic", None));
        let backend = MockBackend { supported: true, ..Default::default() };
        ws.issue_pass(&backend, epoch(100)).await;
        let before = ws.tasks_for_project(PROJECT)[0].updated_at;

        *backend.states.lock().expect("lock") = vec![(100, "closed".to_owned())];
        ws.issue_pass(&backend, epoch(200)).await;
        let stored = ws.tasks_for_project(PROJECT);
        let link = stored[0].links.iter().find(|l| l.kind == LinkKind::Issue).expect("link");
        assert_eq!(link.state.as_deref(), Some("closed"), "the close shows on the board");
        assert_eq!(stored[0].updated_at, before, "and the row did not move");
    }

    /// Move a row by the same door the tools use.
    fn move_to(ws: &Workspace, id: &str, status: TaskStatus) {
        ws.update_task(PROJECT, &TaskId::from(id), By::System, |task| {
            task.status = status;
        })
        .expect("move")
        .expect("the row is there");
    }

    /// A completed row's issue closes as completed on the next pass, and
    /// a live row that has not completed keeps its issue.
    #[tokio::test]
    async fn a_completed_row_closes_its_issue_once() {
        let (_dir, ws) = workspace_with_project(true);
        ws.seed_test_task(row("epic", None));
        ws.seed_test_task(row("sub-a", Some("epic")));
        let backend = MockBackend { supported: true, ..Default::default() };
        ws.issue_pass(&backend, epoch(100)).await;
        assert!(
            backend.closes.lock().expect("lock").is_empty(),
            "nothing has ended, so nothing closes",
        );

        move_to(&ws, "sub-a", TaskStatus::Completed);
        ws.issue_pass(&backend, epoch(200)).await;
        assert_eq!(
            backend.closes.lock().expect("lock").clone(),
            vec![(101, CloseReason::Completed)],
            "the completed row's issue closes; the open epic's does not",
        );
        let stored = ws.tasks_for_project(PROJECT);
        let link = stored
            .iter()
            .find(|task| task.id == TaskId::from("sub-a"))
            .expect("row")
            .links
            .iter()
            .find(|link| link.kind == LinkKind::Issue)
            .expect("link");
        assert_eq!(link.state.as_deref(), Some("closed"), "the board reads the close at once");

        *backend.states.lock().expect("lock") =
            vec![(100, "open".to_owned()), (101, "closed".to_owned())];
        ws.issue_pass(&backend, epoch(300)).await;
        assert_eq!(backend.closes.lock().expect("lock").len(), 1, "a closed issue stays closed");
    }

    /// A failed or canceled row closes as not planned; a row still in
    /// progress or waiting keeps its issue.
    #[tokio::test]
    async fn a_terminal_row_closes_by_its_own_end() {
        let (_dir, ws) = workspace_with_project(true);
        ws.seed_test_task(row("epic", None));
        ws.seed_test_task(row("sub-a", Some("epic")));
        ws.seed_test_task(row("sub-b", Some("epic")));
        ws.seed_test_task(row("sub-c", Some("epic")));
        let backend = MockBackend { supported: true, ..Default::default() };
        ws.issue_pass(&backend, epoch(100)).await;

        move_to(&ws, "sub-a", TaskStatus::Failed);
        move_to(&ws, "sub-b", TaskStatus::Canceled);
        move_to(&ws, "sub-c", TaskStatus::InProgress);
        ws.issue_pass(&backend, epoch(200)).await;

        assert_eq!(
            backend.closes.lock().expect("lock").clone(),
            vec![(101, CloseReason::NotPlanned), (102, CloseReason::NotPlanned)],
            "a failed and a canceled row close as not planned; the epic and the in-progress row keep theirs",
        );
    }

    /// A closed tree closes each issue by its own row's end: a canceled
    /// child is not planned, not completed.
    #[tokio::test]
    async fn a_closed_tree_closes_each_issue_by_its_rows_end() {
        let (_dir, ws) = workspace_with_project(true);
        ws.seed_test_task(row("epic", None));
        ws.seed_test_task(row("sub-a", Some("epic")));
        let backend = MockBackend { supported: true, ..Default::default() };
        ws.issue_pass(&backend, epoch(100)).await;

        move_to(&ws, "sub-a", TaskStatus::Canceled);
        move_to(&ws, "epic", TaskStatus::Completed);
        assert!(ws.tasks_for_project(PROJECT).is_empty(), "the tree is archived");
        ws.issue_pass(&backend, epoch(200)).await;

        assert_eq!(
            backend.closes.lock().expect("lock").clone(),
            vec![(100, CloseReason::Completed), (101, CloseReason::NotPlanned)],
            "the epic's issue is completed and the canceled child's is not planned",
        );
    }

    /// A close the backend refuses writes nothing, and the next pass
    /// asks again.
    #[tokio::test]
    async fn a_refused_close_writes_nothing_and_retries() {
        let (_dir, ws) = workspace_with_project(true);
        ws.seed_test_task(row("epic", None));
        ws.seed_test_task(row("sub-a", Some("epic")));
        let backend = MockBackend { supported: true, ..Default::default() };
        ws.issue_pass(&backend, epoch(100)).await;

        move_to(&ws, "sub-a", TaskStatus::Completed);
        *backend.refuse.lock().expect("lock") = true;
        ws.issue_pass(&backend, epoch(200)).await;
        assert_eq!(backend.closes.lock().expect("lock").len(), 1, "the close was attempted");
        let stored = ws.tasks_for_project(PROJECT);
        let link = stored
            .iter()
            .find(|task| task.id == TaskId::from("sub-a"))
            .expect("row")
            .links
            .iter()
            .find(|link| link.kind == LinkKind::Issue)
            .expect("link");
        assert_eq!(
            link.state.as_deref(),
            Some("open"),
            "a refused close writes nothing to the link",
        );

        *backend.refuse.lock().expect("lock") = false;
        ws.issue_pass(&backend, epoch(300)).await;
        assert_eq!(backend.closes.lock().expect("lock").len(), 2, "and the next pass asks again");
    }

    /// A store that cannot be read is not an empty one: the close step
    /// skips the pass rather than close a closed tree's row as gone.
    #[tokio::test]
    async fn an_unreadable_archive_skips_the_close_step() {
        let (_dir, ws) = workspace_with_project(true);
        ws.seed_test_task(row("epic", None));
        let backend = MockBackend { supported: true, ..Default::default() };
        ws.issue_pass(&backend, epoch(100)).await;

        move_to(&ws, "epic", TaskStatus::Completed);
        assert!(ws.tasks_for_project(PROJECT).is_empty(), "the tree is archived");
        *ws.db.lock() = None;
        ws.issue_pass(&backend, epoch(200)).await;

        assert!(
            backend.closes.lock().expect("lock").is_empty(),
            "an unreadable archive must not close the epic's issue as a gone row's",
        );
    }

    /// A root completing archives its tree, and the archived rows' issues
    /// close as completed - the only way the epic's own issue ever ends.
    #[tokio::test]
    async fn a_closed_trees_issues_close_as_completed() {
        let (_dir, ws) = workspace_with_project(true);
        ws.seed_test_task(row("epic", None));
        let backend = MockBackend { supported: true, ..Default::default() };
        ws.issue_pass(&backend, epoch(100)).await;

        move_to(&ws, "epic", TaskStatus::Completed);
        assert!(ws.tasks_for_project(PROJECT).is_empty(), "the tree is archived");
        ws.issue_pass(&backend, epoch(200)).await;
        assert_eq!(
            backend.closes.lock().expect("lock").clone(),
            vec![(100, CloseReason::Completed)],
        );
    }

    /// A row that is gone closes its issue as not planned, once.
    #[tokio::test]
    async fn a_deleted_rows_issue_closes_as_not_planned() {
        let (_dir, ws) = workspace_with_project(true);
        ws.seed_test_task(row("epic", None));
        ws.seed_test_task(row("sub-a", Some("epic")));
        let backend = MockBackend { supported: true, ..Default::default() };
        ws.issue_pass(&backend, epoch(100)).await;

        ws.remove_task_tree(PROJECT, &TaskId::from("epic"));
        ws.issue_pass(&backend, epoch(200)).await;
        assert_eq!(
            backend.closes.lock().expect("lock").clone(),
            vec![(100, CloseReason::NotPlanned), (101, CloseReason::NotPlanned)],
            "both rows are gone, so both issues go",
        );

        *backend.states.lock().expect("lock") =
            vec![(100, "closed".to_owned()), (101, "closed".to_owned())];
        ws.issue_pass(&backend, epoch(300)).await;
        assert_eq!(backend.closes.lock().expect("lock").len(), 2, "and neither closes twice");
    }

    /// Only this project's markers are candidates: an issue with no marker
    /// and one naming another project are left alone.
    #[tokio::test]
    async fn closes_only_this_projects_marked_issues() {
        let (_dir, ws) = workspace_with_project(true);
        ws.seed_test_task(row("epic", None));
        let backend = MockBackend { supported: true, ..Default::default() };
        ws.issue_pass(&backend, epoch(100)).await;

        backend.bodies.lock().expect("lock").push((102, "no marker here".to_owned()));
        backend
            .bodies
            .lock()
            .expect("lock")
            .push((103, "<!-- forge-task:other:gone -->".to_owned()));
        backend
            .states
            .lock()
            .expect("lock")
            .extend([(102, "open".to_owned()), (103, "open".to_owned())]);
        ws.issue_pass(&backend, epoch(200)).await;

        assert!(
            backend.closes.lock().expect("lock").is_empty(),
            "an unmarked issue and another project's marker are not this pass's to close",
        );
    }
}
