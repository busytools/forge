//! Narrow workspace-state surface the workers MCP Tool impls
//! consume. Trait + mock for unit-testing the Tool impls without
//! spinning up a real Workspace. Mirrors `crate::mcp::peers::facade`.
//!
//! The production impl (`ProdWorkerFacade`) lands in Task 7.

use std::sync::{Arc, Weak};

use forge_primitives::WorkerStatus;

use crate::SessionKey;
use crate::mcp::peers::facade::PeerStatsDelta;
use crate::mcp::peers::types::{CorrelationId, InflightAsk, WrappedPrompt};
use crate::mcp::workers::types::WorkerEntry;
use crate::protocol::{Command, WorkerSpawnReply};
use crate::workspace::Workspace;

/// Synchronous decision from `deliver_worker_prompt` - whether the
/// target was found (delivered) or unknown (label has no live
/// match). Async failures surface via existing `inflight_asks`
/// expiry machinery, same as peer-MCP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerTargetStatus {
    Delivered,
}

/// Synchronous error from `deliver_worker_prompt`. Async delivery
/// failures (worker crashes mid-flight) flow through the same
/// `Workspace::expire_*_inflight` machinery as peer-MCP.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkerDeliverError {
    /// No live worker in `project_key` matches `label`.
    UnknownLabel { project_key: String, label: String },
    /// Outgoing hop exceeds the limit (default 10).
    HopLimitExceeded { hop: u8, limit: u8 },
}

/// Synchronous error from `deliver_prompt_to_lead`. The caller wants
/// to send a prompt back to its lead via the reserved `"lead"`
/// addressing keyword.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkerLeadDeliverError {
    /// Caller resolves to no known session (defensive — should not
    /// happen with a valid CallerKeyResolver).
    UnknownCaller,
    /// Caller is a project lead — leads have no lead to talk back
    /// to. The `"lead"` keyword is worker-only.
    LeadCallerHasNoLead,
    /// Worker's recorded `spawned_by_session_id` no longer resolves
    /// to a session in the pool (lead session ended). The worker
    /// can't reach its lead anymore.
    LeadGone { lead_session_id: String },
    /// Outgoing hop exceeds the limit (default 10).
    HopLimitExceeded { hop: u8, limit: u8 },
}

/// Label string the workers MCP reserves for addressing the caller's
/// lead via `workers__tell` / `workers__ask`. Workers may target the
/// lead with `label="lead"`; `workers__spawn` rejects the label so
/// no live worker can shadow the keyword.
pub const LEAD_LABEL: &str = "lead";

/// Synchronous error from `spawn_worker`. All gating happens before
/// the workspace dispatch is even issued.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkerSpawnError {
    /// Caller is a worker, not a project lead. v1 is lead-only.
    NotLeadCaller,
    /// `label` is empty after trim.
    EmptyLabel,
    /// `label` collides with the reserved `"lead"` keyword
    /// (workers MCP uses it as an addressing target for the worker's
    /// own lead — see [`LEAD_LABEL`]).
    ReservedLabel,
    /// `charter` is empty after trim.
    EmptyCharter,
    /// Caller's session key resolves to no known project (defensive;
    /// the tools closure-bind a real key at spawn time, so this
    /// should never fire in practice).
    UnknownCallerProject,
    /// Spawn was dispatched but the workspace returned an error
    /// (e.g. tag-write failed). Forwarded as-is to the LLM.
    DispatchFailed { message: String },
}

/// Caller's project + lead-or-not flag. Returned by
/// [`WorkerFacade::caller_project`].
#[derive(Debug, Clone)]
pub struct CallerProject {
    pub project_key: crate::ProjectKey,
    pub is_lead: bool,
}

/// The narrow workspace-state surface workers MCP Tool impls
/// depend on.
///
/// `spawn_worker` is async because the production impl awaits a
/// oneshot receiver wired into `Command::SpawnWorker`. The remaining
/// methods are synchronous (one mutex acquire over in-memory state).
#[async_trait::async_trait]
pub trait WorkerFacade: Send + Sync {
    /// Resolve the caller's project key + lead/worker flag.
    /// Returns `None` when `caller` matches no known session.
    fn caller_project(&self, caller: &SessionKey) -> Option<CallerProject>;

    /// Dispatch a `Command::SpawnWorker` and await its synchronous
    /// reply. Gating (lead-only, non-empty label/charter) happens
    /// before dispatch.
    async fn spawn_worker(
        &self,
        caller: &SessionKey,
        label: String,
        charter: String,
    ) -> Result<WorkerSpawnReply, WorkerSpawnError>;

    /// Snapshot of every worker in the caller's project. Returns an
    /// empty Vec when the caller resolves to no project.
    fn list_workers(&self, caller: &SessionKey) -> Vec<WorkerStatus>;

    /// Dispatch a worker-bound wrapped prompt. Returns immediately
    /// with `Delivered` (target was in `live_workers` and the
    /// `Command::DeliverWorkerPrompt` was dispatched) or
    /// `UnknownLabel` (no live match). Hop-limit excess is rejected
    /// synchronously here too.
    fn deliver_worker_prompt(
        &self,
        caller: &SessionKey,
        target_label: &str,
        wrapped: WrappedPrompt,
    ) -> Result<WorkerTargetStatus, WorkerDeliverError>;

    /// Dispatch a wrapped prompt from a worker back to its lead.
    /// Caller MUST be a worker. The target lead is resolved from the
    /// caller's `WorkerEntry::spawned_by_session_id`. Returns
    /// `Delivered` (`Command::DeliverWorkerPromptToLead` dispatched)
    /// or one of the [`WorkerLeadDeliverError`] variants. Same
    /// hop-limit + wire-shape contract as `deliver_worker_prompt`.
    fn deliver_prompt_to_lead(
        &self,
        caller: &SessionKey,
        wrapped: WrappedPrompt,
    ) -> Result<WorkerTargetStatus, WorkerLeadDeliverError>;

    /// Register an outgoing ask in the workspace's `inflight_asks`
    /// map. Same map the peer-MCP uses; correlation ids never
    /// collide because of the `q-` / `t-` prefix scheme.
    fn register_inflight_ask(&self, ask: InflightAsk);

    /// Atomically remove an `InflightAsk` from the inflight map.
    /// Returns the removed ask so the caller can inspect its
    /// metadata, or `None` when the entry was already gone.
    fn complete_inflight_ask(&self, id: &CorrelationId) -> Option<InflightAsk>;

    /// Look up an `InflightAsk` without removing it. Used by
    /// `workers__tell` to classify an `in_reply_to` argument as
    /// either a clean reply (entry exists, target matches) or a
    /// degraded message (entry gone / mismatched).
    fn resolve_correlation(&self, id: &CorrelationId) -> Option<InflightAsk>;

    /// Bump per-session peer-inflight stats counters. Same map the
    /// peer-MCP uses; workers__ask bumps `OutgoingPlus1` on the
    /// caller when it fires, workers__tell with `in_reply_to`
    /// decrements `OutgoingMinus1` on the original asker and
    /// `IncomingMinus1` on the replier.
    fn bump_inflight_stats(&self, key: &SessionKey, delta: PeerStatsDelta);
}

/// Production impl. Holds a `Weak<Workspace>` so construction doesn't
/// close a strong cycle through the Workspace -> bridge -> MCP ->
/// Tool -> facade -> Workspace path. Every method starts with
/// `upgrade()` and short-circuits when the workspace has been
/// dropped (only possible during shutdown).
pub struct ProdWorkerFacade {
    workspace: Weak<Workspace>,
}

impl ProdWorkerFacade {
    #[must_use]
    pub fn from_arc(workspace: &Arc<Workspace>) -> Arc<dyn WorkerFacade> {
        Arc::new(Self { workspace: Arc::downgrade(workspace) })
    }
}

#[async_trait::async_trait]
impl WorkerFacade for ProdWorkerFacade {
    fn caller_project(&self, caller: &SessionKey) -> Option<CallerProject> {
        let ws = self.workspace.upgrade()?;
        // live_workers is the authoritative "child agent" registry;
        // a session is a worker iff it appears there. A session is a
        // lead iff it appears in the project's catalog AND is NOT in
        // live_workers. The catalog can index worker sessions once
        // their Connected fires, so `sessions.first()` is not a
        // reliable lead marker - read live_workers first.
        for view in ws.list_projects() {
            let live = ws.list_live_workers(&view.key);
            if live.iter().any(|w| w.session_key == *caller) {
                return Some(CallerProject { project_key: view.key, is_lead: false });
            }
            if view.sessions.iter().any(|s| s.session == *caller) {
                return Some(CallerProject { project_key: view.key, is_lead: true });
            }
        }
        None
    }

    async fn spawn_worker(
        &self,
        caller: &SessionKey,
        label: String,
        charter: String,
    ) -> Result<WorkerSpawnReply, WorkerSpawnError> {
        let cp = self.caller_project(caller).ok_or(WorkerSpawnError::UnknownCallerProject)?;
        if !cp.is_lead {
            return Err(WorkerSpawnError::NotLeadCaller);
        }
        if label.trim().is_empty() {
            return Err(WorkerSpawnError::EmptyLabel);
        }
        if label.trim() == LEAD_LABEL {
            return Err(WorkerSpawnError::ReservedLabel);
        }
        if charter.trim().is_empty() {
            return Err(WorkerSpawnError::EmptyCharter);
        }
        let ws = self.workspace.upgrade().ok_or_else(|| WorkerSpawnError::DispatchFailed {
            message: "workspace dropped".into(),
        })?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let cmd = Command::SpawnWorker {
            project_key: cp.project_key,
            label,
            charter,
            spawned_by_session_id: caller.as_str().to_owned(),
            return_to: tx,
        };
        if let Err(err) = ws.dispatch(cmd) {
            return Err(WorkerSpawnError::DispatchFailed {
                message: format!("dispatch failed: {err:?}"),
            });
        }
        match rx.await {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(message)) => Err(WorkerSpawnError::DispatchFailed { message }),
            Err(_) => Err(WorkerSpawnError::DispatchFailed {
                message: "spawn handler dropped reply channel".into(),
            }),
        }
    }

    fn list_workers(&self, caller: &SessionKey) -> Vec<WorkerStatus> {
        let Some(cp) = self.caller_project(caller) else {
            return Vec::new();
        };
        let Some(ws) = self.workspace.upgrade() else {
            return Vec::new();
        };
        ws.list_live_workers(&cp.project_key).iter().map(WorkerEntry::to_status).collect()
    }

    fn deliver_worker_prompt(
        &self,
        caller: &SessionKey,
        target_label: &str,
        wrapped: WrappedPrompt,
    ) -> Result<WorkerTargetStatus, WorkerDeliverError> {
        if wrapped.hop > wrapped.hop_limit {
            return Err(WorkerDeliverError::HopLimitExceeded {
                hop: wrapped.hop,
                limit: wrapped.hop_limit,
            });
        }
        let cp = self.caller_project(caller).ok_or_else(|| WorkerDeliverError::UnknownLabel {
            project_key: "<unknown>".into(),
            label: target_label.into(),
        })?;
        let Some(ws) = self.workspace.upgrade() else {
            return Err(WorkerDeliverError::UnknownLabel {
                project_key: cp.project_key.as_str().into(),
                label: target_label.into(),
            });
        };
        let known = ws.list_live_workers(&cp.project_key).iter().any(|w| w.label == target_label);
        if !known {
            return Err(WorkerDeliverError::UnknownLabel {
                project_key: cp.project_key.as_str().into(),
                label: target_label.into(),
            });
        }
        if let Err(err) = ws.dispatch(Command::DeliverWorkerPrompt {
            caller: caller.clone(),
            project_key: cp.project_key,
            target_label: target_label.into(),
            wrapped,
        }) {
            tracing::warn!(
                target: "forge_workspace::mcp::workers",
                error = ?err,
                "Command::DeliverWorkerPrompt dispatch failed"
            );
        }
        Ok(WorkerTargetStatus::Delivered)
    }

    fn deliver_prompt_to_lead(
        &self,
        caller: &SessionKey,
        wrapped: WrappedPrompt,
    ) -> Result<WorkerTargetStatus, WorkerLeadDeliverError> {
        if wrapped.hop > wrapped.hop_limit {
            return Err(WorkerLeadDeliverError::HopLimitExceeded {
                hop: wrapped.hop,
                limit: wrapped.hop_limit,
            });
        }
        let cp = self.caller_project(caller).ok_or(WorkerLeadDeliverError::UnknownCaller)?;
        if cp.is_lead {
            return Err(WorkerLeadDeliverError::LeadCallerHasNoLead);
        }
        let Some(ws) = self.workspace.upgrade() else {
            return Err(WorkerLeadDeliverError::UnknownCaller);
        };
        // Find the caller's WorkerEntry to read its spawned_by_session_id.
        // The synth -> real key migration on Connected updates session_key
        // on the entry, so matching by SessionKey works for both the
        // pre-Connect and post-Connect windows.
        let Some(entry) =
            ws.list_live_workers(&cp.project_key).into_iter().find(|w| w.session_key == *caller)
        else {
            return Err(WorkerLeadDeliverError::UnknownCaller);
        };
        let lead_session_id = entry.spawned_by_session_id.clone();
        let target_lead_key = SessionKey::from_session_id(lead_session_id.clone());
        // Defensive: confirm the lead's session is still in the pool
        // before dispatching. If it closed since the worker was
        // spawned, surface a clear error so the worker LLM can adapt.
        if !ws.pool.lock().contains_key(&target_lead_key) {
            return Err(WorkerLeadDeliverError::LeadGone { lead_session_id });
        }
        if let Err(err) = ws.dispatch(Command::DeliverWorkerPromptToLead {
            caller: caller.clone(),
            target_lead_key,
            wrapped,
        }) {
            tracing::warn!(
                target: "forge_workspace::mcp::workers",
                error = ?err,
                "Command::DeliverWorkerPromptToLead dispatch failed"
            );
        }
        Ok(WorkerTargetStatus::Delivered)
    }

    fn register_inflight_ask(&self, ask: InflightAsk) {
        let Some(ws) = self.workspace.upgrade() else { return };
        ws.inflight_asks.lock().insert(ask.correlation_id.clone(), ask);
    }

    fn complete_inflight_ask(&self, id: &CorrelationId) -> Option<InflightAsk> {
        let ws = self.workspace.upgrade()?;
        ws.inflight_asks.lock().remove(id)
    }

    fn resolve_correlation(&self, id: &CorrelationId) -> Option<InflightAsk> {
        let ws = self.workspace.upgrade()?;
        ws.inflight_asks.lock().get(id).cloned()
    }

    fn bump_inflight_stats(&self, key: &SessionKey, delta: PeerStatsDelta) {
        // Reuse the peer-MCP facade's identical implementation by
        // routing through `ProdWorkspaceFacade`. Same `peer_stats`
        // map; one Mutex shared between peer + worker traffic.
        let Some(ws) = self.workspace.upgrade() else { return };
        let facade = crate::mcp::peers::facade::ProdWorkspaceFacade::from_arc(&ws);
        facade.bump_inflight_stats(key, delta);
    }
}

/// Mock for unit-testing the four Tool impls. Captures every
/// dispatched call into a Vec so tests can assert "tool X
/// dispatched spawn with these args" without spinning up a real
/// Workspace.
#[cfg(any(test, feature = "testing"))]
#[derive(Default)]
pub struct MockWorkerFacade {
    /// Pre-loaded caller -> project mapping. Tests insert entries
    /// before invoking the tool.
    pub callers: parking_lot::Mutex<std::collections::HashMap<SessionKey, CallerProject>>,
    /// Pre-loaded `live_workers` snapshot per project_key string.
    /// `list_workers` and `deliver_worker_prompt` both read from
    /// this.
    pub workers: parking_lot::Mutex<std::collections::HashMap<String, Vec<WorkerStatus>>>,
    /// Captured `spawn_worker` calls.
    pub spawn_calls: parking_lot::Mutex<Vec<(SessionKey, String, String)>>,
    /// Pre-loaded reply for `spawn_worker`. When `None`, the mock
    /// returns `DispatchFailed { message: "no preloaded reply" }`.
    pub spawn_reply: parking_lot::Mutex<Option<Result<WorkerSpawnReply, WorkerSpawnError>>>,
    /// Captured `deliver_worker_prompt` calls.
    pub deliver_calls: parking_lot::Mutex<Vec<(SessionKey, String, WrappedPrompt)>>,
    /// Inflight asks the mock has registered.
    pub inflight: parking_lot::Mutex<std::collections::HashMap<CorrelationId, InflightAsk>>,
    /// Captured `bump_inflight_stats` calls so tests can assert the
    /// expected delta sequence (e.g. `OutgoingPlus1` on ask, then
    /// `IncomingMinus1` + `OutgoingMinus1` on a reply tell).
    pub bumps: parking_lot::Mutex<Vec<(SessionKey, PeerStatsDelta)>>,
}

#[cfg(any(test, feature = "testing"))]
impl MockWorkerFacade {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn into_arc(self) -> std::sync::Arc<dyn WorkerFacade> {
        std::sync::Arc::new(self)
    }
}

#[cfg(any(test, feature = "testing"))]
#[async_trait::async_trait]
impl WorkerFacade for MockWorkerFacade {
    fn caller_project(&self, caller: &SessionKey) -> Option<CallerProject> {
        self.callers.lock().get(caller).cloned()
    }

    async fn spawn_worker(
        &self,
        caller: &SessionKey,
        label: String,
        charter: String,
    ) -> Result<WorkerSpawnReply, WorkerSpawnError> {
        let cp = self.caller_project(caller).ok_or(WorkerSpawnError::UnknownCallerProject)?;
        if !cp.is_lead {
            return Err(WorkerSpawnError::NotLeadCaller);
        }
        if label.trim().is_empty() {
            return Err(WorkerSpawnError::EmptyLabel);
        }
        if label.trim() == LEAD_LABEL {
            return Err(WorkerSpawnError::ReservedLabel);
        }
        if charter.trim().is_empty() {
            return Err(WorkerSpawnError::EmptyCharter);
        }
        self.spawn_calls.lock().push((caller.clone(), label, charter));
        self.spawn_reply.lock().clone().unwrap_or(Err(WorkerSpawnError::DispatchFailed {
            message: "no preloaded reply".into(),
        }))
    }

    fn list_workers(&self, caller: &SessionKey) -> Vec<WorkerStatus> {
        let Some(cp) = self.caller_project(caller) else {
            return Vec::new();
        };
        self.workers.lock().get(cp.project_key.as_str()).cloned().unwrap_or_default()
    }

    fn deliver_worker_prompt(
        &self,
        caller: &SessionKey,
        target_label: &str,
        wrapped: WrappedPrompt,
    ) -> Result<WorkerTargetStatus, WorkerDeliverError> {
        if wrapped.hop > wrapped.hop_limit {
            return Err(WorkerDeliverError::HopLimitExceeded {
                hop: wrapped.hop,
                limit: wrapped.hop_limit,
            });
        }
        let cp = self.caller_project(caller).ok_or_else(|| WorkerDeliverError::UnknownLabel {
            project_key: "<unknown>".into(),
            label: target_label.into(),
        })?;
        let known = self
            .workers
            .lock()
            .get(cp.project_key.as_str())
            .is_some_and(|ws| ws.iter().any(|w| w.label == target_label));
        if !known {
            return Err(WorkerDeliverError::UnknownLabel {
                project_key: cp.project_key.as_str().into(),
                label: target_label.into(),
            });
        }
        self.deliver_calls.lock().push((caller.clone(), target_label.into(), wrapped));
        Ok(WorkerTargetStatus::Delivered)
    }

    fn deliver_prompt_to_lead(
        &self,
        caller: &SessionKey,
        wrapped: WrappedPrompt,
    ) -> Result<WorkerTargetStatus, WorkerLeadDeliverError> {
        if wrapped.hop > wrapped.hop_limit {
            return Err(WorkerLeadDeliverError::HopLimitExceeded {
                hop: wrapped.hop,
                limit: wrapped.hop_limit,
            });
        }
        let cp = self.caller_project(caller).ok_or(WorkerLeadDeliverError::UnknownCaller)?;
        if cp.is_lead {
            return Err(WorkerLeadDeliverError::LeadCallerHasNoLead);
        }
        // Mock surfaces a synthetic spawned_by lookup via the
        // preloaded workers map (entries carry spawned_by via
        // WorkerStatus). The Tool tests preload that field; the
        // failure modes (LeadGone, UnknownCaller) are still
        // reachable when the test omits the entry.
        let lead_session_id = self
            .workers
            .lock()
            .get(cp.project_key.as_str())
            .and_then(|ws| ws.iter().find(|w| w.session_id == caller.as_str()))
            .map(|w| w.spawned_by_session_id.clone());
        let Some(lead_session_id) = lead_session_id else {
            return Err(WorkerLeadDeliverError::UnknownCaller);
        };
        // Mock has no pool to consult; treat empty `spawned_by` as
        // "lead gone" so tests can exercise that path explicitly.
        if lead_session_id.is_empty() {
            return Err(WorkerLeadDeliverError::LeadGone { lead_session_id });
        }
        // Record under the synthetic label `<lead>` so tests can
        // assert "the lead-bound delivery happened" without colliding
        // with a real worker label.
        self.deliver_calls.lock().push((caller.clone(), "<lead>".to_owned(), wrapped));
        Ok(WorkerTargetStatus::Delivered)
    }

    fn register_inflight_ask(&self, ask: InflightAsk) {
        self.inflight.lock().insert(ask.correlation_id.clone(), ask);
    }

    fn complete_inflight_ask(&self, id: &CorrelationId) -> Option<InflightAsk> {
        self.inflight.lock().remove(id)
    }

    fn resolve_correlation(&self, id: &CorrelationId) -> Option<InflightAsk> {
        self.inflight.lock().get(id).cloned()
    }

    fn bump_inflight_stats(&self, key: &SessionKey, delta: PeerStatsDelta) {
        self.bumps.lock().push((key.clone(), delta));
    }
}

#[cfg(test)]
mod mock_tests {
    use super::*;
    use crate::mcp::peers::types::WrappedKind;

    #[test]
    fn mock_caller_project_returns_preloaded() {
        let mock = MockWorkerFacade::new();
        mock.callers.lock().insert(
            SessionKey::from_session_id("k1"),
            CallerProject { project_key: crate::ProjectKey::new("forge"), is_lead: true },
        );
        let cp = mock.caller_project(&SessionKey::from_session_id("k1")).unwrap();
        assert!(cp.is_lead);
        assert_eq!(cp.project_key.as_str(), "forge");
    }

    #[tokio::test]
    async fn mock_spawn_rejects_non_lead() {
        let mock = MockWorkerFacade::new();
        mock.callers.lock().insert(
            SessionKey::from_session_id("k1"),
            CallerProject { project_key: crate::ProjectKey::new("forge"), is_lead: false },
        );
        let res = mock
            .spawn_worker(&SessionKey::from_session_id("k1"), "reviewer".into(), "charter".into())
            .await;
        assert!(matches!(res, Err(WorkerSpawnError::NotLeadCaller)));
    }

    #[tokio::test]
    async fn mock_spawn_records_call_and_returns_preloaded_reply() {
        let mock = MockWorkerFacade::new();
        mock.callers.lock().insert(
            SessionKey::from_session_id("lead-key"),
            CallerProject { project_key: crate::ProjectKey::new("forge"), is_lead: true },
        );
        *mock.spawn_reply.lock() = Some(Ok(WorkerSpawnReply {
            session_id: "new-uuid".into(),
            tag: "forge:worker:reviewer".into(),
        }));
        let res = mock
            .spawn_worker(
                &SessionKey::from_session_id("lead-key"),
                "reviewer".into(),
                "charter".into(),
            )
            .await
            .unwrap();
        assert_eq!(res.session_id, "new-uuid");
        assert_eq!(mock.spawn_calls.lock().len(), 1);
    }

    #[test]
    fn mock_deliver_unknown_label_errors() {
        let mock = MockWorkerFacade::new();
        let caller = SessionKey::from_session_id("k1");
        mock.callers.lock().insert(
            caller.clone(),
            CallerProject { project_key: crate::ProjectKey::new("forge"), is_lead: true },
        );
        let wrapped = WrappedPrompt {
            correlation_id: CorrelationId::new_ask(),
            kind: WrappedKind::Question,
            sender_name: "forge".into(),
            sender_org: "Personal".into(),
            hop: 1,
            hop_limit: 10,
            body: "hi".into(),
        };
        let res = mock.deliver_worker_prompt(&caller, "missing", wrapped);
        assert!(matches!(res, Err(WorkerDeliverError::UnknownLabel { .. })));
    }
}
