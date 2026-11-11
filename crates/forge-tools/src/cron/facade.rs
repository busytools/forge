//! `CronFacade` - the seam between the cron MCP tools and the subsystem.
//! The production impl ([`ProdCronFacade`]) resolves the caller's project
//! through its host and drives the [`Cron`] handle; the mock records calls
//! for tool tests.
//!
//! Cron-list mutations are direct state writes (lock the entry list +
//! persist), NOT Command-bus dispatches - a cron entry is workspace state,
//! like the account-usage cache, not a session action.

use std::sync::{Arc, Weak};
use std::time::SystemTime;

use forge_primitives::SessionSlot;
use forge_primitives::cron::{CronEntry, CronId, CronKind};

use crate::cron::schedule;
use crate::cron::subsystem::Cron;

/// The caller's project identity, as the facade scopes cron records by it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronCaller {
    pub project_name: String,
    pub worker_label: Option<String>,
}

/// What the facade needs from the workspace it is registered on: the
/// caller's project and worker label, resolved from `forge.toml` and the
/// worker registry.
pub trait CronFacadeHost: Send + Sync {
    fn caller(&self, caller: &SessionSlot) -> Option<CronCaller>;
}

/// Why `cron__create` failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CronCreateError {
    /// The caller couldn't be mapped to a project (transient race, or the
    /// session ended). Shouldn't happen for a live session.
    UnknownCallerProject,
    /// The 5-field cron expression didn't parse. Carries croner's message.
    InvalidExpression(String),
    /// The schedule has no upcoming occurrence - a run-once whose time has
    /// already passed, or a recurring expression that never matches.
    NoUpcomingOccurrence,
}

/// Why `cron__list` failed.
///
/// The same unresolvable caller is what `delete_cron` answers with its own
/// [`CronDeleteError::UnknownCallerProject`]; the variants stay per-verb
/// because a list cannot have "no such cron" or "not yours".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CronListError {
    /// The caller couldn't be mapped to a project (transient race, or the
    /// session ended). Shouldn't happen for a live session.
    UnknownCallerProject,
    /// This run's boot could not load the durable crons, so an empty list
    /// would claim the caller has none.
    Unavailable,
}

/// Why `cron__delete` failed. The last two are the two cases a refusal
/// keeps apart: an id the project holds under another session reads
/// differently from one the project never had.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CronDeleteError {
    /// The caller couldn't be mapped to a project (transient race, or the
    /// session ended). Shouldn't happen for a live session.
    UnknownCallerProject,
    /// No cron with that id in the caller's project.
    NoSuchCron,
    /// The id is in the caller's project but belongs to another session.
    NotOwnedByCaller,
}

/// The cron tools' view of the workspace. Sync - cron-list mutations are
/// direct state writes with no async handler to await.
pub trait CronFacade: Send + Sync {
    /// Validate + register a cron owned by the caller: compute
    /// `next_fire`, persist, and return the new entry. `description` is
    /// the caller-supplied human summary (already trimmed to `None` when
    /// blank).
    fn create_cron(
        &self,
        caller: &SessionSlot,
        kind: CronKind,
        prompt: String,
        description: Option<String>,
        wake: bool,
    ) -> Result<CronEntry, CronCreateError>;

    /// The crons the caller registered, within its project. An empty answer
    /// means the caller has none.
    fn list_crons(&self, caller: &SessionSlot) -> Result<Vec<CronEntry>, CronListError>;

    /// Delete the caller's own cron by id, within its project, and return
    /// the entry as it stood. A refusal says which case it is:
    /// [`CronDeleteError::NoSuchCron`] when the project has no such id,
    /// [`CronDeleteError::NotOwnedByCaller`] when it has one under
    /// another session.
    fn delete_cron(&self, caller: &SessionSlot, id: &CronId) -> Result<CronEntry, CronDeleteError>;
}

/// Production facade over the subsystem and its host, both weak to avoid a
/// cycle with the MCP server the workspace owns.
pub struct ProdCronFacade {
    cron: Weak<Cron>,
    host: Weak<dyn CronFacadeHost>,
}

impl ProdCronFacade {
    pub fn new(cron: Weak<Cron>, host: Weak<dyn CronFacadeHost>) -> Self {
        Self { cron, host }
    }

    fn parts(&self) -> Option<(Arc<Cron>, Arc<dyn CronFacadeHost>)> {
        Some((self.cron.upgrade()?, self.host.upgrade()?))
    }
}

impl CronFacade for ProdCronFacade {
    fn create_cron(
        &self,
        caller: &SessionSlot,
        kind: CronKind,
        prompt: String,
        description: Option<String>,
        wake: bool,
    ) -> Result<CronEntry, CronCreateError> {
        let (cron, host) = self.parts().ok_or(CronCreateError::UnknownCallerProject)?;
        let cx = host.caller(caller).ok_or(CronCreateError::UnknownCallerProject)?;

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
            description,
            created_at: now,
            last_fire: None,
            next_fire,
            team_role: cx.worker_label,
            wake,
        };
        cron.push_cron(entry.clone());
        Ok(entry)
    }

    fn list_crons(&self, caller: &SessionSlot) -> Result<Vec<CronEntry>, CronListError> {
        let (cron, host) = self.parts().ok_or(CronListError::UnknownCallerProject)?;
        let cx = host.caller(caller).ok_or(CronListError::UnknownCallerProject)?;
        if cron.durable_crons_unavailable() {
            return Err(CronListError::Unavailable);
        }
        // Symmetric: every caller sees only its own crons - a lead
        // (`worker_label == None`) the lead crons, a worker its own.
        Ok(cron
            .crons_for_project(&cx.project_name)
            .into_iter()
            .filter(|c| c.team_role == cx.worker_label)
            .collect())
    }

    fn delete_cron(&self, caller: &SessionSlot, id: &CronId) -> Result<CronEntry, CronDeleteError> {
        let (cron, host) = self.parts().ok_or(CronDeleteError::UnknownCallerProject)?;
        let cx = host.caller(caller).ok_or(CronDeleteError::UnknownCallerProject)?;
        if let Some(entry) =
            cron.remove_cron_owned_by(&cx.project_name, id, cx.worker_label.as_deref())
        {
            return Ok(entry);
        }
        // The owner-scoped removal matched nothing, which is either an id
        // the project never had or one another session owns; the caller is
        // told which.
        if cron.crons_for_project(&cx.project_name).iter().any(|c| c.id == *id) {
            Err(CronDeleteError::NotOwnedByCaller)
        } else {
            Err(CronDeleteError::NoSuchCron)
        }
    }
}

/// One recorded `create_cron` call: caller, kind, prompt, description, wake.
#[cfg(any(test, feature = "testing"))]
type CreateCall = (SessionSlot, CronKind, String, Option<String>, bool);

/// Records calls + returns preloaded results so the tool tests can assert
/// the tool correctly parses args, resolves the caller, and surfaces
/// facade results/errors - without a real workspace.
#[cfg(any(test, feature = "testing"))]
#[derive(Default)]
pub struct MockCronFacade {
    pub crons: parking_lot::Mutex<Vec<CronEntry>>,
    pub list_result: parking_lot::Mutex<Option<Result<Vec<CronEntry>, CronListError>>>,
    pub create_calls: parking_lot::Mutex<Vec<CreateCall>>,
    pub create_result: parking_lot::Mutex<Option<Result<CronEntry, CronCreateError>>>,
    pub delete_calls: parking_lot::Mutex<Vec<(SessionSlot, CronId)>>,
    pub delete_result: parking_lot::Mutex<Option<Result<CronEntry, CronDeleteError>>>,
}

#[cfg(any(test, feature = "testing"))]
impl MockCronFacade {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn into_arc(self) -> Arc<dyn CronFacade> {
        Arc::new(self)
    }
}

#[cfg(any(test, feature = "testing"))]
impl CronFacade for MockCronFacade {
    fn create_cron(
        &self,
        caller: &SessionSlot,
        kind: CronKind,
        prompt: String,
        description: Option<String>,
        wake: bool,
    ) -> Result<CronEntry, CronCreateError> {
        self.create_calls.lock().push((
            caller.clone(),
            kind.clone(),
            prompt.clone(),
            description.clone(),
            wake,
        ));
        if let Some(preloaded) = self.create_result.lock().clone() {
            return preloaded;
        }
        Ok(CronEntry {
            id: CronId::from("mock-cron-id"),
            project_name: "mock".to_owned(),
            kind,
            prompt,
            description,
            created_at: SystemTime::UNIX_EPOCH,
            last_fire: None,
            next_fire: SystemTime::UNIX_EPOCH,
            team_role: None,
            wake,
        })
    }

    fn list_crons(&self, _caller: &SessionSlot) -> Result<Vec<CronEntry>, CronListError> {
        self.list_result.lock().clone().unwrap_or_else(|| Ok(self.crons.lock().clone()))
    }

    fn delete_cron(&self, caller: &SessionSlot, id: &CronId) -> Result<CronEntry, CronDeleteError> {
        self.delete_calls.lock().push((caller.clone(), id.clone()));
        self.delete_result.lock().clone().unwrap_or_else(|| {
            Ok(CronEntry {
                id: id.clone(),
                project_name: "mock".to_owned(),
                kind: CronKind::Recurring("0 9 * * *".to_owned()),
                prompt: "p".to_owned(),
                description: None,
                created_at: SystemTime::UNIX_EPOCH,
                last_fire: None,
                next_fire: SystemTime::UNIX_EPOCH,
                wake: true,
                team_role: None,
            })
        })
    }
}
