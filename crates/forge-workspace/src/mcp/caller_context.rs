//! Shared "who's calling, and what's their project context?" helper
//! consumed by both the peers MCP and the workers MCP facades.
//!
//! Before this helper, peers `whoami` and workers `caller_project`
//! each carried their own walk over `list_projects()` and disagreed:
//! peers required the caller == the project's lead session; workers
//! walked both `live_workers` and the catalog. The peers walk was
//! the buggy one (#298 Cause 1) - workers couldn't call
//! `peers__whoami`. This helper consolidates the lookup so both
//! facades see the same answer.

use std::path::PathBuf;

use crate::SessionKey;
use crate::target::ProjectKey;
use crate::views::{ProjectView, SessionView};
use crate::workspace::Workspace;

/// Resolved context for an incoming MCP caller.
#[derive(Debug, Clone)]
pub(crate) struct CallerContext {
    /// The project the caller belongs to.
    pub project_key: ProjectKey,
    /// Human-readable project metadata mirrored from [`ProjectView`].
    pub project_name: String,
    pub project_org: String,
    pub project_path: PathBuf,
    /// The catalog row for the project's lead session, if one is
    /// currently registered. `None` when the project has no lead row
    /// yet (rare; right after spawn before the lead's `Connected`
    /// resolves, or after the lead session ended).
    pub lead_session_view: Option<SessionView>,
    /// True when `caller` itself IS the project's lead session.
    pub is_lead: bool,
}

/// Resolve a caller's project context. Walks [`Workspace::list_projects`]
/// looking for the caller as either:
///   1. A live worker (registered in `live_workers` for the project).
///   2. The project's lead session (first session in the catalog that
///      is NOT a live worker - same rule peers facade uses).
///   3. Any other session row in the project's catalog (a session
///      that isn't a live worker AND isn't the lead).
///
/// Returns the project the caller belongs to plus the lead's session
/// view + an `is_lead` flag. `None` when the caller doesn't appear in
/// any project (caller hasn't connected yet, or transient race).
pub(crate) fn caller_context(ws: &Workspace, caller: &SessionKey) -> Option<CallerContext> {
    ws.list_projects().into_iter().find_map(|view| caller_context_in_view(ws, &view, caller))
}

/// Per-view resolution. File-private so the test mod can exercise it
/// against hand-constructed [`ProjectView`]s without going through
/// the catalog-driven [`Workspace::list_projects`] path (same pattern
/// peers/facade.rs uses for [`crate::mcp::peers::facade`]'s
/// `lead_session_view` tests).
fn caller_context_in_view(
    ws: &Workspace,
    view: &ProjectView,
    caller: &SessionKey,
) -> Option<CallerContext> {
    let live = ws.list_live_workers(&view.key);
    let is_live_worker = live.iter().any(|w| w.session_key == *caller);
    // Lead = first catalog session that isn't a live worker. Mirrors
    // peers/facade.rs::lead_session_view and workers/facade.rs::caller_project.
    let lead_session_view =
        view.sessions.iter().find(|s| !live.iter().any(|w| w.session_key == s.session)).cloned();
    let is_lead = lead_session_view.as_ref().is_some_and(|lead| lead.session == *caller);
    // Accept the caller as a member of this project when they appear
    // as a live worker, the lead, or any other catalog session row
    // (workers/facade.rs::caller_project's broader catalog-match
    // semantics).
    if !is_live_worker && !is_lead && !view.sessions.iter().any(|s| s.session == *caller) {
        return None;
    }
    Some(CallerContext {
        project_key: view.key.clone(),
        project_name: view.name.clone(),
        project_org: view.org.clone(),
        project_path: view.path.clone(),
        lead_session_view,
        is_lead,
    })
}

#[cfg(all(test, feature = "test-helpers"))]
mod tests {
    use super::*;
    use crate::WorkerEntry;
    use forge_primitives::WorkerLiveness;
    use std::time::SystemTime;

    fn session(id: &str) -> SessionView {
        SessionView::new_for_test(SessionKey::from_session_id(id), id, true, None)
    }

    fn worker_entry(session_key: SessionKey) -> WorkerEntry {
        WorkerEntry {
            label: "reviewer".into(),
            charter: "review the diff".into(),
            session_key,
            status: WorkerLiveness::Running,
            spawned_at: SystemTime::UNIX_EPOCH,
            spawned_by_session_id: "lead-uuid".into(),
            needs_tag: false,
            is_git_repo_at_spawn: false,
            diagnostic: None,
            kick: None,
        }
    }

    fn fixture() -> (std::sync::Arc<Workspace>, ProjectView, SessionView, SessionView) {
        let (ws, _rx) = Workspace::testing_stub();
        let key = ProjectKey::new("myproj".to_owned());
        let lead = session("lead-uuid");
        let worker = session("worker-uuid");
        let view = ProjectView::new_for_test_with_org(
            key.clone(),
            "myproj",
            "/tmp/myproj",
            "me",
            Vec::new(),
            vec![lead.clone(), worker.clone()],
        );
        ws.insert_live_worker(&key, worker_entry(worker.session.clone()));
        (ws, view, lead, worker)
    }

    #[test]
    fn caller_context_resolves_worker_as_non_lead() {
        let (ws, view, lead, worker) = fixture();
        let cx = caller_context_in_view(&ws, &view, &worker.session)
            .expect("worker caller resolves to its project");
        assert_eq!(cx.project_name, "myproj");
        assert_eq!(cx.project_org, "me");
        assert_eq!(cx.project_key.as_str(), "myproj");
        assert!(!cx.is_lead, "worker is not the lead");
        assert_eq!(
            cx.lead_session_view.as_ref().map(|v| v.session.clone()),
            Some(lead.session),
            "lead_session_view points at the project's lead row",
        );
    }

    #[test]
    fn caller_context_resolves_lead_as_lead() {
        let (ws, view, lead, _) = fixture();
        let cx = caller_context_in_view(&ws, &view, &lead.session)
            .expect("lead caller resolves to its project");
        assert!(cx.is_lead, "lead caller flagged as lead");
        assert_eq!(cx.lead_session_view.as_ref().map(|v| v.session.clone()), Some(lead.session),);
    }

    #[test]
    fn caller_context_returns_none_for_unknown_caller() {
        let (ws, view, _, _) = fixture();
        let cx = caller_context_in_view(&ws, &view, &SessionKey::from_session_id("ghost-uuid"));
        assert!(cx.is_none(), "caller not in the project must return None");
    }
}
