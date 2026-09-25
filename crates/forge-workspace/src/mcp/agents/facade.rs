//! The seam between the merged `agents__*` family and the two engines
//! it drives. Every verb that takes a target resolves it here, so the
//! routing rule lives in one place rather than at each call site.

use std::sync::Arc;

use crate::SessionSlot;
use crate::mcp::agents::target::{AgentTarget, LEAD_LABEL};
use crate::mcp::peers::facade::{TargetStatus, WorkspaceFacade};
use crate::mcp::peers::types::WrappedPrompt;
use crate::mcp::workers::facade::{WorkerDeliverError, WorkerFacade, WorkerLeadDeliverError};

/// The two engines, both held: a target inside the caller's project is
/// served by the workers engine and one outside it by the peers
/// engine. Keeping both is what makes this a dispatcher rather than a
/// third implementation.
pub struct AgentDispatcher {
    peers: Arc<dyn WorkspaceFacade>,
    workers: Arc<dyn WorkerFacade>,
}

impl AgentDispatcher {
    pub fn new(peers: Arc<dyn WorkspaceFacade>, workers: Arc<dyn WorkerFacade>) -> Self {
        Self { peers, workers }
    }

    /// The cross-project engine, for listing configured projects and
    /// reading the caller's own identity.
    pub fn peers(&self) -> &Arc<dyn WorkspaceFacade> {
        &self.peers
    }

    /// The in-project engine, for the caller's own workers and every
    /// worker lifecycle verb.
    pub fn workers(&self) -> &Arc<dyn WorkerFacade> {
        &self.workers
    }

    /// Deliver `wrapped` to `target` and report what the delivering
    /// engine decided: `delivered`, or `queued_for_spawn` when the
    /// target is another project's sleeping agent.
    ///
    /// Three of the four paths are the ones the two families already
    /// had, unchanged. The fourth is the reach the merge adds: a
    /// worker address in another project, which the peers engine
    /// cannot serve because it resolves a project to that project's
    /// own agent.
    pub fn deliver(
        &self,
        caller: &SessionSlot,
        target: &AgentTarget,
        wrapped: WrappedPrompt,
    ) -> Result<&'static str, String> {
        let own_project = target.org() == caller.org() && target.project() == caller.project();
        if own_project {
            return self.deliver_within_own_project(caller, target, wrapped);
        }
        if target.label() == LEAD_LABEL {
            return match self.peers.deliver_peer_prompt(caller, target.project(), wrapped) {
                Ok(TargetStatus::Delivered) => Ok("delivered"),
                Ok(TargetStatus::QueuedForSpawn) => Ok("queued_for_spawn"),
                Err(err) => Err(format!(
                    "project '{}' is no longer reachable ({err:?}); call agents__list to see \
                     who you can reach.",
                    target.project(),
                )),
            };
        }
        match self.workers.deliver_worker_prompt_to_project(
            caller,
            target.org(),
            target.project(),
            target.label(),
            wrapped,
        ) {
            Ok(_) => Ok("delivered"),
            Err(err) => Err(unknown_label_message(target, &err)),
        }
    }

    fn deliver_within_own_project(
        &self,
        caller: &SessionSlot,
        target: &AgentTarget,
        wrapped: WrappedPrompt,
    ) -> Result<&'static str, String> {
        if target.label() == LEAD_LABEL {
            return match self.workers.deliver_prompt_to_lead(caller, wrapped) {
                Ok(_) => Ok("delivered"),
                Err(err) => Err(lead_deliver_message(&err)),
            };
        }
        match self.workers.deliver_worker_prompt(caller, target.label(), wrapped) {
            Ok(_) => Ok("delivered"),
            Err(err) => Err(unknown_label_message(target, &err)),
        }
    }
}

fn unknown_label_message(target: &AgentTarget, err: &WorkerDeliverError) -> String {
    let WorkerDeliverError::UnknownLabel { project_key, label } = err;
    format!(
        "worker '{label}' is not available (no live worker by that label in '{project_key}', \
         the project you addressed as '{}'/'{}'). Call agents__list for your own project's \
         pool; a worker in another project is addressed by whatever label that project's \
         own agent gives you.",
        target.org(),
        target.project(),
    )
}

/// `label` naming a project's own agent is worker-only addressing: a
/// lead has no lead above it to talk back to.
fn lead_deliver_message(err: &WorkerLeadDeliverError) -> String {
    match err {
        WorkerLeadDeliverError::UnknownCaller => {
            "could not resolve caller to a known session (forge bug)".to_owned()
        }
        WorkerLeadDeliverError::LeadCallerHasNoLead => {
            "the label 'lead' addresses your own project's agent, which is worker-only: this \
             session IS that agent, so it has nothing above it. Address another project by its \
             org and project name instead."
                .to_owned()
        }
        WorkerLeadDeliverError::LeadGone => {
            "your lead is not available (its session closed since this worker was spawned)."
                .to_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProjectKey;
    use crate::mcp::peers::facade::MockWorkspaceFacade;
    use crate::mcp::peers::types::{CorrelationId, PeerLiveness, PeerStatus, WrappedKind};
    use crate::mcp::workers::facade::{CallerProject, MockWorkerFacade};
    use forge_primitives::{WorkerLiveness, WorkerStatus};

    /// Two projects: the caller's own (`acme`/`core`) and another
    /// (`other`/`proj`). The other project has a lead and one worker.
    fn host() -> (Arc<MockWorkspaceFacade>, Arc<MockWorkerFacade>, AgentDispatcher) {
        let peers = Arc::new(MockWorkspaceFacade::new());
        peers.peers.lock().extend([configured("acme", "core"), configured("other", "proj")]);
        let workers = Arc::new(MockWorkerFacade::new());
        // `core` is the caller's project key; `proj` names the other
        // project, which the project-keyed delivery resolves by name.
        workers.workers.lock().insert("core".to_owned(), vec![worker("acme", "core", "w1")]);
        workers.workers.lock().insert("proj".to_owned(), vec![worker("other", "proj", "w1")]);
        let dispatcher = AgentDispatcher::new(
            Arc::clone(&peers) as Arc<dyn WorkspaceFacade>,
            Arc::clone(&workers) as Arc<dyn WorkerFacade>,
        );
        (peers, workers, dispatcher)
    }

    #[test]
    fn a_project_name_the_caller_shares_with_another_org_is_not_its_own_project() {
        // `core` exists under both orgs here, which the config loader would
        // refuse - it is the pair the dispatcher compares, and this is
        // that comparison on its own rather than a state forge can reach.
        let (peers, _workers, dispatcher) = host();
        let caller = SessionSlot::lead("acme", "core");
        let status = dispatcher
            .deliver(&caller, &target("other", "core", None), message())
            .expect("core is configured under other");
        assert_eq!(status, "delivered");
        assert_eq!(peers.deliver_calls.lock().len(), 1, "the peers engine carried it");
    }

    fn configured(org: &str, name: &str) -> PeerStatus {
        PeerStatus {
            name: name.to_owned(),
            org: org.to_owned(),
            path: std::path::PathBuf::from(format!("/tmp/{name}")),
            status: PeerLiveness::Running,
            in_flight_incoming: 0,
            in_flight_outgoing: 0,
            spawned_at: None,
        }
    }

    fn worker(org: &str, project: &str, label: &str) -> WorkerStatus {
        WorkerStatus {
            label: label.to_owned(),
            charter: format!("{label}'s charter"),
            status: WorkerLiveness::Running,
            session_id: format!("{label}-session"),
            slot: SessionSlot::worker(org, project, label),
            spawned_at: std::time::SystemTime::UNIX_EPOCH,
            spawned_by: SessionSlot::lead(org, project),
            diagnostic: None,
            activity: None,
        }
    }

    fn target(org: &str, project: &str, label: Option<&str>) -> AgentTarget {
        AgentTarget::parse(&configured_projects(), org, project, label).expect("configured project")
    }

    /// `core` exists twice, under two orgs, so a target that resolves by
    /// project name alone is distinguishable from one that needs its org.
    fn configured_projects() -> Vec<PeerStatus> {
        vec![configured("acme", "core"), configured("other", "proj"), configured("other", "core")]
    }

    fn message() -> WrappedPrompt {
        WrappedPrompt {
            correlation_id: CorrelationId::new_tell(),
            kind: WrappedKind::Message,
            sender_name: "lead".to_owned(),
            sender_org: "Personal".to_owned(),
            body: "hi".to_owned(),
        }
    }

    fn caller_in_own_project(workers: &MockWorkerFacade) -> SessionSlot {
        let slot = SessionSlot::lead("acme", "core");
        workers.callers.lock().insert(
            slot.clone(),
            CallerProject { project_key: ProjectKey::new("core"), is_lead: true },
        );
        slot
    }

    #[test]
    fn a_worker_in_the_callers_own_project_goes_through_the_workers_engine() {
        let (peers, workers, dispatcher) = host();
        let caller = caller_in_own_project(&workers);
        let status = dispatcher
            .deliver(&caller, &target("acme", "core", Some("w1")), message())
            .expect("w1 is live in the caller's project");

        assert_eq!(status, "delivered");
        assert_eq!(workers.deliver_calls.lock().len(), 1, "the workers engine carried it");
        assert!(peers.deliver_calls.lock().is_empty(), "the peers engine was not used");
    }

    #[test]
    fn another_projects_agent_goes_through_the_peers_engine() {
        let (peers, workers, dispatcher) = host();
        let caller = caller_in_own_project(&workers);
        let status = dispatcher
            .deliver(&caller, &target("other", "proj", None), message())
            .expect("proj is a configured project");

        assert_eq!(status, "delivered");
        assert_eq!(peers.deliver_calls.lock().len(), 1, "the peers engine carried it");
        assert!(workers.deliver_calls.lock().is_empty(), "the workers engine was not used");
    }

    #[test]
    fn a_named_worker_in_another_project_goes_through_the_workers_engine() {
        // The reach the merge adds: a slot, not a project lead, may be
        // the target from anywhere.
        let (peers, workers, dispatcher) = host();
        let caller = caller_in_own_project(&workers);
        let status = dispatcher
            .deliver(&caller, &target("other", "proj", Some("w1")), message())
            .expect("w1 is live in proj");

        assert_eq!(status, "delivered");
        let calls = workers.deliver_to_project_calls.lock();
        assert_eq!(calls.len(), 1, "the workers engine carried it, keyed to the target project");
        assert_eq!(calls[0].org, "other", "addressed by the target's org");
        assert_eq!(calls[0].project, "proj", "addressed by the target's project");
        assert!(peers.deliver_calls.lock().is_empty(), "the peers engine was not used");
    }

    #[test]
    fn a_label_that_is_not_live_is_refused_rather_than_dropped() {
        let (_peers, workers, dispatcher) = host();
        let caller = caller_in_own_project(&workers);
        let err = dispatcher
            .deliver(&caller, &target("other", "proj", Some("ghost")), message())
            .expect_err("no live worker by that label");
        assert!(err.contains("ghost"), "the refusal names the label: {err}");
    }
}
