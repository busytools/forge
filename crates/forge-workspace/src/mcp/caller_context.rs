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

use crate::SessionSlot;
use crate::target::ProjectKey;
use crate::views::ProjectView;
use crate::workspace::Workspace;

/// Resolved context for an incoming MCP caller.
#[derive(Debug)]
pub(crate) struct CallerContext {
    /// The project the caller belongs to.
    pub project_key: ProjectKey,
    /// Human-readable project metadata mirrored from [`ProjectView`].
    pub project_name: String,
    pub project_org: String,
    pub project_path: PathBuf,
    /// The project's lead slot. Always known: a project's lead is named
    /// by the triple its spawn stated, not by whichever catalog row
    /// currently happens to look like one.
    pub lead: SessionSlot,
    /// Whether an agent is pooled for that lead slot right now.
    pub lead_running: bool,
    /// True when `caller` itself IS the project's lead session.
    pub is_lead: bool,
    /// The caller's worker label when it is a live worker, else `None`
    /// (a lead or other session). Stamps + scopes a caller's crons.
    pub worker_label: Option<String>,
}

/// Resolve a caller's project context: the project the caller's slot
/// names, when `list_projects` still declares it. A caller whose slot
/// names no declared project resolves to nothing (a project dropped
/// from `forge.toml` since the session spawned).
pub(crate) fn caller_context(ws: &Workspace, caller: &SessionSlot) -> Option<CallerContext> {
    ws.list_projects().into_iter().find_map(|view| caller_context_in_view(ws, &view, caller))
}

/// Per-view resolution. File-private so the test mod can exercise it
/// against hand-constructed [`ProjectView`]s without going through
/// the catalog-driven [`Workspace::list_projects`] path.
fn caller_context_in_view(
    ws: &Workspace,
    view: &ProjectView,
    caller: &SessionSlot,
) -> Option<CallerContext> {
    if caller.org() != view.org || caller.project() != view.name {
        return None;
    }
    let live = ws.list_live_workers(&view.key);
    let worker_label = live.iter().find(|w| w.slot == *caller).map(|w| w.label.clone());
    let is_lead = worker_label.is_none() && caller.is_lead();
    let lead = SessionSlot::lead(&view.org, &view.name);
    let lead_running = ws.session_is_pooled(&lead);
    Some(CallerContext {
        project_key: view.key.clone(),
        project_name: view.name.clone(),
        project_org: view.org.clone(),
        project_path: view.path.clone(),
        lead,
        lead_running,
        is_lead,
        worker_label,
    })
}

#[cfg(all(test, feature = "test-helpers"))]
mod tests {
    use super::*;
    use crate::WorkerEntry;
    use forge_primitives::WorkerLiveness;
    use std::time::SystemTime;

    fn worker_entry(slot: SessionSlot) -> WorkerEntry {
        WorkerEntry {
            label: "reviewer".into(),
            charter: "review the diff".into(),
            slot,
            session_id: None,
            status: WorkerLiveness::Running,
            spawned_at: SystemTime::UNIX_EPOCH,
            spawned_by: SessionSlot::lead("me", "myproj"),
            needs_tag: false,
            is_git_repo_at_spawn: false,
            diagnostic: None,
            kick: None,
        }
    }

    fn fixture() -> (std::sync::Arc<Workspace>, ProjectView, SessionSlot, SessionSlot) {
        let (ws, _rx) = Workspace::testing_stub();
        let key = ProjectKey::new("myproj".to_owned());
        let lead = SessionSlot::lead("me", "myproj");
        let worker = SessionSlot::worker("me", "myproj", "reviewer");
        let view = ProjectView::new_for_test_with_org(
            key.clone(),
            "myproj",
            "/tmp/myproj",
            "me",
            Vec::new(),
            Vec::new(),
            vec![],
        );
        ws.insert_live_worker(&key, worker_entry(worker.clone()));
        (ws, view, lead, worker)
    }

    #[test]
    fn caller_context_resolves_worker_as_non_lead() {
        let (ws, view, lead, worker) = fixture();
        let cx = caller_context_in_view(&ws, &view, &worker)
            .expect("worker caller resolves to its project");
        assert_eq!(cx.project_name, "myproj");
        assert_eq!(cx.project_org, "me");
        assert_eq!(cx.project_key.as_str(), "myproj");
        assert!(!cx.is_lead, "worker is not the lead");
        assert_eq!(cx.worker_label.as_deref(), Some("reviewer"), "a live worker carries its label");
        assert_eq!(cx.lead, lead, "the project's lead slot is named by the triple");
        assert!(!cx.lead_running, "no lead is pooled in this fixture");
    }

    #[test]
    fn caller_context_resolves_lead_as_lead() {
        let (ws, view, lead, _) = fixture();
        let cx = caller_context_in_view(&ws, &view, &lead)
            .expect("lead caller resolves to its project");
        assert!(cx.is_lead, "lead caller flagged as lead");
        assert_eq!(cx.worker_label, None, "a lead has no worker label");
        assert_eq!(cx.lead, lead);
    }

    /// A caller whose slot names another project (or none) is not a
    /// member of this one, so it resolves to nothing.
    #[test]
    fn caller_context_returns_none_for_an_unrelated_caller() {
        let (ws, view, _, _) = fixture();
        let elsewhere = SessionSlot::worker("someone-else", "otherproj", "reviewer");
        assert!(
            caller_context_in_view(&ws, &view, &elsewhere).is_none(),
            "a caller under another project must return None",
        );
    }

    /// The lead is named by the project's own triple, so the answer does
    /// not depend on which catalog rows happen to be on disk: a project
    /// with no lead transcript still has a lead slot.
    #[test]
    fn caller_context_names_the_lead_slot_with_no_catalog_rows() {
        let (ws, _rx) = Workspace::testing_stub();
        let key = ProjectKey::new("myproj".to_owned());
        let worker = SessionSlot::worker("me", "myproj", "reviewer");
        ws.insert_live_worker(&key, worker_entry(worker.clone()));
        let view = ProjectView::new_for_test_with_org(
            key,
            "myproj",
            "/tmp/myproj",
            "me",
            Vec::new(),
            Vec::new(),
            vec![],
        );

        let cx = caller_context_in_view(&ws, &view, &worker).expect("worker resolves");
        assert_eq!(cx.lead, SessionSlot::lead("me", "myproj"));
    }
}
