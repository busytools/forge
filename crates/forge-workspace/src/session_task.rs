//! Per-session actor that receives `Command`s. Phase 1 implements
//! `RespondPermission` / `RespondQuestion` / `RespondElicitation`
//! end-to-end (pop the oneshot from `DomainSession.pending_interactions`,
//! fulfill it). Other commands log + drop until Phase 2 wires them
//! to `AgentHandle` methods.
//!
//! Event drain (`AgentHandle::take_events()`) stays on
//! `bridge_lifecycle` in Phase 1; Phase 3 moves it here.

use std::sync::Arc;

use forge_agent::AgentHandle;
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
