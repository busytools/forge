//! The narrow workspace surface the review-conversation tools call, plus
//! the production impl on [`Workspace`] and a mock for unit tests.
//!
//! Mirrors [`crate::mcp::peers::facade`]: the four `Tool` impls hold an
//! `Arc<dyn ReviewFacade>` so tests drive them with a
//! [`MockReviewFacade`] instead of a live `Workspace`. [`resolve_scope`]
//! maps the caller to its `(project, branch)` review scope - project via
//! the shared [`crate::mcp::caller_context`], branch via a git query on
//! the caller's cwd - matching the key the `/diff` overlay persists under.
//!
//! [`resolve_scope`]: ReviewFacade::resolve_scope

use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use forge_primitives::git_diff::RepoGate;
use forge_primitives::review::ReviewStatus;

use crate::SessionKey;
use crate::mcp::caller_context::CallerContext;
use crate::mcp::review::{ReviewDetail, ReviewSummary};
use crate::workspace::Workspace;

/// The caller's resolved review context. `(project, branch)` is the store
/// key both the `/diff` overlay and these tools address; `author_label` is
/// how a `review__reply` from this caller is attributed in the thread;
/// `caller` is the session whose turn buffer accumulates the activity so
/// the reviewer gets one batched notice at turn end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewScope {
    pub project: String,
    pub branch: String,
    pub author_label: String,
    pub caller: SessionKey,
}

/// Which step of [`ReviewFacade::resolve_scope`]'s chain failed. One
/// variant per step so the caller-facing [`Self::message`] states only
/// what was observed: the prior single string asserted a detached HEAD
/// for every failure, and workers chased their own HEAD over a step that
/// had failed earlier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScopeError {
    /// The workspace is gone (forge shutting down mid-call).
    WorkspaceGone,
    /// The caller's session isn't registered under any loaded project.
    UnknownCaller,
    /// The caller's project resolved, but the workspace holds no cwd for
    /// the session.
    SessionCwdUnknown,
    /// The checkout forge derived for this session isn't on disk.
    ScanDirMissing { scan_cwd: PathBuf },
    /// The dir is there but git reported no branch for it. `git
    /// rev-parse` exits 128 both for a non-work-tree and for its own
    /// failures; the scanner separates them with a repo-existence
    /// probe, so `gate` distinguishes a genuine non-repo
    /// (`NotARepo`) from a sick scanner (`ScannerFailed`) rather than
    /// just recording that git said nothing usable.
    NoBranchFromGit { scan_cwd: PathBuf, gate: RepoGate },
    /// The inspected checkout is a repo, on a detached HEAD.
    DetachedHead { scan_cwd: PathBuf },
}

impl ScopeError {
    /// The failing step. Filter the WARN by field, not by bare name -
    /// `grep '"event_name":"review_scope_unresolved"'`; the plain name
    /// also matches the log's record of the command that greps for it.
    fn step(&self) -> &'static str {
        match self {
            Self::WorkspaceGone => "workspace_upgrade",
            Self::UnknownCaller => "caller_context",
            Self::SessionCwdUnknown => "session_cwd",
            Self::ScanDirMissing { .. } => "scan_dir",
            Self::NoBranchFromGit { .. } => "git_branch",
            Self::DetachedHead { .. } => "git_head",
        }
    }

    /// The checkout the git step inspected, for the steps that got that
    /// far.
    fn scan_cwd(&self) -> Option<&Path> {
        match self {
            Self::WorkspaceGone | Self::UnknownCaller | Self::SessionCwdUnknown => None,
            Self::ScanDirMissing { scan_cwd }
            | Self::NoBranchFromGit { scan_cwd, .. }
            | Self::DetachedHead { scan_cwd } => Some(scan_cwd),
        }
    }

    /// What git reported at the branch read, for the WARN line. `None`
    /// for the steps that never got to git.
    fn gate(&self) -> Option<RepoGate> {
        match self {
            Self::NoBranchFromGit { gate, .. } => Some(*gate),
            Self::DetachedHead { .. } => Some(RepoGate::InRepo),
            _ => None,
        }
    }

    /// What the tool tells the caller. Each states the observation and
    /// stops; only [`Self::DetachedHead`] mentions a detached HEAD, and
    /// every path-bearing variant names the checkout it inspected.
    pub fn message(&self) -> String {
        match self {
            Self::WorkspaceGone => {
                "forge is shutting down, so your review scope could not be resolved.".to_owned()
            }
            Self::UnknownCaller => "your session is not registered under any forge project, so \
                 there is no project to key reviews by."
                .to_owned(),
            Self::SessionCwdUnknown => "forge has no working directory recorded for your session, \
                 so it cannot tell which checkout you are in."
                .to_owned(),
            Self::ScanDirMissing { scan_cwd } => {
                format!("the checkout forge has you in, {}, is not on disk.", scan_cwd.display())
            }
            Self::NoBranchFromGit { scan_cwd, .. } => format!(
                "git reported no branch for {} - it is either not a work tree or the git call \
                 failed; forge's log carries git's own error.",
                scan_cwd.display(),
            ),
            Self::DetachedHead { scan_cwd } => format!(
                "the checkout at {} is on a detached HEAD, so it has no branch name to key \
                 reviews by.",
                scan_cwd.display(),
            ),
        }
    }
}

/// Narrow surface the review-conversation tools depend on. `resolve_scope`
/// is async (it shells out to git for the branch); the store ops are
/// synchronous over the resolved scope.
#[async_trait]
pub trait ReviewFacade: Send + Sync {
    /// Resolve the caller to its review scope, or the step that failed.
    async fn resolve_scope(&self, caller: &SessionKey) -> Result<ReviewScope, ScopeError>;

    /// `review__list` rows for the scope's branch, newest review first.
    fn list(&self, scope: &ReviewScope) -> Result<Vec<ReviewSummary>, String>;

    /// Branches in the scope's project that hold submitted reviews. Tells
    /// [`Self::list`]'s branch-scoped answer apart from the project's
    /// whole set, on an empty result and a populated one alike.
    fn branches_with_reviews(&self, scope: &ReviewScope) -> Vec<String>;

    /// `review__get` detail for one review, or `None` when it isn't on the
    /// scope's branch.
    fn get(&self, scope: &ReviewScope, review_id: &str) -> Result<Option<ReviewDetail>, String>;

    /// Append a reply to `comment_id`; returns the thread's status after
    /// the append. `Err` when `comment_id` isn't in this scope.
    fn reply(
        &self,
        scope: &ReviewScope,
        comment_id: &str,
        text: &str,
        at: &str,
    ) -> Result<ReviewStatus, String>;

    /// Mark `comment_id` Resolved. `Err` when it isn't in this scope.
    fn resolve(&self, scope: &ReviewScope, comment_id: &str) -> Result<(), String>;
}

/// Production impl over a `Weak<Workspace>` (the same cycle-breaking shape
/// [`crate::mcp::peers::facade::ProdWorkspaceFacade`] uses).
pub struct ProdReviewFacade(pub Weak<Workspace>);

impl ProdReviewFacade {
    /// Construct from a strong reference, downgrading immediately.
    pub fn from_arc(workspace: &Arc<Workspace>) -> Arc<dyn ReviewFacade> {
        Arc::new(Self(Arc::downgrade(workspace)))
    }
}

/// The one WARN a failed scope resolution emits, carrying every field
/// that discriminates the failing step. This is the line to reach for
/// when a caller reports it cannot read its reviews.
fn warn_unresolved(
    caller: &SessionKey,
    cx: Option<&CallerContext>,
    cwd_raw: Option<&str>,
    error: ScopeError,
) -> ScopeError {
    tracing::warn!(
        target: "forge_workspace::review",
        event_name = "review_scope_unresolved",
        step = error.step(),
        caller = %caller.as_str(),
        project = cx.map_or("-", |c| c.project_name.as_str()),
        worker_label = cx.and_then(|c| c.worker_label.as_deref()).unwrap_or("-"),
        cwd_raw = cwd_raw.unwrap_or("-"),
        scan_cwd = %error.scan_cwd().unwrap_or(Path::new("-")).display(),
        gate = ?error.gate(),
        message = %error.message(),
    );
    error
}

#[async_trait]
impl ReviewFacade for ProdReviewFacade {
    async fn resolve_scope(&self, caller: &SessionKey) -> Result<ReviewScope, ScopeError> {
        let Some(ws) = self.0.upgrade() else {
            return Err(warn_unresolved(caller, None, None, ScopeError::WorkspaceGone));
        };
        let Some(cx) = crate::mcp::caller_context::caller_context(&ws, caller) else {
            return Err(warn_unresolved(caller, None, None, ScopeError::UnknownCaller));
        };
        // The branch keys the review store; resolve it from the caller's
        // git dir the same way the /diff overlay does. A worker's git dir
        // is its worktree, so route the raw cwd through the scan-dir
        // adjustment before querying.
        let Some(cwd_raw) = ws.cwd_for_session(caller) else {
            return Err(warn_unresolved(caller, Some(&cx), None, ScopeError::SessionCwdUnknown));
        };
        let scan_cwd = ws.git_scan_cwd_for_session(caller, Path::new(&cwd_raw));
        let fail = |error| Err(warn_unresolved(caller, Some(&cx), Some(cwd_raw.as_str()), error));
        if !scan_cwd.is_dir() {
            return fail(ScopeError::ScanDirMissing { scan_cwd });
        }
        let branch = match forge_agent::env::git_diff::current_branch(&scan_cwd).await {
            Ok(Some(branch)) => branch,
            Ok(None) => return fail(ScopeError::DetachedHead { scan_cwd }),
            Err(gate) => return fail(ScopeError::NoBranchFromGit { scan_cwd, gate }),
        };
        let author_label = cx.worker_label.unwrap_or_else(|| "agent".to_owned());
        Ok(ReviewScope { project: cx.project_name, branch, author_label, caller: caller.clone() })
    }

    fn list(&self, scope: &ReviewScope) -> Result<Vec<ReviewSummary>, String> {
        let ws = self.0.upgrade().ok_or_else(|| "workspace unavailable".to_owned())?;
        ws.review_list(&scope.project, &scope.branch)
    }

    /// A failure here can only degrade a diagnostic, so it warns and
    /// reports none rather than failing the `review__list` that already
    /// succeeded.
    fn branches_with_reviews(&self, scope: &ReviewScope) -> Vec<String> {
        let Some(ws) = self.0.upgrade() else {
            return Vec::new();
        };
        ws.review_branches(&scope.project).unwrap_or_else(|error| {
            tracing::warn!(
                target: "forge_workspace::review",
                event_name = "review_branch_probe_failed",
                project = %scope.project,
                %error,
            );
            Vec::new()
        })
    }

    fn get(&self, scope: &ReviewScope, review_id: &str) -> Result<Option<ReviewDetail>, String> {
        let ws = self.0.upgrade().ok_or_else(|| "workspace unavailable".to_owned())?;
        ws.review_get(&scope.project, &scope.branch, review_id)
    }

    fn reply(
        &self,
        scope: &ReviewScope,
        comment_id: &str,
        text: &str,
        at: &str,
    ) -> Result<ReviewStatus, String> {
        let ws = self.0.upgrade().ok_or_else(|| "workspace unavailable".to_owned())?;
        ws.review_reply(
            &scope.caller,
            &scope.project,
            &scope.branch,
            comment_id,
            &scope.author_label,
            text,
            at,
        )
    }

    fn resolve(&self, scope: &ReviewScope, comment_id: &str) -> Result<(), String> {
        let ws = self.0.upgrade().ok_or_else(|| "workspace unavailable".to_owned())?;
        ws.review_resolve(&scope.caller, &scope.project, &scope.branch, comment_id)
    }
}

/// Mock for the four Tool impls' unit tests. Preloads a scope + return
/// values and captures reply/resolve calls, so tests assert routing +
/// scope-rejection without a live `Workspace`.
#[cfg(test)]
pub struct MockReviewFacade {
    /// Resolved scope; an `Err` exercises the unresolved-scope path.
    pub scope: parking_lot::Mutex<Result<ReviewScope, ScopeError>>,
    pub summaries: parking_lot::Mutex<Vec<ReviewSummary>>,
    /// Branches the scope's project holds reviews on.
    pub review_branches: parking_lot::Mutex<Vec<String>>,
    pub detail: parking_lot::Mutex<Option<ReviewDetail>>,
    /// Captured `(comment_id, text)` reply calls.
    pub reply_calls: parking_lot::Mutex<Vec<(String, String)>>,
    /// Captured `comment_id` resolve calls.
    pub resolve_calls: parking_lot::Mutex<Vec<String>>,
    /// Status a successful `reply` returns.
    pub reply_status: parking_lot::Mutex<ReviewStatus>,
    /// When set, `reply` / `resolve` return this error (scope rejection).
    pub force_error: parking_lot::Mutex<Option<String>>,
}

#[cfg(test)]
impl MockReviewFacade {
    pub fn new() -> Self {
        Self {
            scope: parking_lot::Mutex::new(Ok(ReviewScope {
                project: "forge".to_owned(),
                branch: "feat".to_owned(),
                author_label: "implementer".to_owned(),
                caller: SessionKey::from_session_id("caller"),
            })),
            summaries: parking_lot::Mutex::new(Vec::new()),
            review_branches: parking_lot::Mutex::new(Vec::new()),
            detail: parking_lot::Mutex::new(None),
            reply_calls: parking_lot::Mutex::new(Vec::new()),
            resolve_calls: parking_lot::Mutex::new(Vec::new()),
            reply_status: parking_lot::Mutex::new(ReviewStatus::Addressed),
            force_error: parking_lot::Mutex::new(None),
        }
    }

    pub fn into_arc(self) -> Arc<dyn ReviewFacade> {
        Arc::new(self)
    }
}

#[cfg(test)]
#[async_trait]
impl ReviewFacade for MockReviewFacade {
    async fn resolve_scope(&self, _caller: &SessionKey) -> Result<ReviewScope, ScopeError> {
        self.scope.lock().clone()
    }

    fn list(&self, _scope: &ReviewScope) -> Result<Vec<ReviewSummary>, String> {
        Ok(self.summaries.lock().clone())
    }

    fn branches_with_reviews(&self, _scope: &ReviewScope) -> Vec<String> {
        self.review_branches.lock().clone()
    }

    fn get(&self, _scope: &ReviewScope, review_id: &str) -> Result<Option<ReviewDetail>, String> {
        Ok(self.detail.lock().clone().filter(|d| d.review_id == review_id))
    }

    fn reply(
        &self,
        _scope: &ReviewScope,
        comment_id: &str,
        text: &str,
        _at: &str,
    ) -> Result<ReviewStatus, String> {
        if let Some(err) = self.force_error.lock().clone() {
            return Err(err);
        }
        self.reply_calls.lock().push((comment_id.to_owned(), text.to_owned()));
        Ok(*self.reply_status.lock())
    }

    fn resolve(&self, _scope: &ReviewScope, comment_id: &str) -> Result<(), String> {
        if let Some(err) = self.force_error.lock().clone() {
            return Err(err);
        }
        self.resolve_calls.lock().push(comment_id.to_owned());
        Ok(())
    }
}

/// Drives [`ProdReviewFacade`] against a real (stub) `Workspace` so each
/// failure step is asserted where it is produced, not where a mock
/// replays it.
#[cfg(test)]
mod resolve_scope_tests {
    use super::{ProdReviewFacade, ReviewFacade, ReviewScope, ScopeError};
    use crate::SessionKey;
    use crate::workspace::Workspace;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Weak};

    #[test]
    fn branches_with_reviews_reports_the_projects_other_branches() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        let reviewer = SessionKey::from_session_id("lead-uuid");
        ws.submit_review("myproj", "feat/theirs", None, &[], reviewer.clone())
            .expect("submit theirs");
        ws.submit_review("other", "feat/elsewhere", None, &[], reviewer).expect("submit elsewhere");
        let scope = ReviewScope {
            project: "myproj".to_owned(),
            branch: "feat/mine".to_owned(),
            author_label: "implementer".to_owned(),
            caller: SessionKey::from_session_id("caller-uuid"),
        };
        assert_eq!(
            ProdReviewFacade(Arc::downgrade(&ws)).branches_with_reviews(&scope),
            vec!["feat/theirs".to_owned()],
            "only this project's review-bearing branches",
        );
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?} in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr),
        );
    }

    /// A repo at `dir` on `branch`, with one commit so HEAD resolves.
    /// `git init -b` needs git 2.28; CI runs 2.25, hence `symbolic-ref`.
    fn init_repo(dir: &Path, branch: &str) {
        git(dir, &["init", "-q"]);
        git(dir, &["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")]);
        git(dir, &["config", "user.email", "test@example.com"]);
        git(dir, &["config", "user.name", "Test"]);
        git(dir, &["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.join("README.md"), "hi\n").expect("write");
        git(dir, &["add", "."]);
        git(dir, &["commit", "-q", "-m", "init"]);
    }

    /// A worktree fork of the repo at `root`, at the path claude's
    /// `--worktree <label>` puts it, on branch `worktree-<label>`.
    fn add_worktree(root: &Path, label: &str) -> PathBuf {
        let path = root.join(".claude/worktrees").join(label);
        git(
            root,
            &["worktree", "add", "-q", "-b", &format!("worktree-{label}"), &path.to_string_lossy()],
        );
        path
    }

    /// A workspace holding one project rooted at `cwd`, with the returned
    /// caller registered as a catalog session whose cwd is that root.
    fn ws_with_session_cwd(cwd: &str) -> (Arc<Workspace>, SessionKey) {
        let (ws, _rx) = Workspace::testing_stub();
        ws.seed_test_project("myproj", cwd);
        let caller = SessionKey::from_session_id("caller-uuid");
        ws.record_connected_session(cwd, caller.as_str(), None);
        (ws, caller)
    }

    /// Register `label` as a live worker of the project rooted at
    /// `project_root`, with NO catalog row - the state every spawned
    /// worker is in (the boot scan hides worker-tagged sessions and the
    /// Connected handler skips the catalog mirror for workers), so the
    /// worker registry is the only place its cwd can come from.
    fn seed_worker(
        ws: &Arc<Workspace>,
        project_root: &str,
        label: &str,
        is_git_repo_at_spawn: bool,
    ) -> SessionKey {
        ws.seed_test_project("myproj", project_root);
        let key = ws
            .list_projects()
            .into_iter()
            .find(|v| v.name == "myproj")
            .map(|v| v.key)
            .expect("seeded project");
        let caller = SessionKey::from_session_id("worker-uuid");
        ws.insert_live_worker(
            &key,
            crate::WorkerEntry {
                label: label.to_owned(),
                charter: "build".to_owned(),
                session_key: caller.clone(),
                status: forge_primitives::WorkerLiveness::Running,
                spawned_at: std::time::SystemTime::UNIX_EPOCH,
                spawned_by_session_id: "lead".to_owned(),
                needs_tag: false,
                is_git_repo_at_spawn,
                diagnostic: None,
                kick: None,
            },
        );
        caller
    }

    async fn scope_err(ws: &Arc<Workspace>, caller: &SessionKey) -> ScopeError {
        ProdReviewFacade(Arc::downgrade(ws))
            .resolve_scope(caller)
            .await
            .expect_err("scope resolution must fail")
    }

    #[tokio::test]
    async fn workspace_gone_is_its_own_reason() {
        let err = ProdReviewFacade(Weak::new())
            .resolve_scope(&SessionKey::from_session_id("caller-uuid"))
            .await
            .expect_err("a dropped workspace fails");
        assert_eq!(err, ScopeError::WorkspaceGone);
        assert!(!err.message().contains("detached"), "{}", err.message());
    }

    #[tokio::test]
    async fn caller_outside_every_project_is_its_own_reason() {
        let (ws, _rx) = Workspace::testing_stub();
        let err = scope_err(&ws, &SessionKey::from_session_id("ghost-uuid")).await;
        assert_eq!(err, ScopeError::UnknownCaller);
        assert!(!err.message().contains("detached"), "{}", err.message());
    }

    /// A worker's cwd comes from the worker registry, not the sessions
    /// catalog - the catalog never holds a worker row. The review the
    /// worker must read is the one keyed to its worktree's branch, not
    /// the project's default branch.
    #[tokio::test]
    async fn worker_without_a_catalog_row_reads_its_worktree_branch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("myproj");
        std::fs::create_dir_all(&root).expect("mkdir");
        init_repo(&root, "main");
        add_worktree(&root, "pyth-review-fixes");

        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        let caller = seed_worker(&ws, &root.to_string_lossy(), "pyth-review-fixes", true);
        ws.submit_review(
            "myproj",
            "worktree-pyth-review-fixes",
            Some("round 1".to_owned()),
            &[],
            SessionKey::from_session_id("lead-uuid"),
        )
        .expect("submit review on the worker's branch");

        let facade = ProdReviewFacade(Arc::downgrade(&ws));
        let scope = facade.resolve_scope(&caller).await.expect("worker scope resolves");
        assert_eq!(scope.branch, "worktree-pyth-review-fixes");
        assert_eq!(scope.author_label, "pyth-review-fixes");
        assert_eq!(
            facade.list(&scope).expect("list").len(),
            1,
            "the worker reads the review filed on its worktree branch",
        );
    }

    /// A non-git worker never forked into a worktree, so the registry
    /// resolves it to the project root. The root here is not a repo, so
    /// git is what fails - the cwd step no longer does.
    #[tokio::test]
    async fn non_git_worker_without_a_catalog_row_resolves_the_project_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub();
        let caller = seed_worker(&ws, &dir.path().to_string_lossy(), "researcher", false);
        let err = scope_err(&ws, &caller).await;
        assert!(
            matches!(&err, ScopeError::NoBranchFromGit { scan_cwd, .. } if scan_cwd == dir.path()),
            "the cwd step must resolve to the project root: {err:?}",
        );
    }

    /// `git rev-parse` exits 128 outside a work tree, which the
    /// scanner's repo-existence probe resolves to `NotARepo`. The
    /// variant still covers both gates, so its message claims neither.
    #[tokio::test]
    async fn non_git_scan_dir_is_its_own_reason() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path().to_string_lossy().into_owned();
        let (ws, caller) = ws_with_session_cwd(&cwd);
        let err = scope_err(&ws, &caller).await;
        assert!(
            matches!(&err, ScopeError::NoBranchFromGit { scan_cwd, .. } if scan_cwd == dir.path()),
            "{err:?}",
        );
        assert!(err.message().contains(&cwd), "the message names the dir it inspected");
        assert!(!err.message().contains("detached"), "{}", err.message());
    }

    #[tokio::test]
    async fn a_scan_dir_that_is_not_on_disk_is_its_own_reason() {
        let dir = tempfile::tempdir().expect("tempdir");
        let gone = dir.path().join("gone");
        let cwd = gone.to_string_lossy().into_owned();
        let (ws, caller) = ws_with_session_cwd(&cwd);
        let err = scope_err(&ws, &caller).await;
        assert_eq!(err, ScopeError::ScanDirMissing { scan_cwd: gone });
        assert!(err.message().contains(&cwd), "the message names the dir it looked for");
        assert!(!err.message().contains("detached"), "{}", err.message());
    }

    #[tokio::test]
    async fn detached_head_names_the_checkout_it_inspected() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(dir.path(), "main");
        git(dir.path(), &["checkout", "-q", "--detach"]);
        let cwd = dir.path().to_string_lossy().into_owned();
        let (ws, caller) = ws_with_session_cwd(&cwd);
        let err = scope_err(&ws, &caller).await;
        assert_eq!(err, ScopeError::DetachedHead { scan_cwd: dir.path().to_path_buf() });
        assert!(err.message().contains("detached HEAD"), "{}", err.message());
        assert!(err.message().contains(&cwd), "the message names the checkout it inspected");
    }

    #[tokio::test]
    async fn a_named_branch_resolves_to_a_scope() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(dir.path(), "feat/wgpu-plotter");
        let cwd = dir.path().to_string_lossy().into_owned();
        let (ws, caller) = ws_with_session_cwd(&cwd);
        let scope = ProdReviewFacade(Arc::downgrade(&ws))
            .resolve_scope(&caller)
            .await
            .expect("a named branch resolves");
        assert_eq!(scope.project, "myproj");
        assert_eq!(scope.branch, "feat/wgpu-plotter");
        assert_eq!(scope.author_label, "agent", "a non-worker caller falls back to 'agent'");
        assert_eq!(scope.caller, caller);
    }
}
