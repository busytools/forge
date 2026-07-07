//! `CronFacade` - the seam between the cron MCP tools and workspace
//! state. The production impl ([`ProdCronFacade`]) resolves the caller's
//! project and drives the direct `Workspace` cron methods; the mock
//! records calls for tool tests.
//!
//! Cron-list mutations are direct `Workspace` methods (lock the `crons`
//! mutex + persist), NOT Command-bus dispatches - a cron entry is
//! workspace state, like the account-usage cache, not a session action.

use std::sync::{Arc, Weak};
use std::time::SystemTime;

use forge_primitives::cron::{CronEntry, CronId, CronKind};

use crate::SessionKey;
use crate::mcp::caller_context::caller_context;
use crate::mcp::cron::schedule;
use crate::workspace::Workspace;

/// Why `cron__create` failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CronCreateError {
    /// The caller couldn't be mapped to a project (transient race, or the
    /// session ended). Shouldn't happen for a live session.
    UnknownCallerProject,
    /// The 5-field cron expression didn't parse. Carries croner's message.
    InvalidExpression(String),
    /// The schedule has no upcoming occurrence - a run-once whose time has
    /// already passed, or a recurring expression that never matches.
    NoUpcomingOccurrence,
}

/// Why `cron__delete` failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CronDeleteError {
    UnknownCallerProject,
}

/// The cron tools' view of the workspace. Sync - cron-list mutations are
/// direct state writes with no async handler to await.
pub(crate) trait CronFacade: Send + Sync {
    /// Validate + register a cron for the caller's project: compute
    /// `next_fire`, persist, and return the new entry.
    fn create_cron(
        &self,
        caller: &SessionKey,
        kind: CronKind,
        prompt: String,
    ) -> Result<CronEntry, CronCreateError>;

    /// The crons registered for the caller's project.
    fn list_crons(&self, caller: &SessionKey) -> Vec<CronEntry>;

    /// Delete a cron by id within the caller's project. `Ok(true)` if an
    /// entry was removed, `Ok(false)` if no such cron belongs to the
    /// caller's project.
    fn delete_cron(&self, caller: &SessionKey, id: &CronId) -> Result<bool, CronDeleteError>;
}

/// Production facade over `Weak<Workspace>` (weak to avoid a cycle with
/// the MCP server the workspace owns).
pub(crate) struct ProdCronFacade {
    workspace: Weak<Workspace>,
}

impl ProdCronFacade {
    pub(crate) fn from_arc(workspace: &Arc<Workspace>) -> Arc<dyn CronFacade> {
        Arc::new(Self { workspace: Arc::downgrade(workspace) })
    }
}

impl CronFacade for ProdCronFacade {
    fn create_cron(
        &self,
        caller: &SessionKey,
        kind: CronKind,
        prompt: String,
    ) -> Result<CronEntry, CronCreateError> {
        let ws = self.workspace.upgrade().ok_or(CronCreateError::UnknownCallerProject)?;
        let cx = caller_context(&ws, caller).ok_or(CronCreateError::UnknownCallerProject)?;

        if let CronKind::Recurring(expr) = &kind {
            schedule::validate_cron_expr(expr).map_err(CronCreateError::InvalidExpression)?;
        }
        let now = SystemTime::now();
        let next_fire =
            schedule::next_fire_after(&kind, now).ok_or(CronCreateError::NoUpcomingOccurrence)?;

        let entry = CronEntry {
            id: CronId::from(uuid::Uuid::new_v4().to_string()),
            project_name: cx.project_name,
            kind,
            prompt,
            created_at: now,
            last_fire: None,
            next_fire,
            team_role: cx.worker_label,
        };
        ws.push_cron(entry.clone());
        Ok(entry)
    }

    fn list_crons(&self, caller: &SessionKey) -> Vec<CronEntry> {
        let Some(ws) = self.workspace.upgrade() else { return Vec::new() };
        let Some(cx) = caller_context(&ws, caller) else { return Vec::new() };
        // Symmetric: every caller sees only its own crons - a lead
        // (`worker_label == None`) the lead crons, a worker its own.
        ws.crons_for_project(&cx.project_name)
            .into_iter()
            .filter(|c| c.team_role == cx.worker_label)
            .collect()
    }

    fn delete_cron(&self, caller: &SessionKey, id: &CronId) -> Result<bool, CronDeleteError> {
        let ws = self.workspace.upgrade().ok_or(CronDeleteError::UnknownCallerProject)?;
        let cx = caller_context(&ws, caller).ok_or(CronDeleteError::UnknownCallerProject)?;
        Ok(ws.remove_cron_owned_by(&cx.project_name, id, cx.worker_label.as_deref()))
    }
}

/// Records calls + returns preloaded results so the tool tests can assert
/// the tool correctly parses args, resolves the caller, and surfaces
/// facade results/errors - without a real workspace.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct MockCronFacade {
    pub crons: parking_lot::Mutex<Vec<CronEntry>>,
    pub create_calls: parking_lot::Mutex<Vec<(SessionKey, CronKind, String)>>,
    pub create_result: parking_lot::Mutex<Option<Result<CronEntry, CronCreateError>>>,
    pub delete_calls: parking_lot::Mutex<Vec<(SessionKey, CronId)>>,
    pub delete_result: parking_lot::Mutex<Option<Result<bool, CronDeleteError>>>,
}

#[cfg(test)]
impl MockCronFacade {
    pub(crate) fn new() -> Self {
        Self::default()
    }
    pub(crate) fn into_arc(self) -> Arc<dyn CronFacade> {
        Arc::new(self)
    }
}

#[cfg(test)]
impl CronFacade for MockCronFacade {
    fn create_cron(
        &self,
        caller: &SessionKey,
        kind: CronKind,
        prompt: String,
    ) -> Result<CronEntry, CronCreateError> {
        self.create_calls.lock().push((caller.clone(), kind.clone(), prompt.clone()));
        if let Some(preloaded) = self.create_result.lock().clone() {
            return preloaded;
        }
        Ok(CronEntry {
            id: CronId::from("mock-cron-id"),
            project_name: "mock".to_owned(),
            kind,
            prompt,
            created_at: SystemTime::UNIX_EPOCH,
            last_fire: None,
            next_fire: SystemTime::UNIX_EPOCH,
            team_role: None,
        })
    }

    fn list_crons(&self, _caller: &SessionKey) -> Vec<CronEntry> {
        self.crons.lock().clone()
    }

    fn delete_cron(&self, caller: &SessionKey, id: &CronId) -> Result<bool, CronDeleteError> {
        self.delete_calls.lock().push((caller.clone(), id.clone()));
        self.delete_result.lock().clone().unwrap_or(Ok(false))
    }
}

/// Owner-scoping tests over a real `Workspace` (the mock above can't
/// exercise `caller_context`). A project with a lead session and a live
/// worker drives create/list/delete through `ProdCronFacade`.
#[cfg(test)]
mod prod_facade_tests {
    use super::*;
    use crate::WorkerEntry;
    use forge_primitives::WorkerLiveness;

    fn worker_entry(label: &str, session_id: &str) -> WorkerEntry {
        WorkerEntry {
            label: label.to_owned(),
            charter: "review".to_owned(),
            session_key: SessionKey::from_session_id(session_id),
            status: WorkerLiveness::Running,
            spawned_at: SystemTime::UNIX_EPOCH,
            spawned_by_session_id: "lead-uuid".to_owned(),
            needs_tag: false,
            is_git_repo_at_spawn: false,
            diagnostic: None,
            kick: None,
        }
    }

    fn fixture() -> (Arc<Workspace>, Arc<dyn CronFacade>, SessionKey, SessionKey) {
        let (ws, _rx) = Workspace::testing_stub();
        ws.seed_test_project_with_static_workers("myproj", "/tmp/b2-myproj", &[]);
        let key =
            ws.list_projects().into_iter().find(|v| v.name == "myproj").expect("seeded view").key;
        ws.record_connected_session("/tmp/b2-myproj", "lead-uuid", None);
        ws.insert_live_worker(&key, worker_entry("reviewer", "worker-uuid"));
        let facade = ProdCronFacade::from_arc(&ws);
        (ws, facade, SessionKey::from_session_id("lead-uuid"), SessionKey::from_session_id("worker-uuid"))
    }

    fn daily(prompt: &str) -> (CronKind, String) {
        (CronKind::Recurring("0 9 * * *".to_owned()), prompt.to_owned())
    }

    #[test]
    fn create_stamps_owner_and_list_delete_are_owner_scoped() {
        let (_ws, facade, lead, worker) = fixture();

        let (lk, lp) = daily("lead-standup");
        let lead_cron = facade.create_cron(&lead, lk, lp).expect("lead create");
        assert_eq!(lead_cron.team_role, None, "a lead cron is owner-less");
        let (wk, wp) = daily("worker-standup");
        let worker_cron = facade.create_cron(&worker, wk, wp).expect("worker create");
        assert_eq!(
            worker_cron.team_role.as_deref(),
            Some("reviewer"),
            "a worker cron is stamped with its label",
        );

        let worker_list = facade.list_crons(&worker);
        assert_eq!(worker_list.len(), 1, "the worker sees only its own cron");
        assert_eq!(worker_list[0].id, worker_cron.id);
        let lead_list = facade.list_crons(&lead);
        assert_eq!(lead_list.len(), 1, "the lead sees only lead crons");
        assert_eq!(lead_list[0].id, lead_cron.id);

        assert!(
            !facade.delete_cron(&worker, &lead_cron.id).expect("delete"),
            "a worker cannot delete a lead's cron",
        );
        assert!(
            !facade.delete_cron(&lead, &worker_cron.id).expect("delete"),
            "a lead cannot delete a worker's cron",
        );
        assert!(
            facade.delete_cron(&worker, &worker_cron.id).expect("delete"),
            "a worker deletes its own cron",
        );
    }
}
