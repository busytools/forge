//! Narrow workspace-state surface the workers MCP Tool impls
//! consume. Trait + mock for unit-testing the Tool impls without
//! spinning up a real Workspace. Mirrors `crate::mcp::peers::facade`.
//!
//! The production impl (`ProdWorkerFacade`) lands in Task 7.

use forge_primitives::WorkerStatus;

use crate::SessionKey;
use crate::mcp::peers::types::{CorrelationId, InflightAsk, WrappedPrompt};
use crate::protocol::WorkerSpawnReply;

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

/// Synchronous error from `spawn_worker`. All gating happens before
/// the workspace dispatch is even issued.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkerSpawnError {
    /// Caller is a worker, not a project lead. v1 is lead-only.
    NotLeadCaller,
    /// `label` is empty after trim.
    EmptyLabel,
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

    /// Register an outgoing ask in the workspace's `inflight_asks`
    /// map. Same map the peer-MCP uses; correlation ids never
    /// collide because of the `q-` / `t-` prefix scheme.
    fn register_inflight_ask(&self, ask: InflightAsk);

    /// Atomically remove an `InflightAsk` from the inflight map.
    /// Returns the removed ask so the caller can inspect its
    /// metadata, or `None` when the entry was already gone.
    fn complete_inflight_ask(&self, id: &CorrelationId) -> Option<InflightAsk>;
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

    fn register_inflight_ask(&self, ask: InflightAsk) {
        self.inflight.lock().insert(ask.correlation_id.clone(), ask);
    }

    fn complete_inflight_ask(&self, id: &CorrelationId) -> Option<InflightAsk> {
        self.inflight.lock().remove(id)
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
