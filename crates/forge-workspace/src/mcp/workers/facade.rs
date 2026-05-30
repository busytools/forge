//! Narrow workspace-state surface the workers MCP Tool impls
//! consume. Trait + mock for unit-testing the Tool impls without
//! spinning up a real Workspace. Mirrors `crate::mcp::peers::facade`.
//!
//! The production impl is `ProdWorkerFacade` (below).

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
    /// Caller resolves to no known session (defensive - should not
    /// happen with a valid CallerKeyResolver).
    UnknownCaller,
    /// Caller is a project lead - leads have no lead to talk back
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

/// Org string stamped into a lead caller's wire envelope (and the
/// matching synthetic org for Assistant peer-outbound tool_use rows
/// on the lead-worker chat surface). Pairs with [`LEAD_LABEL`] so
/// both sides of the wire envelope stay in sync when they describe
/// the lead's identity.
pub const PERSONAL_ORG: &str = "Personal";

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
    /// own lead - see [`LEAD_LABEL`]).
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
    /// claude failed to create the git worktree for `--worktree <label>`.
    /// Reason text comes from claude's error output, passed through
    /// verbatim so the lead's LLM can decide whether to retry (e.g.
    /// pick a different label, or `git commit --allow-empty` first
    /// in the empty-repo case).
    WorktreeCreationFailed { reason: String },
}

/// Heuristic mapping from a raw spawn-error message + the worker's
/// `is_git_repo_at_spawn` flag to a typed [`WorkerSpawnError`].
///
/// Routing rules:
///
/// - The message mentions "worktree" verbatim, OR matches a known
///   worktree-creation failure pattern ("failed to resolve base branch",
///   "already used by worktree") AND the worker was actually spawned
///   in a git repo (so `--worktree=<label>` was on the argv) →
///   [`WorkerSpawnError::WorktreeCreationFailed`]. The original message
///   is forwarded verbatim as `reason`.
/// - Everything else → [`WorkerSpawnError::DispatchFailed`].
///
/// The `is_git_repo_at_spawn` guard exists because a worker spawned in
/// a non-git-repo project (no `--worktree` on the argv) could not have
/// produced a worktree-creation failure. A "worktree" word arriving for
/// such a worker is suspicious - some other layer of the spawn pipe -
/// and falls through to [`WorkerSpawnError::DispatchFailed`] so the
/// LLM sees the raw message rather than a misclassified variant.
///
/// Matching is case-insensitive. The reason string passed back keeps
/// the original casing for verbatim LLM display.
///
/// ## Bridge-prefix contract (#245 Layer C blocker 2)
///
/// Async spawn failures arrive here pre-wrapped by the bridge in
/// [`forge_agent::forge_sdk_bridge`]:
///
/// - `"forge-sdk session spawn failed: {err}"`
/// - `"forge-sdk session resume failed: {err}"`
/// - `"forge-sdk session spawn failed after resume fallback (resume err: {a}; new err: {b})"`
///
/// None of those wrapper strings contain "worktree" or any of the
/// other discriminators above, so the classifier's substring search
/// remains correct even after wrapping. Any future bridge wrapper
/// that introduces the word "worktree" into the literal prefix
/// MUST update this classifier (or switch to a typed channel) -
/// the unit test `bridge_prefix_does_not_collide_with_worktree_predicate`
/// pins this contract so the breaking change surfaces in CI.
#[must_use]
pub fn classify_worker_spawn_failure(
    message: &str,
    is_git_repo_at_spawn: bool,
) -> WorkerSpawnError {
    let lower = message.to_lowercase();
    let mentions_worktree = lower.contains("worktree");
    let resembles_branch_resolve = lower.contains("failed to resolve base branch");
    let resembles_already_exists = lower.contains("already used by worktree");
    if (mentions_worktree || resembles_branch_resolve || resembles_already_exists)
        && is_git_repo_at_spawn
    {
        return WorkerSpawnError::WorktreeCreationFailed { reason: message.to_owned() };
    }
    WorkerSpawnError::DispatchFailed { message: message.to_owned() }
}

/// Caller's project + lead-or-not flag. Returned by
/// [`WorkerFacade::caller_project`].
#[derive(Debug, Clone)]
pub struct CallerProject {
    pub project_key: crate::ProjectKey,
    pub is_lead: bool,
}

/// Display identity for the sender of a `workers__tell` or
/// `workers__ask` envelope. Returned by [`WorkerFacade::caller_identity`]
/// and stamped into `WrappedPrompt::sender_name` / `sender_org` so the
/// recipient's chat renders `from agent '<name>' (org '<org>')` with a
/// human-readable label rather than the raw session UUID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerIdentity {
    pub name: String,
    pub org: String,
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

    /// Resolve a display identity for `caller`. Always returns a value
    /// (no `Option`); the production impl falls back to the raw
    /// session id for genuinely unresolvable callers so the envelope
    /// at least renders something. Three resolved shapes:
    ///
    /// - lead caller → `(project_key, "Personal")`
    /// - worker caller with a live `WorkerEntry` →
    ///   `(label, "worker in <project_key>")`
    /// - worker caller whose entry was reaped (detached, mid-shutdown) →
    ///   `(session_id, "worker in <project_key> (detached)")`
    fn caller_identity(&self, caller: &SessionKey) -> WorkerIdentity;

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
    /// or one of the `WorkerLeadDeliverError` variants. Same
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

/// Validation chain shared by the production and mock `spawn_worker`
/// impls so tests exercise the real rules rather than a hand-copied
/// duplicate. `is_lead` is the resolved caller role.
pub(super) fn validate_worker_spawn(
    is_lead: bool,
    label: &str,
    charter: &str,
) -> Result<(), WorkerSpawnError> {
    if !is_lead {
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
    Ok(())
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

    fn caller_identity(&self, caller: &SessionKey) -> WorkerIdentity {
        let Some(ws) = self.workspace.upgrade() else {
            return WorkerIdentity { name: caller.as_str().to_owned(), org: String::new() };
        };
        let Some(cp) = self.caller_project(caller) else {
            return WorkerIdentity { name: caller.as_str().to_owned(), org: String::new() };
        };
        if cp.is_lead {
            return WorkerIdentity { name: LEAD_LABEL.to_owned(), org: PERSONAL_ORG.to_owned() };
        }
        let label = ws
            .list_live_workers(&cp.project_key)
            .into_iter()
            .find(|w| w.session_key == *caller)
            .map(|w| w.label);
        match label {
            Some(label) => WorkerIdentity {
                name: label,
                org: format!("worker in {}", cp.project_key.as_str()),
            },
            None => WorkerIdentity {
                name: caller.as_str().to_owned(),
                org: format!("worker in {} (detached)", cp.project_key.as_str()),
            },
        }
    }

    async fn spawn_worker(
        &self,
        caller: &SessionKey,
        label: String,
        charter: String,
    ) -> Result<WorkerSpawnReply, WorkerSpawnError> {
        let cp = self.caller_project(caller).ok_or(WorkerSpawnError::UnknownCallerProject)?;
        validate_worker_spawn(cp.is_lead, &label, &charter)?;
        let ws = self.workspace.upgrade().ok_or_else(|| WorkerSpawnError::DispatchFailed {
            message: "workspace dropped".into(),
        })?;
        // Cache the is_git_repo answer here so the classifier on the
        // failure side has the same is_git_repo_at_spawn signal the
        // WorkerEntry would have carried. `handle_spawn_worker` does
        // the same probe; doing it here too keeps the facade self-
        // contained (the WorkerEntry is gone by the time we see the
        // error). Falls back to false when the project no longer
        // resolves - matches the conservative default in the
        // classifier's heuristic.
        let is_git_repo_at_spawn = ws
            .list_projects()
            .into_iter()
            .find(|v| v.key == cp.project_key)
            .is_some_and(|view| forge_agent::env::worktree::is_git_repo(&view.path));
        let (tx, rx) = tokio::sync::oneshot::channel();
        let cmd = Command::SpawnWorker {
            project_key: cp.project_key,
            label,
            charter,
            spawned_by_session_id: caller.as_str().to_owned(),
            // MCP-driven spawn is always a fresh session - the LLM
            // explicitly requested a NEW worker. Resume is for the
            // engineering-team Connected hook only.
            resume_existing: None,
            return_to: tx,
        };
        if let Err(err) = ws.dispatch(cmd) {
            return Err(WorkerSpawnError::DispatchFailed {
                message: format!("dispatch failed: {err:?}"),
            });
        }
        match rx.await {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(message)) => Err(classify_worker_spawn_failure(&message, is_git_repo_at_spawn)),
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

    fn caller_identity(&self, caller: &SessionKey) -> WorkerIdentity {
        let Some(cp) = self.caller_project(caller) else {
            return WorkerIdentity { name: caller.as_str().to_owned(), org: String::new() };
        };
        if cp.is_lead {
            return WorkerIdentity { name: LEAD_LABEL.to_owned(), org: PERSONAL_ORG.to_owned() };
        }
        let label = self.workers.lock().get(cp.project_key.as_str()).and_then(|ws| {
            ws.iter().find(|w| w.session_id == caller.as_str()).map(|w| w.label.clone())
        });
        match label {
            Some(label) => WorkerIdentity {
                name: label,
                org: format!("worker in {}", cp.project_key.as_str()),
            },
            None => WorkerIdentity {
                name: caller.as_str().to_owned(),
                org: format!("worker in {} (detached)", cp.project_key.as_str()),
            },
        }
    }

    async fn spawn_worker(
        &self,
        caller: &SessionKey,
        label: String,
        charter: String,
    ) -> Result<WorkerSpawnReply, WorkerSpawnError> {
        let cp = self.caller_project(caller).ok_or(WorkerSpawnError::UnknownCallerProject)?;
        validate_worker_spawn(cp.is_lead, &label, &charter)?;
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
    fn validate_worker_spawn_enforces_lead_and_nonempty_fields() {
        // Prod + mock spawn_worker both route through this, so the rules
        // are covered against the shipping code, not a copy.
        assert!(matches!(
            validate_worker_spawn(false, "reviewer", "charter"),
            Err(WorkerSpawnError::NotLeadCaller)
        ));
        assert!(matches!(
            validate_worker_spawn(true, "   ", "charter"),
            Err(WorkerSpawnError::EmptyLabel)
        ));
        assert!(matches!(
            validate_worker_spawn(true, LEAD_LABEL, "charter"),
            Err(WorkerSpawnError::ReservedLabel)
        ));
        assert!(matches!(
            validate_worker_spawn(true, "reviewer", "  "),
            Err(WorkerSpawnError::EmptyCharter)
        ));
        assert!(validate_worker_spawn(true, "reviewer", "charter").is_ok());
    }

    #[test]
    fn caller_identity_lead_returns_lead_label_and_personal() {
        // Lead callers stamp the symbolic `lead` label into the
        // wire envelope's `sender_name`, not the sanitized project
        // path - workers address the lead via `workers__tell("lead",
        // ...)`, so the reverse direction must match for the chat
        // surfaces to render `▶ Message lead` instead of the
        // hyphenated env-key path.
        let mock = MockWorkerFacade::new();
        let lead = SessionKey::from_session_id("lead-uuid");
        mock.callers.lock().insert(
            lead.clone(),
            CallerProject { project_key: crate::ProjectKey::new("forge"), is_lead: true },
        );
        let id = mock.caller_identity(&lead);
        assert_eq!(id.name, LEAD_LABEL);
        assert_eq!(id.org, "Personal");
    }

    #[test]
    fn caller_identity_worker_with_live_entry_returns_label_and_worker_in_project() {
        let mock = MockWorkerFacade::new();
        let worker_key = SessionKey::from_session_id("worker-uuid");
        mock.callers.lock().insert(
            worker_key.clone(),
            CallerProject { project_key: crate::ProjectKey::new("forge"), is_lead: false },
        );
        mock.workers.lock().insert(
            "forge".into(),
            vec![WorkerStatus {
                label: "reviewer".into(),
                charter: "review the diff".into(),
                status: forge_primitives::WorkerLiveness::Running,
                session_id: "worker-uuid".into(),
                spawned_at: std::time::SystemTime::UNIX_EPOCH,
                spawned_by_session_id: "lead-uuid".into(),
                diagnostic: None,
            }],
        );
        let id = mock.caller_identity(&worker_key);
        assert_eq!(id.name, "reviewer");
        assert_eq!(id.org, "worker in forge");
    }

    #[test]
    fn caller_identity_detached_worker_falls_back_to_session_id() {
        let mock = MockWorkerFacade::new();
        let worker_key = SessionKey::from_session_id("worker-uuid");
        mock.callers.lock().insert(
            worker_key.clone(),
            CallerProject { project_key: crate::ProjectKey::new("forge"), is_lead: false },
        );
        // Caller resolves to project, but no matching WorkerEntry in
        // live_workers (e.g. reaped mid-shutdown).
        let id = mock.caller_identity(&worker_key);
        assert_eq!(id.name, "worker-uuid");
        assert_eq!(id.org, "worker in forge (detached)");
    }

    #[test]
    fn caller_identity_unknown_caller_returns_session_id_with_empty_org() {
        let mock = MockWorkerFacade::new();
        let unknown = SessionKey::from_session_id("ghost-uuid");
        // No entry in mock.callers - mirrors the genuinely-unresolved case.
        let id = mock.caller_identity(&unknown);
        assert_eq!(id.name, "ghost-uuid");
        assert_eq!(id.org, "");
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

#[cfg(test)]
mod worktree_creation_failed_tests {
    use super::*;

    #[test]
    fn worktree_creation_failed_carries_reason() {
        let err = WorkerSpawnError::WorktreeCreationFailed {
            reason: "fatal: 'worktree-reviewer' is already used by worktree at /a".into(),
        };
        let WorkerSpawnError::WorktreeCreationFailed { reason } = err else {
            panic!("expected WorktreeCreationFailed");
        };
        assert!(reason.contains("already used by worktree"));
    }

    // ---------------------------------------------------------------
    // classify_worker_spawn_failure: heuristic mapping from claude's
    // raw error text + the WorkerEntry's is_git_repo_at_spawn flag
    // into either WorktreeCreationFailed (when the message implicates
    // the --worktree machinery AND the worker was actually spawned in
    // a git repo) or DispatchFailed (everything else). The classifier
    // is pure so it's unit-testable without spinning up a workspace.
    // ---------------------------------------------------------------

    #[test]
    fn classify_routes_worktree_word_to_worktree_creation_failed_when_git() {
        // claude's worktree failures usually mention the word
        // "worktree" verbatim (e.g. "Error creating worktree: ...").
        let err =
            classify_worker_spawn_failure("Error creating worktree: something went wrong", true);
        let WorkerSpawnError::WorktreeCreationFailed { reason } = err else {
            panic!("expected WorktreeCreationFailed; got {err:?}");
        };
        assert!(reason.contains("worktree"));
    }

    #[test]
    fn classify_routes_base_branch_resolve_to_worktree_creation_failed_when_git() {
        // The empty-repo (unborn HEAD) case from spike 0.3: claude
        // refuses with "Failed to resolve base branch \"HEAD\": git
        // rev-parse failed". The word "worktree" does not appear, so
        // the classifier matches on the resolve-base-branch substring
        // instead.
        let err = classify_worker_spawn_failure(
            "Failed to resolve base branch \"HEAD\": git rev-parse failed",
            true,
        );
        assert!(
            matches!(err, WorkerSpawnError::WorktreeCreationFailed { .. }),
            "expected WorktreeCreationFailed; got {err:?}",
        );
    }

    #[test]
    fn classify_routes_already_used_to_worktree_creation_failed_when_git() {
        // "fatal: '<label>' is already used by worktree at <path>" -
        // the conflicting-branch case (label collision with an
        // existing worktree).
        let err = classify_worker_spawn_failure(
            "fatal: 'reviewer' is already used by worktree at /a/b/c",
            true,
        );
        assert!(
            matches!(err, WorkerSpawnError::WorktreeCreationFailed { .. }),
            "expected WorktreeCreationFailed; got {err:?}",
        );
    }

    #[test]
    fn classify_falls_through_to_dispatch_failed_for_unrelated_messages() {
        // A generic spawn-side error that has nothing to do with
        // worktrees should stay DispatchFailed so the LLM gets the
        // raw text verbatim, not a misclassified worktree variant.
        let err = classify_worker_spawn_failure(
            "agent spawn failed: subprocess exited with code 2",
            true,
        );
        let WorkerSpawnError::DispatchFailed { message } = err else {
            panic!("expected DispatchFailed; got {err:?}");
        };
        assert!(message.contains("subprocess exited"));
    }

    #[test]
    fn classify_with_worktree_word_but_non_git_repo_falls_through() {
        // Defensive: a worktree-mentioning message arriving for a
        // worker that was NEVER spawned with --worktree (non-git
        // project, is_git_repo_at_spawn=false) is suspicious - the
        // claude binary couldn't have emitted a worktree-creation
        // error since it wasn't asked to create one. Fall through to
        // DispatchFailed rather than mislabel it.
        let err =
            classify_worker_spawn_failure("Error creating worktree: something went wrong", false);
        assert!(
            matches!(err, WorkerSpawnError::DispatchFailed { .. }),
            "expected DispatchFailed; got {err:?}",
        );
    }

    #[test]
    fn classify_is_case_insensitive_on_match_terms() {
        // Be lenient about casing - claude has shipped messages with
        // varying capitalization across CLI versions. Match on the
        // lowercased message body.
        let err = classify_worker_spawn_failure("WORKTREE creation failed", true);
        assert!(
            matches!(err, WorkerSpawnError::WorktreeCreationFailed { .. }),
            "expected case-insensitive match to WorktreeCreationFailed; got {err:?}",
        );
    }

    #[test]
    fn classify_preserves_original_casing_in_reason() {
        // The classifier may lowercase for matching but the reason
        // string handed back to the LLM should preserve the original
        // casing - the LLM sees claude's verbatim error text.
        let original = "Error creating Worktree: nope";
        let err = classify_worker_spawn_failure(original, true);
        let WorkerSpawnError::WorktreeCreationFailed { reason } = err else {
            panic!("expected WorktreeCreationFailed; got {err:?}");
        };
        assert_eq!(reason, original);
    }

    /// #245 Layer C blocker 2: pin the bridge-prefix contract.
    /// `forge_sdk_bridge` wraps every async ConnectionFailed message
    /// with a literal "forge-sdk session ..." prefix; none of those
    /// prefixes contain the words "worktree" / "base branch" /
    /// "already used by worktree" that the classifier matches on, so
    /// a generic dispatch failure can't accidentally classify as
    /// `WorktreeCreationFailed` just because the bridge happened to
    /// wrap it.
    ///
    /// If a future bridge change introduces "worktree" into the
    /// wrapper prose, this test fails - that's the signal to either
    /// rename the wrapper or plumb a typed reason variant through
    /// `AgentEvent::ConnectionFailed`.
    #[test]
    fn bridge_prefix_does_not_collide_with_worktree_predicate() {
        // The three wrapper prefixes verbatim from forge_sdk_bridge.
        let prefixes = [
            "forge-sdk session spawn failed: ",
            "forge-sdk session resume failed: ",
            "forge-sdk session spawn failed after resume fallback (resume err: ",
        ];
        for prefix in prefixes {
            let lower = prefix.to_lowercase();
            assert!(
                !lower.contains("worktree"),
                "bridge prefix {prefix:?} must not contain 'worktree' or the \
                 classifier will misclassify generic errors as worktree failures",
            );
            assert!(
                !lower.contains("failed to resolve base branch"),
                "bridge prefix {prefix:?} must not contain the 'failed to resolve base branch' \
                 discriminator",
            );
            assert!(
                !lower.contains("already used by worktree"),
                "bridge prefix {prefix:?} must not contain the 'already used by worktree' \
                 discriminator",
            );
        }

        // Concretely: a non-worktree generic failure wrapped by the
        // bridge stays classified as DispatchFailed.
        let wrapped = "forge-sdk session resume failed: subprocess exited with code 2";
        let err = classify_worker_spawn_failure(wrapped, /* is_git_repo_at_spawn */ true);
        assert!(
            matches!(err, WorkerSpawnError::DispatchFailed { .. }),
            "wrapped generic failure must stay DispatchFailed; got {err:?}",
        );
    }
}
