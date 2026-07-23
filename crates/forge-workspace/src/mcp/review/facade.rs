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

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use forge_primitives::review::ReviewStatus;

use crate::SessionKey;
use crate::mcp::review::{ReviewDetail, ReviewSummary};
use crate::workspace::Workspace;

/// The caller's resolved review context. `(project, branch)` is the store
/// key both the `/diff` overlay and these tools address; `author_label` is
/// how a `review__reply` from this caller is attributed in the thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewScope {
    pub project: String,
    pub branch: String,
    pub author_label: String,
}

/// Narrow surface the review-conversation tools depend on. `resolve_scope`
/// is async (it shells out to git for the branch); the store ops are
/// synchronous over the resolved scope.
#[async_trait]
pub trait ReviewFacade: Send + Sync {
    /// Resolve the caller to its review scope, or `None` when the caller
    /// isn't in a known project or isn't on a named branch (detached HEAD,
    /// non-git dir).
    async fn resolve_scope(&self, caller: &SessionKey) -> Option<ReviewScope>;

    /// `review__list` rows for the scope's branch, newest review first.
    fn list(&self, scope: &ReviewScope) -> Result<Vec<ReviewSummary>, String>;

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

#[async_trait]
impl ReviewFacade for ProdReviewFacade {
    async fn resolve_scope(&self, caller: &SessionKey) -> Option<ReviewScope> {
        let ws = self.0.upgrade()?;
        let cx = crate::mcp::caller_context::caller_context(&ws, caller)?;
        // The branch keys the review store; resolve it from the caller's
        // git dir the same way the /diff overlay does. A worker's git dir
        // is its worktree, so route the raw cwd through the scan-dir
        // adjustment before querying.
        let cwd_raw = ws.session_cwd_for(caller)?;
        let scan_cwd = ws.git_scan_cwd_for_session(caller, std::path::Path::new(&cwd_raw));
        let branch = forge_agent::env::git_diff::current_branch(&scan_cwd).await?;
        let author_label = cx.worker_label.unwrap_or_else(|| "agent".to_owned());
        Some(ReviewScope { project: cx.project_name, branch, author_label })
    }

    fn list(&self, scope: &ReviewScope) -> Result<Vec<ReviewSummary>, String> {
        let ws = self.0.upgrade().ok_or_else(|| "workspace unavailable".to_owned())?;
        ws.review_list(&scope.project, &scope.branch)
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
        ws.review_reply(&scope.project, &scope.branch, comment_id, &scope.author_label, text, at)
    }

    fn resolve(&self, scope: &ReviewScope, comment_id: &str) -> Result<(), String> {
        let ws = self.0.upgrade().ok_or_else(|| "workspace unavailable".to_owned())?;
        ws.review_resolve(&scope.project, &scope.branch, comment_id)
    }
}

/// Mock for the four Tool impls' unit tests. Preloads a scope + return
/// values and captures reply/resolve calls, so tests assert routing +
/// scope-rejection without a live `Workspace`.
#[cfg(test)]
pub struct MockReviewFacade {
    /// Resolved scope; `None` exercises the unresolved-caller error path.
    pub scope: parking_lot::Mutex<Option<ReviewScope>>,
    pub summaries: parking_lot::Mutex<Vec<ReviewSummary>>,
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
            scope: parking_lot::Mutex::new(Some(ReviewScope {
                project: "forge".to_owned(),
                branch: "feat".to_owned(),
                author_label: "implementer".to_owned(),
            })),
            summaries: parking_lot::Mutex::new(Vec::new()),
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
    async fn resolve_scope(&self, _caller: &SessionKey) -> Option<ReviewScope> {
        self.scope.lock().clone()
    }

    fn list(&self, _scope: &ReviewScope) -> Result<Vec<ReviewSummary>, String> {
        Ok(self.summaries.lock().clone())
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
