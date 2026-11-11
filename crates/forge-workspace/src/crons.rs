//! The workspace's half of the cron subsystem's seams.
//!
//! The subsystem owns the durable schedule set and the policy over it; the
//! workspace owns what every host owns - the redb store, the update fanout,
//! the project catalogue, the worker registry, the supervisor's tick and the
//! delivery path into a session - and the two meet here.
//!
//! [`CronPorts`] implements [`CronStore`], [`CronEmit`] and [`CronHost`];
//! [`build`] assembles the handle the boot path, the tools and the scheduler
//! keep. Nothing here holds the subsystem's state: `cron` is a handle, and
//! the subsystem reads the workspace only through the seams.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use forge_primitives::cron::{CronEntry, CronId, CronName};
use forge_primitives::{SessionSlot, cloud::service_status::ServiceSeverity};
use forge_tools::cron::{Cron, CronEmit, CronFireOutcome, CronHost, CronSeams, CronStore};
use parking_lot::Mutex;
use tracing::Instrument;

use crate::protocol::SessionUpdate;
use crate::target::ProjectKey;
use crate::update_fanout::UpdateFanout;
use crate::workspace::Workspace;

/// How often the cron scheduler wakes to fire due crons.
pub(crate) const CRON_TICK_INTERVAL: Duration = forge_tools::cron::CRON_TICK_INTERVAL;

/// The subsystem's seams over this workspace: the durable store, the wire
/// frames and the delivery into a session.
pub(crate) struct CronPorts {
    db: Arc<Mutex<Option<crate::store::Db>>>,
    updates: UpdateFanout,
    /// Set once the workspace exists as an `Arc`: a delivery enters a
    /// session's task and the announcement reads the project catalogue, so
    /// neither can be answered from a store handle alone.
    workspace: std::sync::OnceLock<std::sync::Weak<Workspace>>,
}

impl CronPorts {
    pub(crate) fn new(db: Arc<Mutex<Option<crate::store::Db>>>, updates: UpdateFanout) -> Self {
        Self { db, updates, workspace: std::sync::OnceLock::new() }
    }

    /// Bind the host actions that need the workspace itself. Called from
    /// every `Arc<Workspace>` entry the cron subsystem can be reached
    /// through; setting it twice is a no-op.
    pub(crate) fn bind(&self, workspace: &Arc<Workspace>) {
        let _ = self.workspace.set(Arc::downgrade(workspace));
    }

    fn host_workspace(&self) -> Option<Arc<Workspace>> {
        self.workspace.get()?.upgrade()
    }
}

impl CronStore for CronPorts {
    fn upsert(&self, cron: &CronEntry) -> Result<(), String> {
        let db = self.db.lock();
        let Some(db) = db.as_ref() else { return Ok(()) };
        crate::store::cron::upsert(db, cron).map_err(|error| error.to_string())
    }

    fn delete(&self, id: &CronId) -> Result<(), String> {
        let db = self.db.lock();
        let Some(db) = db.as_ref() else { return Ok(()) };
        crate::store::cron::delete(db, id).map(|_removed| ()).map_err(|error| error.to_string())
    }

    fn put_name(&self, prompt: &str, name: &CronName, now: SystemTime) -> Result<(), String> {
        let db = self.db.lock();
        let Some(db) = db.as_ref() else { return Ok(()) };
        crate::store::cron_names::put(db, prompt, name, now).map_err(|error| error.to_string())
    }

    fn names_for(
        &self,
        prompts: &[&str],
    ) -> Result<std::collections::BTreeMap<String, CronName>, String> {
        let db = self.db.lock();
        let Some(db) = db.as_ref() else {
            return Ok(std::collections::BTreeMap::new());
        };
        crate::store::cron_names::for_uuids(db, prompts).map_err(|error| error.to_string())
    }
}

impl CronEmit for CronPorts {
    fn schedules_changed(&self, project: &str, crons: &[CronEntry]) {
        // The frame routes on the project's lead seat. A name no project
        // carries has no seat to route on and announces nothing.
        let Some(workspace) = self.host_workspace() else {
            tracing::debug!(
                target: "forge_workspace::crons",
                event_name = "cron_announce_unbound",
                project,
                "no workspace is bound yet; a schedule announcement was dropped",
            );
            return;
        };
        let Some(key) = workspace.lead_slot_for_project(project) else {
            tracing::debug!(
                target: "forge_workspace::crons",
                event_name = "cron_schedules_changed_unroutable",
                project,
                "a cron write named a project no project carries; nothing was announced",
            );
            return;
        };
        let _ =
            self.updates.send(SessionUpdate::CronSchedulesChanged { key, crons: crons.to_vec() });
    }

    fn service_status(&self, message: String) {
        let _ = self
            .updates
            .send(SessionUpdate::ServiceStatus { severity: ServiceSeverity::Warning, message });
    }
}

impl CronHost for CronPorts {
    fn deliver(&self, cron: &CronEntry, missed: bool) -> CronFireOutcome {
        let Some(workspace) = self.host_workspace() else {
            return CronFireOutcome::DispatchFailed;
        };
        // The delivery's own outcome, never a bus acceptance: the advance
        // decision turns on whether the prompt actually landed. The
        // delivery returns the port's outcome type itself, so there is no
        // second enum to drift from it.
        crate::spawn::deliver_cron_prompt(&workspace, cron, missed)
    }

    fn is_shutting_down(&self) -> bool {
        self.host_workspace().is_some_and(|workspace| workspace.is_shutting_down())
    }
}

impl forge_tools::cron::facade::CronFacadeHost for Workspace {
    fn caller(&self, caller: &SessionSlot) -> Option<forge_tools::cron::facade::CronCaller> {
        let cx = crate::mcp::caller_context::caller_context(self, caller)?;
        Some(forge_tools::cron::facade::CronCaller {
            project_name: cx.project_name,
            worker_label: cx.worker_label,
        })
    }
}

/// Assemble the subsystem and its ports.
pub(crate) fn build(
    entries: Vec<CronEntry>,
    load_failed: bool,
    db: Arc<Mutex<Option<crate::store::Db>>>,
    updates: UpdateFanout,
) -> (Arc<Cron>, Arc<CronPorts>) {
    let ports = Arc::new(CronPorts::new(db, updates));
    let cron = Arc::new(Cron::new(
        entries,
        load_failed,
        CronSeams {
            store: Arc::clone(&ports) as Arc<dyn CronStore>,
            emit: Arc::clone(&ports) as Arc<dyn CronEmit>,
            host: Arc::clone(&ports) as Arc<dyn CronHost>,
        },
    ));
    (cron, ports)
}

impl Workspace {
    /// Bind the subsystem's host callbacks to this workspace. Idempotent.
    pub(crate) fn bind_cron(self: &Arc<Self>) {
        self.cron_ports.bind(self);
    }

    /// The cron family's tool provider for one session: the facade over the
    /// subsystem handle and this workspace as its host, both weak so the MCP
    /// server the workspace owns cannot keep it alive.
    pub(crate) fn cron_tools(
        self: &Arc<Self>,
        slot: SessionSlot,
    ) -> Arc<dyn forge_mcp::ToolProvider> {
        // No bind here: boot starts the scheduler and the boot catch-up
        // before any session exists to compose a provider, every fire path
        // binds again, and `testing_stub` binds at construction. An unbound
        // arm is the debug record of a host that reached this without any of
        // them, not a state a tool call can create.
        let host: Arc<dyn forge_tools::cron::facade::CronFacadeHost> = self.clone();
        let facade: Arc<dyn forge_tools::cron::facade::CronFacade> =
            Arc::new(forge_tools::cron::facade::ProdCronFacade::new(
                Arc::downgrade(&self.cron),
                Arc::downgrade(&host),
            ));
        Arc::new(forge_tools::cron::CronTools::new(facade, slot))
    }

    /// Lock the durable cron list and apply `f`. Every cron-list mutation
    /// routes through here to take the lock; the mutation site persists
    /// what it changed from inside its own closure - a row at a time, never
    /// the whole in-memory set.
    #[cfg(test)]
    pub(crate) fn with_crons_mut<R>(&self, f: impl FnOnce(&mut Vec<CronEntry>) -> R) -> R {
        self.cron.with_crons_mut(f)
    }

    /// Append a cron and persist it. Backs `cron__create`.
    #[cfg(test)]
    pub(crate) fn push_cron(&self, entry: CronEntry) {
        self.cron.push_cron(entry);
    }

    /// Remove the cron `id` in `project_name` regardless of who owns it.
    #[cfg(test)]
    pub(crate) fn remove_cron(&self, project_name: &str, id: &CronId) -> bool {
        self.cron.remove_cron(project_name, id)
    }

    /// Remove the cron `id` only when its owner matches `label`.
    #[cfg(test)]
    pub(crate) fn remove_cron_owned_by(
        &self,
        project_name: &str,
        id: &CronId,
        label: Option<&str>,
    ) -> Option<CronEntry> {
        self.cron.remove_cron_owned_by(project_name, id, label)
    }

    /// Remove worker `label`'s crons in `project_key` from the in-memory set
    /// and the store. The key is the workspace's catalogue; the schedule set
    /// is the subsystem's.
    pub(crate) fn delete_crons_for_worker(&self, project_key: &ProjectKey, label: &str) {
        let Some(project_name) =
            self.list_projects().into_iter().find(|v| v.key == *project_key).map(|v| v.name)
        else {
            tracing::warn!(
                target: "forge_workspace::crons",
                project = %project_key.as_str(),
                label = %label,
                "could not resolve a project name at worker teardown; its crons may be stranded",
            );
            return;
        };
        self.cron.remove_crons_for_worker(&project_name, label);
    }

    /// The crons registered for `project_name`, whatever their owner.
    pub fn crons_for_project(&self, project_name: &str) -> Vec<CronEntry> {
        self.cron.crons_for_project(project_name)
    }

    /// Whether this run's boot could not load the durable crons.
    pub fn durable_crons_unavailable(&self) -> bool {
        self.cron.durable_crons_unavailable()
    }

    /// Remember the schedule a fired prompt came from.
    pub(crate) fn remember_cron_name(
        &self,
        slot: &SessionSlot,
        uuid: &str,
        cron_id: &str,
        description: Option<&str>,
    ) {
        self.cron.remember_cron_name(slot, uuid, cron_id, description);
    }

    /// The schedules the given fired prompts came from, keyed by the prompt's
    /// own id.
    pub fn cron_names(
        &self,
        slot: &SessionSlot,
        uuids: &[&str],
    ) -> std::collections::BTreeMap<String, CronName> {
        self.cron.cron_names(slot, uuids)
    }

    /// Advance a fired cron and persist: a recurring cron records
    /// `last_fire` and moves `next_fire`; a run-once is removed.
    #[cfg(test)]
    pub(crate) fn advance_or_remove_cron(&self, id: &CronId, fired_at: SystemTime) {
        self.cron.advance_or_remove_cron(id, fired_at);
    }

    /// Fire every cron due at `now`. Also the boot catch-up: calling this
    /// once at startup fires every cron that came due while forge was down.
    pub fn fire_due_crons(self: &Arc<Self>, now: SystemTime) {
        self.bind_cron();
        self.cron.fire_due(now);
    }

    /// Spawn the cron scheduler: a background task that wakes every
    /// `CRON_TICK_INTERVAL` and fires every due cron. The registry makes a
    /// second call a no-op.
    pub fn start_cron_scheduler(self: &Arc<Self>) {
        self.bind_cron();
        let weak = Arc::downgrade(self);
        let span = tracing::info_span!("cron_scheduler");
        self.supervisor.spawn("cron_scheduler", forge_supervise::Policy::backoff(), move || {
            let weak = weak.clone();
            let span = span.clone();
            async move {
                let mut interval = tokio::time::interval_at(
                    tokio::time::Instant::now() + CRON_TICK_INTERVAL,
                    CRON_TICK_INTERVAL,
                );
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    interval.tick().await;
                    let Some(workspace) = weak.upgrade() else {
                        return;
                    };
                    workspace.cron.fire_due(std::time::SystemTime::now());
                }
            }
            .instrument(span)
        });
    }
}

#[cfg(any(test, feature = "testing"))]
impl Workspace {
    /// Register a durable cron directly, bypassing the MCP create path.
    ///
    /// The store write without the announcement: a fixture seeds state, it
    /// does not perform the write a view is owed a frame for.
    pub fn seed_test_cron(&self, entry: CronEntry) {
        self.cron.seed(entry);
    }
}

/// A cron prompt buffered for a sleeping slot, re-exported at this path for
/// the code that drains it.
pub(crate) use crate::parked::PendingCron;

/// India Standard Time's offset, re-exported for the healing notices that
/// print a reader's local clock.
pub(crate) use forge_tools::cron::IST_OFFSET_MINUTES;
