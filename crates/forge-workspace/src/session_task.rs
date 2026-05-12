//! Per-session actor. Owns one `Arc<AgentHandle>`'s event drain
//! (`AgentHandle::take_events`) plus a per-session `Command`
//! receiver. Translates each [`AgentEvent`] into the matching
//! [`SessionUpdate`] envelope and emits onto the workspace-wide
//! fan-in channel. Mutates [`DomainSession`] inline before each
//! emit so workspace-side projections stay current.
//!
//! Phase 4 made `SessionTask::run` the sole consumer of the
//! AgentHandle event stream; `bridge_lifecycle` is gone.

use std::sync::Arc;

use forge_agent::AgentHandle;
use forge_agent::client::AgentEvent;
use forge_primitives::SessionId;
use forge_primitives::runtime::SessionLifecycleState;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::SessionKey;
use crate::domain_session::DomainSession;
use crate::protocol::{Command, PendingInteractionSlot, SessionUpdate};

pub(crate) struct SessionTask {
    pub(crate) key: SessionKey,
    pub(crate) handle: Arc<AgentHandle>,
    pub(crate) command_rx: mpsc::UnboundedReceiver<Command>,
    pub(crate) domain: Arc<Mutex<DomainSession>>,
    pub(crate) update_tx: mpsc::UnboundedSender<SessionUpdate>,
    /// Synthetic key tagged by `Workspace::attach_spawn_key` so the
    /// task can emit `SessionUpdate::KeyRenamed { from: spawn_key,
    /// to: real_key }` ahead of the first `Connected` emit. Cleared
    /// after the first migration.
    pub(crate) spawn_key: Option<SessionKey>,
    /// Tracks whether the first `Connected` has been emitted. The
    /// second-and-beyond Connected on the same task drives
    /// `SessionUpdate::SessionReplaced` instead (covers `/new`,
    /// login, logout flows).
    pub(crate) connected_once: bool,
    /// Weak reference to the parent [`crate::Workspace`]. Used inside
    /// [`Self::translate_event`]'s `Connected` / `SessionReplaced`
    /// arms to call back into `Workspace::record_connected_session`
    /// so the project catalog stays current as freshly-spawned
    /// sessions reach Connected. `Weak` avoids the Workspace<->Task
    /// reference cycle (Workspace holds Task's command_tx; Task
    /// holds Workspace).
    pub(crate) workspace: std::sync::Weak<crate::Workspace>,
}

impl SessionTask {
    pub(crate) async fn run(mut self) {
        tracing::debug!(
            target: "forge_workspace::session_task",
            key = %self.key.as_str(),
            "session task started"
        );
        // Take the agent's event receiver. The bridge seeds it on
        // spawn; if it's already been taken (future refactor double-
        // tapping), surface as ConnectionFailed + return.
        let Some(mut event_rx) = self.handle.take_events() else {
            tracing::error!(
                target: "forge_workspace::session_task",
                key = %self.key.as_str(),
                "AgentHandle::take_events returned None; session task aborting"
            );
            let fail_key = self.spawn_key.clone().unwrap_or_else(|| self.key.clone());
            let _ = self.update_tx.send(SessionUpdate::ConnectionFailed {
                key: fail_key,
                message: "agent event receiver unavailable".to_owned(),
                fatal: false,
            });
            return;
        };

        // Forge-side display_name is known the instant the workspace
        // picks an account. Emit ForgeAccountIdentity now so welcome
        // rendering shows the right label from the first frame.
        if let Some(display_name) = self.handle.display_name() {
            {
                let mut guard = self.domain.lock();
                guard.active_account_display_name = Some(display_name.clone());
            }
            let id_key = self.spawn_key.clone().unwrap_or_else(|| self.key.clone());
            let _ = self
                .update_tx
                .send(SessionUpdate::ForgeAccountIdentity { key: id_key, display_name });
        }

        loop {
            tokio::select! {
                maybe_event = event_rx.recv() => {
                    let Some(event) = maybe_event else { break; };
                    self.translate_event(event);
                }
                maybe_cmd = self.command_rx.recv() => {
                    let Some(cmd) = maybe_cmd else { break; };
                    self.execute_command(cmd);
                }
            }
        }
        tracing::info!(
            target: "forge_workspace::session_task",
            key = %self.key.as_str(),
            "session task exiting (agent event channel closed)"
        );
    }

    /// Translate one `AgentEvent` into the matching `SessionUpdate`
    /// (or pair of updates for `Connected`, which also emits
    /// `KeyRenamed` if a synthetic spawn key is pending). Mutates
    /// `DomainSession` inline before the emit so workspace-side
    /// projections stay current.
    fn translate_event(&mut self, event: AgentEvent) {
        // First, update DomainSession in-place.
        {
            let mut guard = self.domain.lock();
            apply_event_to_domain(&mut guard, &event);
        }

        // Mirror Connected/SessionReplaced into the project catalog
        // so the Projects pane's drilldown reflects newly-spawned
        // sessions without forcing a full disk re-scan. Pre-Phase-4
        // this was done by `Workspace::record_event_for_domain`; now
        // it's inlined here since `SessionTask` owns the event drain.
        if let AgentEvent::Connected { session_id, cwd, .. }
        | AgentEvent::SessionReplaced { session_id, cwd, .. } = &event
            && !cwd.is_empty()
            && let Some(workspace) = self.workspace.upgrade()
        {
            workspace.record_connected_session(cwd, session_id, None);
        }

        match event {
            AgentEvent::Connected {
                session_id,
                cwd,
                current_model,
                available_models,
                mode,
                history_updates,
            } => {
                let history = history_updates.unwrap_or_default();
                let real_key = SessionKey::from_session_id(session_id.clone());
                if self.connected_once {
                    let _ = self.update_tx.send(SessionUpdate::SessionReplaced {
                        key: real_key,
                        session_id: SessionId::new(session_id),
                        cwd,
                        current_model,
                        available_models,
                        mode,
                        history,
                        conn: Arc::clone(&self.handle),
                    });
                } else {
                    self.connected_once = true;
                    // First Connected: emit KeyRenamed { from:
                    // spawn_key, to: real_key } so the TUI migrates
                    // its synthetic spawn bucket onto the real
                    // session UUID atomically.
                    if let Some(spawn_key) = self.spawn_key.take()
                        && spawn_key.as_str() != real_key.as_str()
                    {
                        let _ = self.update_tx.send(SessionUpdate::KeyRenamed {
                            from: spawn_key,
                            to: real_key.clone(),
                        });
                    }
                    let _ = self.update_tx.send(SessionUpdate::Connected {
                        key: real_key,
                        session_id: SessionId::new(session_id),
                        cwd,
                        current_model,
                        available_models,
                        mode,
                        history,
                        conn: Arc::clone(&self.handle),
                    });
                }
            }
            AgentEvent::SessionReplaced {
                session_id,
                cwd,
                current_model,
                available_models,
                mode,
                history_updates,
            } => {
                let history = history_updates.unwrap_or_default();
                let real_key = SessionKey::from_session_id(session_id.clone());
                let _ = self.update_tx.send(SessionUpdate::SessionReplaced {
                    key: real_key,
                    session_id: SessionId::new(session_id),
                    cwd,
                    current_model,
                    available_models,
                    mode,
                    history,
                    conn: Arc::clone(&self.handle),
                });
            }
            AgentEvent::AuthRequired { method_name, method_description } => {
                let key = self.spawn_key.clone().unwrap_or_else(|| self.key.clone());
                let _ = self.update_tx.send(SessionUpdate::AuthRequired {
                    key,
                    method_name,
                    method_description,
                });
            }
            AgentEvent::ConnectionFailed { message } => {
                let key = self.spawn_key.clone().unwrap_or_else(|| self.key.clone());
                let _ = self.update_tx.send(SessionUpdate::ConnectionFailed {
                    key,
                    message,
                    fatal: false,
                });
            }
            AgentEvent::PermissionRequest { session_id, request } => {
                let session_key = SessionKey::from_session_id(session_id.clone());
                let tool_call_id = request.tool_call.tool_call_id.clone();
                let wire_request = request;
                let (response_tx, response_rx) =
                    tokio::sync::oneshot::channel::<forge_primitives::PermissionOutcome>();
                {
                    let mut guard = self.domain.lock();
                    guard.pending_interactions.insert(
                        tool_call_id.clone(),
                        PendingInteractionSlot::Permission(response_tx),
                    );
                }
                if self
                    .update_tx
                    .send(SessionUpdate::PermissionRequest {
                        key: session_key,
                        tool_id: tool_call_id.clone(),
                        request: wire_request,
                    })
                    .is_ok()
                {
                    spawn_permission_response_forwarder(
                        Arc::clone(&self.handle),
                        response_rx,
                        session_id,
                        tool_call_id,
                    );
                }
            }
            AgentEvent::QuestionRequest { session_id, request } => {
                let session_key = SessionKey::from_session_id(session_id.clone());
                let tool_call_id = request.tool_call.tool_call_id.clone();
                let wire_request = request;
                let (response_tx, response_rx) =
                    tokio::sync::oneshot::channel::<forge_primitives::QuestionOutcome>();
                {
                    let mut guard = self.domain.lock();
                    guard.pending_interactions.insert(
                        tool_call_id.clone(),
                        PendingInteractionSlot::Question(response_tx),
                    );
                }
                if self
                    .update_tx
                    .send(SessionUpdate::QuestionRequest {
                        key: session_key,
                        tool_id: tool_call_id.clone(),
                        request: wire_request,
                    })
                    .is_ok()
                {
                    spawn_question_response_forwarder(
                        Arc::clone(&self.handle),
                        response_rx,
                        session_id,
                        tool_call_id,
                    );
                }
            }
            AgentEvent::ElicitationRequest { session_id, request } => {
                let session_key = SessionKey::from_session_id(session_id);
                let elicitation_id = request.elicitation_id.clone().unwrap_or_default();
                let _ = self.update_tx.send(SessionUpdate::McpElicitationRequest {
                    key: session_key,
                    elicitation_id,
                    request,
                });
            }
            AgentEvent::ElicitationComplete { session_id, elicitation_id, server_name } => {
                let session_key = SessionKey::from_session_id(session_id);
                let _ = self.update_tx.send(SessionUpdate::McpElicitationCompleted {
                    key: session_key,
                    elicitation_id,
                    server_name,
                });
            }
            AgentEvent::McpAuthRedirect { session_id, redirect } => {
                let session_key = SessionKey::from_session_id(session_id);
                let _ = self
                    .update_tx
                    .send(SessionUpdate::McpAuthRedirect { key: session_key, redirect });
            }
            AgentEvent::McpOperationError { session_id, error } => {
                let session_key = SessionKey::from_session_id(session_id);
                let _ = self
                    .update_tx
                    .send(SessionUpdate::McpOperationError { key: session_key, error });
            }
            AgentEvent::SlashError { session_id, message } => {
                let session_key = SessionKey::from_session_id(session_id);
                let _ = self
                    .update_tx
                    .send(SessionUpdate::SlashCommandError { key: session_key, message });
            }
            AgentEvent::RuntimeReloadCompleted { session_id } => {
                let _ = self.update_tx.send(SessionUpdate::RuntimeReloadCompleted { session_id });
            }
            AgentEvent::RuntimeReloadFailed { session_id, message } => {
                let _ =
                    self.update_tx.send(SessionUpdate::RuntimeReloadFailed { session_id, message });
            }
            AgentEvent::SessionsListed { sessions } => {
                let _ = self.update_tx.send(SessionUpdate::SessionsListed { sessions });
            }
            AgentEvent::StatusSnapshot { session_id, account, forge_account } => {
                let _ = self.update_tx.send(SessionUpdate::StatusSnapshot {
                    session_id,
                    account,
                    forge_account,
                });
            }
            AgentEvent::OauthCredentialsSnapshot { session_id, credentials } => {
                let _ = self
                    .update_tx
                    .send(SessionUpdate::OauthCredentialsSnapshot { session_id, credentials });
            }
            AgentEvent::GitContextSnapshot { session_id, context } => {
                let _ =
                    self.update_tx.send(SessionUpdate::GitContextSnapshot { session_id, context });
            }
            AgentEvent::ContextUsage { session_id, percentage } => {
                let _ = self
                    .update_tx
                    .send(SessionUpdate::ContextUsageSnapshot { session_id, percentage });
            }
            AgentEvent::McpSnapshot { session_id, servers, error } => {
                let _ =
                    self.update_tx.send(SessionUpdate::McpSnapshot { session_id, servers, error });
            }
            AgentEvent::SdkMessage { session_id, msg } => {
                let _ = self.update_tx.send(SessionUpdate::ChatAppended { session_id, msg });
            }
            AgentEvent::HookObservation {
                session_id,
                tool_use_id,
                permission_mode,
                effort,
                agent_id,
                agent_type,
            } => {
                let _ = self.update_tx.send(SessionUpdate::HookObservation {
                    session_id,
                    tool_use_id,
                    permission_mode,
                    effort,
                    agent_id,
                    agent_type,
                });
            }
        }
    }

    fn execute_command(&self, cmd: Command) {
        match cmd {
            Command::RespondPermission { key: _, tool_id, outcome } => {
                let mut guard = self.domain.lock();
                match guard.pending_interactions.remove(&tool_id) {
                    Some(PendingInteractionSlot::Permission(tx)) => {
                        if tx.send(outcome).is_err() {
                            tracing::warn!(
                                target: "forge_workspace::session_task",
                                key = %self.key.as_str(),
                                tool_id = %tool_id,
                                "permission oneshot receiver dropped before response could be sent"
                            );
                        }
                    }
                    Some(other) => {
                        tracing::warn!(
                            target: "forge_workspace::session_task",
                            key = %self.key.as_str(),
                            tool_id = %tool_id,
                            slot = ?other,
                            "RespondPermission expected Permission slot; got different kind. Dropping."
                        );
                    }
                    None => {
                        tracing::warn!(
                            target: "forge_workspace::session_task",
                            key = %self.key.as_str(),
                            tool_id = %tool_id,
                            "RespondPermission found no pending interaction (already responded or expired)"
                        );
                    }
                }
            }
            Command::RespondQuestion { key: _, tool_id, outcome } => {
                let mut guard = self.domain.lock();
                match guard.pending_interactions.remove(&tool_id) {
                    Some(PendingInteractionSlot::Question(tx)) => {
                        if tx.send(outcome).is_err() {
                            tracing::warn!(
                                target: "forge_workspace::session_task",
                                key = %self.key.as_str(),
                                tool_id = %tool_id,
                                "question oneshot receiver dropped"
                            );
                        }
                    }
                    Some(other) => {
                        tracing::warn!(
                            target: "forge_workspace::session_task",
                            key = %self.key.as_str(),
                            tool_id = %tool_id,
                            slot = ?other,
                            "RespondQuestion got non-Question slot. Dropping."
                        );
                    }
                    None => {
                        tracing::warn!(
                            target: "forge_workspace::session_task",
                            key = %self.key.as_str(),
                            tool_id = %tool_id,
                            "RespondQuestion found no pending interaction"
                        );
                    }
                }
            }
            Command::RespondElicitation { key: _, elicitation_id, action } => {
                let mut guard = self.domain.lock();
                match guard.pending_interactions.remove(&elicitation_id) {
                    Some(PendingInteractionSlot::Elicitation(tx)) => {
                        if tx.send(action).is_err() {
                            tracing::warn!(
                                target: "forge_workspace::session_task",
                                key = %self.key.as_str(),
                                elicitation_id = %elicitation_id,
                                "elicitation oneshot receiver dropped"
                            );
                        }
                    }
                    Some(other) => {
                        tracing::warn!(
                            target: "forge_workspace::session_task",
                            key = %self.key.as_str(),
                            elicitation_id = %elicitation_id,
                            slot = ?other,
                            "RespondElicitation got non-Elicitation slot. Dropping."
                        );
                    }
                    None => {
                        tracing::warn!(
                            target: "forge_workspace::session_task",
                            key = %self.key.as_str(),
                            elicitation_id = %elicitation_id,
                            "RespondElicitation found no pending interaction"
                        );
                    }
                }
            }
            other => {
                tracing::trace!(
                    target: "forge_workspace::session_task",
                    key = %self.key.as_str(),
                    command = ?other,
                    "command received (Phase 4 stub; future phases wire to AgentHandle)"
                );
                drop(other);
            }
        }
    }
}

/// Apply an [`AgentEvent`] to a [`DomainSession`]. Pure mutation; no
/// I/O, no async, no sends. Called from inside
/// [`SessionTask::translate_event`] under the domain's lock.
pub(crate) fn apply_event_to_domain(domain: &mut DomainSession, event: &AgentEvent) {
    match event {
        AgentEvent::Connected { session_id, cwd, .. } => {
            // First Connected on this bridge: stamp session_id, set
            // cwd, lifecycle = Idle.
            if domain.session_id.is_none() {
                domain.session_id = Some(SessionId::new(session_id.clone()));
            }
            if !cwd.is_empty() {
                domain.cwd_raw.clone_from(cwd);
            }
            domain.lifecycle_state = SessionLifecycleState::Idle;
        }
        AgentEvent::SessionReplaced { session_id, cwd, .. } => {
            // /new, login, logout. Bump epoch; new session id; cwd
            // may change.
            domain.session_id = Some(SessionId::new(session_id.clone()));
            if !cwd.is_empty() {
                domain.cwd_raw.clone_from(cwd);
            }
            domain.session_scope_epoch = domain.session_scope_epoch.wrapping_add(1);
            domain.lifecycle_state = SessionLifecycleState::Idle;
        }
        AgentEvent::PermissionRequest { .. }
        | AgentEvent::QuestionRequest { .. }
        | AgentEvent::ElicitationRequest { .. } => {
            domain.lifecycle_state = SessionLifecycleState::Attention;
        }
        AgentEvent::StatusSnapshot { account, forge_account, .. } => {
            domain.account_info = Some(account.clone());
            domain.active_account_display_name =
                forge_account.as_ref().map(|fa| fa.display_name.clone());
        }
        AgentEvent::AuthRequired { .. } => {
            domain.lifecycle_state = SessionLifecycleState::AuthRequired;
        }
        AgentEvent::ConnectionFailed { .. } => {
            domain.lifecycle_state = SessionLifecycleState::Failed;
        }
        _ => {}
    }
}

/// Forward an awaited permission outcome to the agent so the bridge
/// can complete the round-trip with the CLI subprocess.
fn spawn_permission_response_forwarder(
    agent: Arc<AgentHandle>,
    response_rx: tokio::sync::oneshot::Receiver<forge_primitives::PermissionOutcome>,
    session_id: String,
    tool_call_id: String,
) {
    tokio::task::spawn(async move {
        let Ok(outcome) = response_rx.await else {
            tracing::warn!(
                target: "forge_workspace::session_task",
                session_id = %session_id,
                tool_call_id = %tool_call_id,
                "permission response channel closed before forwarding"
            );
            return;
        };
        let selected_option = match &outcome {
            forge_primitives::PermissionOutcome::Selected { option_id } => option_id.clone(),
            forge_primitives::PermissionOutcome::Cancelled => "cancelled".to_owned(),
        };
        let session_id_for_log = session_id.clone();
        let tool_call_id_for_log = tool_call_id.clone();
        match agent.permission_response(session_id, tool_call_id, outcome) {
            Ok(()) => {
                tracing::info!(
                    target: "forge_workspace::session_task",
                    session_id = %session_id_for_log,
                    tool_call_id = %tool_call_id_for_log,
                    selected_option = %selected_option,
                    "permission response forwarded to bridge"
                );
            }
            Err(err) => {
                tracing::error!(
                    target: "forge_workspace::session_task",
                    session_id = %session_id_for_log,
                    tool_call_id = %tool_call_id_for_log,
                    selected_option = %selected_option,
                    error = %err,
                    "failed to forward permission response to bridge"
                );
            }
        }
    });
}

/// Forward an awaited question outcome to the agent so the bridge
/// can complete the round-trip with the CLI subprocess.
fn spawn_question_response_forwarder(
    agent: Arc<AgentHandle>,
    response_rx: tokio::sync::oneshot::Receiver<forge_primitives::QuestionOutcome>,
    session_id: String,
    tool_call_id: String,
) {
    tokio::task::spawn(async move {
        let Ok(outcome) = response_rx.await else {
            tracing::warn!(
                target: "forge_workspace::session_task",
                session_id = %session_id,
                tool_call_id = %tool_call_id,
                "question response channel closed before forwarding"
            );
            return;
        };
        let selected_option_count = match &outcome {
            forge_primitives::QuestionOutcome::Answered { selected_option_ids, .. } => {
                selected_option_ids.len()
            }
            forge_primitives::QuestionOutcome::Cancelled => 0,
        };
        let session_id_for_log = session_id.clone();
        let tool_call_id_for_log = tool_call_id.clone();
        match agent.question_response(session_id, tool_call_id, outcome) {
            Ok(()) => {
                tracing::info!(
                    target: "forge_workspace::session_task",
                    session_id = %session_id_for_log,
                    tool_call_id = %tool_call_id_for_log,
                    selected_option_count,
                    "question response forwarded to bridge"
                );
            }
            Err(err) => {
                tracing::error!(
                    target: "forge_workspace::session_task",
                    session_id = %session_id_for_log,
                    tool_call_id = %tool_call_id_for_log,
                    selected_option_count,
                    error = %err,
                    "failed to forward question response to bridge"
                );
            }
        }
    });
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use forge_agent::Agent;
    use forge_primitives::AccountInfo;

    fn empty_domain() -> DomainSession {
        let (handle, _rx) = Agent::testing_stub();
        DomainSession::new(SessionKey::from_str_for_test("test"), Some(Arc::new(handle)))
    }

    /// `apply_event_to_domain` on `AgentEvent::Connected` stamps
    /// `session_id`, sets `cwd_raw`, and flips lifecycle to `Idle`.
    /// First-Connected behavior — the contract is "fresh session
    /// just landed; bucket reflects its identity".
    #[test]
    fn translate_connected_stamps_session_id_and_cwd() {
        let mut domain = empty_domain();
        assert!(domain.session_id.is_none());
        assert_eq!(domain.cwd_raw, "");

        apply_event_to_domain(
            &mut domain,
            &AgentEvent::Connected {
                session_id: "real-uuid-1".to_owned(),
                cwd: "/proj".to_owned(),
                current_model: forge_primitives::CurrentModel {
                    resolved_id: "claude".to_owned(),
                    display_name_short: "claude".to_owned(),
                    display_name_long: "claude".to_owned(),
                    requested_id: None,
                    catalog_id: None,
                    supports_effort: false,
                    supported_effort_levels: Vec::new(),
                    supports_fast_mode: None,
                    supports_auto_mode: None,
                    supports_adaptive_thinking: None,
                    is_authoritative: true,
                },
                available_models: Vec::new(),
                mode: None,
                history_updates: None,
            },
        );

        assert_eq!(
            domain.session_id.as_ref().map(std::string::ToString::to_string),
            Some("real-uuid-1".to_owned())
        );
        assert_eq!(domain.cwd_raw, "/proj");
        assert!(matches!(domain.lifecycle_state, SessionLifecycleState::Idle));
    }

    /// `SessionReplaced` (e.g., `/new`, login, logout) bumps the
    /// session_scope_epoch and replaces the session_id. Pre-existing
    /// session state is invalidated by the epoch bump.
    #[test]
    fn translate_session_replaced_bumps_epoch() {
        let mut domain = empty_domain();
        domain.session_id = Some(SessionId::new("old-uuid"));
        domain.session_scope_epoch = 5;

        apply_event_to_domain(
            &mut domain,
            &AgentEvent::SessionReplaced {
                session_id: "new-uuid".to_owned(),
                cwd: "/new-proj".to_owned(),
                current_model: forge_primitives::CurrentModel {
                    resolved_id: "claude".to_owned(),
                    display_name_short: "claude".to_owned(),
                    display_name_long: "claude".to_owned(),
                    requested_id: None,
                    catalog_id: None,
                    supports_effort: false,
                    supported_effort_levels: Vec::new(),
                    supports_fast_mode: None,
                    supports_auto_mode: None,
                    supports_adaptive_thinking: None,
                    is_authoritative: true,
                },
                available_models: Vec::new(),
                mode: None,
                history_updates: None,
            },
        );

        assert_eq!(
            domain.session_id.as_ref().map(std::string::ToString::to_string),
            Some("new-uuid".to_owned())
        );
        assert_eq!(domain.cwd_raw, "/new-proj");
        assert_eq!(domain.session_scope_epoch, 6, "epoch incremented");
        assert!(matches!(domain.lifecycle_state, SessionLifecycleState::Idle));
    }

    /// `PermissionRequest` / `QuestionRequest` / `ElicitationRequest`
    /// flip lifecycle to `Attention` (the Projects pane glyph
    /// surfaces it).
    #[test]
    fn translate_permission_request_flips_attention() {
        let mut domain = empty_domain();
        domain.lifecycle_state = SessionLifecycleState::Idle;

        apply_event_to_domain(
            &mut domain,
            &AgentEvent::PermissionRequest {
                session_id: "uuid".to_owned(),
                request: forge_primitives::PermissionRequest {
                    tool_call: forge_primitives::ToolCall {
                        tool_call_id: "tool-1".to_owned(),
                        title: "test".to_owned(),
                        kind: "execute".to_owned(),
                        status: "pending".to_owned(),
                        content: Vec::new(),
                        raw_input: None,
                        raw_output: None,
                        output_metadata: None,
                        task_metadata: None,
                        locations: Vec::new(),
                        meta: None,
                    },
                    options: Vec::new(),
                    display: None,
                },
            },
        );

        assert!(matches!(domain.lifecycle_state, SessionLifecycleState::Attention));
    }

    /// `StatusSnapshot` captures `account_info` and
    /// `active_account_display_name` from the wire envelope. Verifies
    /// the workspace-side cache is current for any TUI reader that
    /// reads via `Workspace::domain_session_for`.
    #[test]
    fn translate_status_snapshot_captures_account_fields() {
        let mut domain = empty_domain();
        assert!(domain.account_info.is_none());

        apply_event_to_domain(
            &mut domain,
            &AgentEvent::StatusSnapshot {
                session_id: "uuid".to_owned(),
                account: AccountInfo {
                    email: Some("user@example.com".to_owned()),
                    ..Default::default()
                },
                forge_account: Some(forge_primitives::ForgeAccountIdentity::new(
                    "Subspace".to_owned(),
                )),
            },
        );

        assert_eq!(
            domain.account_info.as_ref().and_then(|a| a.email.as_deref()),
            Some("user@example.com")
        );
        assert_eq!(domain.active_account_display_name.as_deref(), Some("Subspace"));
    }

    /// `AuthRequired` flips lifecycle to the AuthRequired state.
    #[test]
    fn translate_auth_required_flips_lifecycle() {
        let mut domain = empty_domain();
        apply_event_to_domain(
            &mut domain,
            &AgentEvent::AuthRequired {
                method_name: "oauth".to_owned(),
                method_description: "Sign in with Claude".to_owned(),
            },
        );
        assert!(matches!(domain.lifecycle_state, SessionLifecycleState::AuthRequired));
    }

    /// `ConnectionFailed` flips lifecycle to `Failed` so the
    /// Projects pane glyph reflects the broken session.
    #[test]
    fn translate_connection_failed_flips_lifecycle() {
        let mut domain = empty_domain();
        apply_event_to_domain(
            &mut domain,
            &AgentEvent::ConnectionFailed { message: "boom".to_owned() },
        );
        assert!(matches!(domain.lifecycle_state, SessionLifecycleState::Failed));
    }
}
