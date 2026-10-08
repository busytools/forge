//! The issue mirror: rows become issues in the project's own tracker.
//!
//! One-way by design. Forge files a row once - an epic as a parent
//! issue, its tasks as sub-issues, every body carrying an idempotency
//! marker - and reads issue state back on a slow cadence so a close in
//! GitHub shows on the board. Nothing here writes back to the tracker:
//! two-way sync is the support tax of every tool that tried it.
//!
//! The real gate is the probe (`gh` present, a repo, issues enabled);
//! `issues = false` on a project turns it off, and the default is on,
//! because the user's ask is that everything is filed where it can be.

use std::path::Path;
use std::time::SystemTime;

use forge_agent::env::gh_issue::{IssueRef, IssueSupport};
use forge_primitives::tasks::{LinkKind, TaskId, TaskLink};

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
    async fn states(&self, cwd: &Path) -> Vec<(u64, String)>;
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

    async fn states(&self, cwd: &Path) -> Vec<(u64, String)> {
        forge_agent::env::gh_issue::list_states(cwd).await
    }
}

/// The marker every filed body carries, so a retry can find-before-create.
pub fn marker(project: &str, id: &TaskId) -> String {
    format!("<!-- forge-task:{project}:{} -->", id.as_str())
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
            let states = backend.states(&view.path).await;
            if states.is_empty() {
                continue;
            }
            for row in &rows {
                let Some(link) = issue_link(&row.task) else { continue };
                let Some(number) = link_number(link) else { continue };
                let Some((_, state)) = states.iter().find(|(n, _)| *n == number) else { continue };
                if link.state.as_deref() == Some(state.as_str()) {
                    continue;
                }
                self.set_link_state(&view.name, &row.task.id, &link.target, state);
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
    use forge_primitives::tasks::Task;
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
            Some(IssueRef {
                number,
                url: format!("https://example.invalid/issues/{number}"),
                state: "open".to_owned(),
            })
        }

        async fn states(&self, _cwd: &Path) -> Vec<(u64, String)> {
            self.states.lock().expect("lock").clone()
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
}
