//! Per-session actor that receives `Command`s. Phase 1 implements
//! `RespondPermission` / `RespondQuestion` / `RespondElicitation`
//! end-to-end (pop the oneshot from `DomainSession.pending_interactions`,
//! fulfill it). Other commands log + drop until Phase 2 wires them
//! to `AgentHandle` methods.
//!
//! Event drain (`AgentHandle::take_events()`) stays on
//! `bridge_lifecycle` in Phase 1; Phase 3 moves it here. Phase 2
//! adds [`apply_event_to_domain`], the pure-function state machine
//! that `Workspace::record_event_for_domain` invokes for each event.

use std::sync::Arc;

use forge_agent::AgentHandle;
use forge_agent::client::AgentEvent;
use forge_primitives::SessionId;
use forge_primitives::runtime::SessionLifecycleState;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::SessionKey;
use crate::domain_session::DomainSession;
use crate::protocol::{Command, PendingInteractionSlot};

pub(crate) struct SessionTask {
    pub(crate) key: SessionKey,
    /// `AgentHandle` clone retained for Phase 2's `Command::Prompt`/
    /// `Cancel`/`SetMode` etc. wiring. Currently unused — Phase 1
    /// only handles `Respond*` which doesn't touch the agent; Phase 2
    /// removes this allow when it wires the dispatch sites.
    #[allow(dead_code)]
    pub(crate) handle: Arc<AgentHandle>,
    pub(crate) command_rx: mpsc::UnboundedReceiver<Command>,
    pub(crate) domain: Arc<Mutex<DomainSession>>,
}

impl SessionTask {
    pub(crate) async fn run(mut self) {
        tracing::debug!(
            target: "forge_workspace::session_task",
            key = %self.key.as_str(),
            "session task started (Phase 1: Respond* implemented; other commands stubbed)",
        );
        while let Some(cmd) = self.command_rx.recv().await {
            self.execute_command(cmd);
        }
        tracing::info!(
            target: "forge_workspace::session_task",
            key = %self.key.as_str(),
            "command channel closed; session task exiting",
        );
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
                                "permission oneshot receiver dropped before response could be sent",
                            );
                        }
                    }
                    Some(other) => {
                        tracing::warn!(
                            target: "forge_workspace::session_task",
                            key = %self.key.as_str(),
                            tool_id = %tool_id,
                            slot = ?other,
                            "RespondPermission expected Permission slot; got different kind. Dropping.",
                        );
                    }
                    None => {
                        tracing::warn!(
                            target: "forge_workspace::session_task",
                            key = %self.key.as_str(),
                            tool_id = %tool_id,
                            "RespondPermission found no pending interaction (already responded or expired)",
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
                                "question oneshot receiver dropped",
                            );
                        }
                    }
                    Some(other) => {
                        tracing::warn!(
                            target: "forge_workspace::session_task",
                            key = %self.key.as_str(),
                            tool_id = %tool_id,
                            slot = ?other,
                            "RespondQuestion got non-Question slot. Dropping.",
                        );
                    }
                    None => {
                        tracing::warn!(
                            target: "forge_workspace::session_task",
                            key = %self.key.as_str(),
                            tool_id = %tool_id,
                            "RespondQuestion found no pending interaction",
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
                                "elicitation oneshot receiver dropped",
                            );
                        }
                    }
                    Some(other) => {
                        tracing::warn!(
                            target: "forge_workspace::session_task",
                            key = %self.key.as_str(),
                            elicitation_id = %elicitation_id,
                            slot = ?other,
                            "RespondElicitation got non-Elicitation slot. Dropping.",
                        );
                    }
                    None => {
                        tracing::warn!(
                            target: "forge_workspace::session_task",
                            key = %self.key.as_str(),
                            elicitation_id = %elicitation_id,
                            "RespondElicitation found no pending interaction",
                        );
                    }
                }
            }
            other => {
                tracing::trace!(
                    target: "forge_workspace::session_task",
                    key = %self.key.as_str(),
                    command = ?other,
                    "command received (Phase 1 stub; Phase 2 wires to AgentHandle)",
                );
                drop(other);
            }
        }
    }
}

/// Apply an [`AgentEvent`] to a [`DomainSession`]. Pure mutation; no
/// I/O, no async, no sends. Called from
/// [`crate::workspace::Workspace::record_event_for_domain`] under the
/// domain's lock.
///
/// Phase 2 covers the minimum needed to keep the workspace-side view
/// of session lifecycle/identity/cwd/account current. Phase 3
/// sub-phases extend coverage as TUI handlers migrate.
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
        // Other events: no DomainSession update in Phase 2. Phase 3
        // sub-phases add per-event projections as TUI handlers
        // migrate. Failures (`ConnectionFailed`, `AuthRequired`) keep
        // their existing TUI-side behavior; Phase 3 inverts.
        _ => {}
    }
}
