//! Per-session actor. Owns one `Arc<AgentHandle>`'s event drain
//! (`AgentHandle::take_events`) plus a per-session `Command`
//! receiver. Translates each [`AgentEvent`] into the matching
//! [`SessionUpdate`] envelope and emits onto the workspace-wide
//! fan-in channel. Mutates [`DomainSession`] inline before each
//! emit so workspace-side projections stay current.
//!
//! `SessionTask::run` is the sole consumer of the AgentHandle event
//! stream.

use std::sync::Arc;

use forge_agent::AgentHandle;
use forge_agent::client::AgentEvent;
use forge_primitives::SessionId;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::Instrument;

use crate::SessionKey;
use crate::domain_session::DomainSession;
use crate::protocol::{Command, PendingInteractionSlot, SessionUpdate};

pub(crate) struct SessionTask {
    pub(crate) key: SessionKey,
    pub(crate) handle: Arc<AgentHandle>,
    pub(crate) command_rx: mpsc::UnboundedReceiver<Command>,
    pub(crate) domain: Arc<Mutex<DomainSession>>,
    pub(crate) update_tx: mpsc::UnboundedSender<SessionUpdate>,
    /// Synthetic key tagged by `Workspace::get_agent_handle_with_spawn_key` so the
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
    /// [`Self::translate_event`]'s `Connected` arm to call back into
    /// `Workspace::record_connected_session`
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
            self.emit(SessionUpdate::ConnectionFailed {
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
            let id_key = self.spawn_key.clone().unwrap_or_else(|| self.key.clone());
            self.emit(SessionUpdate::ForgeAccountIdentity { key: id_key, display_name });
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
    /// `KeyRenamed` if a synthetic spawn key is pending). Updates
    /// `DomainSession` in-place before each emit.
    fn translate_event(&mut self, event: AgentEvent) {
        // First, update DomainSession in-place.
        {
            let mut guard = self.domain.lock();
            apply_event_to_domain(&mut guard, &event);
        }

        // Mirror Connected into the project catalog so the Projects
        // pane's drilldown reflects newly-spawned sessions without
        // forcing a full disk re-scan.
        if let AgentEvent::Connected { session_id, cwd, .. } = &event
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
                // Realign the workspace's per-task registrations
                // (`pool`, `command_senders`, `domain_handles`) onto
                // the real session UUID. `get_agent_handle` registered
                // them under the pool key returned by
                // `resolve_target` — usually the lead session id, but
                // a placeholder (`__fresh__:<project_key>`) for a
                // project with no on-disk sessions, or the previous
                // session's id on a `/new` flow. Without this, the
                // TUI's `active_session_key` flips to `real_key` after
                // `Connected` and every subsequent `Command::Prompt`
                // falls off `dispatch`'s key lookup with `UnknownSession`.
                if self.connected_once {
                    // Drop oneshots from the previous identity so parked
                    // forwarder tasks exit instead of waiting on
                    // tool_call_ids the new session will never produce.
                    self.domain.lock().pending_interactions.clear();
                    // Expire any inflight peer asks targeting this
                    // session's project: the OLD session UUID is gone
                    // (the user just `/clear`-ed, `/new`-ed, logged
                    // out, etc.), the NEW session has no knowledge of
                    // any q-id that was pending against the previous
                    // identity, so no reply will ever arrive. Mirrors
                    // the drop-hook behavior in `impl Drop for
                    // SessionTask` below — same `TargetConnectionFailed`
                    // reason because semantically the original target
                    // is unreachable. Fired BEFORE `rekey_to` so the
                    // project lookup inside `expire_target_inflight`
                    // still resolves against the about-to-be-replaced
                    // key.
                    if let Some(workspace) = self.workspace.upgrade() {
                        workspace.expire_target_inflight(
                            &self.key,
                            crate::mcp::peers::types::PeerFailureReason::TargetConnectionFailed,
                        );
                    }
                    self.rekey_to(&real_key);
                    self.emit(SessionUpdate::SessionReplaced {
                        key: real_key,
                        session_id: SessionId::new(session_id),
                        cwd,
                        current_model,
                        available_models,
                        mode,
                        history,
                    });
                } else {
                    self.rekey_to(&real_key);
                    self.connected_once = true;
                    // First Connected: emit KeyRenamed { from:
                    // spawn_key, to: real_key } so the TUI migrates
                    // its synthetic spawn bucket onto the real
                    // session UUID atomically.
                    if let Some(spawn_key) = self.spawn_key.take()
                        && spawn_key.as_str() != real_key.as_str()
                    {
                        self.emit(SessionUpdate::KeyRenamed {
                            from: spawn_key,
                            to: real_key.clone(),
                        });
                    }
                    self.emit(SessionUpdate::Connected {
                        key: real_key,
                        session_id: SessionId::new(session_id),
                        cwd,
                        current_model,
                        available_models,
                        mode,
                        history,
                    });
                    // Drain any peer prompts buffered while this
                    // session was pre-Connected (pushed by
                    // spawn::handle_deliver_peer_prompt when a peer
                    // ask hit a sleeping target). Each gets
                    // re-dispatched as a regular Command::Prompt so
                    // the existing prompt-delivery path handles it
                    // uniformly with user-typed prompts.
                    self.drain_pending_peer_prompts();
                }
            }
            AgentEvent::AuthRequired { method_name, method_description } => {
                let key = self.spawn_key.clone().unwrap_or_else(|| self.key.clone());
                self.emit(SessionUpdate::AuthRequired { key, method_name, method_description });
            }
            AgentEvent::ConnectionFailed { message } => {
                let key = self.spawn_key.clone().unwrap_or_else(|| self.key.clone());
                // Expire any inflight peer asks targeting THIS session
                // before emitting the user-visible ConnectionFailed.
                // Each ask gets the dual-path failure notification to
                // its caller (PeerAskFailed UI state + Command::Prompt
                // with DeliveryFailureNotice). No 30-min wait when
                // we know the target is gone.
                if let Some(workspace) = self.workspace.upgrade() {
                    workspace.expire_target_inflight(
                        &key,
                        crate::mcp::peers::types::PeerFailureReason::TargetConnectionFailed,
                    );
                }
                self.emit(SessionUpdate::ConnectionFailed { key, message, fatal: false });
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
                } else {
                    // TUI channel closed between the insert and the
                    // send. Resolve the orphaned oneshot with Cancelled
                    // so the SDK callback unblocks rather than hanging
                    // the `claude` subprocess turn forever.
                    if let Some(slot) =
                        self.domain.lock().pending_interactions.remove(&tool_call_id)
                        && let PendingInteractionSlot::Permission(tx) = slot
                    {
                        let _ = tx.send(forge_primitives::PermissionOutcome::Cancelled);
                    }
                    tracing::warn!(
                        target: "forge_workspace::session_task",
                        key = %self.key.as_str(),
                        tool_id = %tool_call_id,
                        "PermissionRequest send failed; orphaned oneshot cancelled"
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
                } else {
                    // TUI channel closed between insert and send —
                    // resolve the orphan with Cancelled so the SDK
                    // callback unblocks.
                    if let Some(slot) =
                        self.domain.lock().pending_interactions.remove(&tool_call_id)
                        && let PendingInteractionSlot::Question(tx) = slot
                    {
                        let _ = tx.send(forge_primitives::QuestionOutcome::Cancelled);
                    }
                    tracing::warn!(
                        target: "forge_workspace::session_task",
                        key = %self.key.as_str(),
                        tool_id = %tool_call_id,
                        "QuestionRequest send failed; orphaned oneshot cancelled"
                    );
                }
            }
            AgentEvent::McpOperationError { session_id, error } => {
                let session_key = SessionKey::from_session_id(session_id);
                self.emit(SessionUpdate::McpOperationError { key: session_key, error });
            }
            AgentEvent::SlashError { session_id, message } => {
                let session_key = SessionKey::from_session_id(session_id);
                self.emit(SessionUpdate::SlashCommandError { key: session_key, message });
            }
            AgentEvent::RuntimeReloadCompleted { session_id } => {
                self.emit(SessionUpdate::RuntimeReloadCompleted { session_id });
            }
            AgentEvent::RuntimeReloadFailed { session_id, message } => {
                self.emit(SessionUpdate::RuntimeReloadFailed { session_id, message });
            }
            AgentEvent::SessionsListed { sessions } => {
                // Route via `spawn_key` while the pre-Connect bucket
                // is still in place — same pattern as `AuthRequired`
                // and `ForgeAccountIdentity`. After the first Connected
                // the synth_key migrates to the real session UUID
                // (via `KeyRenamed`); subsequent SessionsListed events
                // land via `self.key` directly because `spawn_key` is
                // cleared in the Connected arm.
                let key = self.spawn_key.clone().unwrap_or_else(|| self.key.clone());
                self.emit(SessionUpdate::SessionsListed { key, sessions });
            }
            AgentEvent::StatusSnapshot { session_id, account, forge_account } => {
                self.emit(SessionUpdate::StatusSnapshot { session_id, account, forge_account });
            }
            AgentEvent::OauthCredentialsSnapshot { session_id, credentials } => {
                self.emit(SessionUpdate::OauthCredentialsSnapshot { session_id, credentials });
            }
            AgentEvent::ContextUsage { session_id, percentage, max_tokens } => {
                self.emit(SessionUpdate::ContextUsageSnapshot {
                    session_id,
                    percentage,
                    max_tokens,
                });
            }
            AgentEvent::McpSnapshot { session_id, servers, error } => {
                self.emit(SessionUpdate::McpSnapshot { session_id, servers, error });
            }
            AgentEvent::SdkMessage { session_id, msg } => {
                // Clear current_inbound_hop on turn boundary so the
                // next user-initiated turn starts the outgoing peer
                // chain at hop=1 instead of inheriting a stale
                // forwarded-peer hop from the prior turn.
                // `Message::Result` is the SDK's signal that the
                // assistant turn has fully completed.
                if matches!(msg, forge_primitives::Message::Result { .. }) {
                    self.domain.lock().current_inbound_hop = None;
                }
                self.emit(SessionUpdate::ChatAppended { session_id, msg });
            }
            AgentEvent::HookObservation {
                session_id,
                tool_use_id,
                permission_mode,
                effort,
                agent_id,
                agent_type,
            } => {
                self.emit(SessionUpdate::HookObservation {
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
                // Peek the slot kind first; only remove on a kind
                // match so a mismatched response leaves the real
                // waiter intact.
                let mut guard = self.domain.lock();
                let kind_matches = matches!(
                    guard.pending_interactions.get(&tool_id),
                    Some(PendingInteractionSlot::Permission(_)),
                );
                if kind_matches
                    && let Some(PendingInteractionSlot::Permission(tx)) =
                        guard.pending_interactions.remove(&tool_id)
                {
                    if tx.send(outcome).is_err() {
                        tracing::warn!(
                            target: "forge_workspace::session_task",
                            key = %self.key.as_str(),
                            tool_id = %tool_id,
                            "permission oneshot receiver dropped before response could be sent"
                        );
                    }
                } else if let Some(other) = guard.pending_interactions.get(&tool_id) {
                    tracing::warn!(
                        target: "forge_workspace::session_task",
                        key = %self.key.as_str(),
                        tool_id = %tool_id,
                        slot = ?other,
                        "RespondPermission expected Permission slot; got different kind. Outcome dropped, slot preserved."
                    );
                } else {
                    tracing::warn!(
                        target: "forge_workspace::session_task",
                        key = %self.key.as_str(),
                        tool_id = %tool_id,
                        "RespondPermission found no pending interaction (already responded or expired)"
                    );
                }
            }
            Command::RespondQuestion { key: _, tool_id, outcome } => {
                // Peek-before-remove — mirror of RespondPermission.
                let mut guard = self.domain.lock();
                let kind_matches = matches!(
                    guard.pending_interactions.get(&tool_id),
                    Some(PendingInteractionSlot::Question(_)),
                );
                if kind_matches
                    && let Some(PendingInteractionSlot::Question(tx)) =
                        guard.pending_interactions.remove(&tool_id)
                {
                    if tx.send(outcome).is_err() {
                        tracing::warn!(
                            target: "forge_workspace::session_task",
                            key = %self.key.as_str(),
                            tool_id = %tool_id,
                            "question oneshot receiver dropped"
                        );
                    }
                } else if let Some(other) = guard.pending_interactions.get(&tool_id) {
                    tracing::warn!(
                        target: "forge_workspace::session_task",
                        key = %self.key.as_str(),
                        tool_id = %tool_id,
                        slot = ?other,
                        "RespondQuestion got non-Question slot. Outcome dropped, slot preserved."
                    );
                } else {
                    tracing::warn!(
                        target: "forge_workspace::session_task",
                        key = %self.key.as_str(),
                        tool_id = %tool_id,
                        "RespondQuestion found no pending interaction"
                    );
                }
            }
            other => {
                let sid = self.session_id_string();
                if let Err(err) =
                    execute_command_via_handle(&self.handle, &self.key, sid.as_deref(), other)
                {
                    tracing::warn!(
                        target: "forge_workspace::session_task",
                        key = %self.key.as_str(),
                        error = %err,
                        "agent command dispatch failed; bridge channel closed?"
                    );
                }
            }
        }
    }

    /// Snapshot the session id stamped on `DomainSession`, formatted
    /// as a `String`. Most agent commands need the wire session_id;
    /// returns `None` when the bridge hasn't emitted its first
    /// `Connected` yet.
    fn session_id_string(&self) -> Option<String> {
        self.domain.lock().session_id.as_ref().map(std::string::ToString::to_string)
    }

    /// Send `update` to the workspace fan-in; log on send failure so a
    /// closed-channel regression leaves a trail rather than silently
    /// dropping events.
    fn emit(&self, update: SessionUpdate) {
        if self.update_tx.send(update).is_err() {
            tracing::warn!(
                target: "forge_workspace::session_task",
                key = %self.key.as_str(),
                "SessionUpdate channel closed; dropping event"
            );
        }
    }

    /// Migrate this task's workspace-side registrations from the
    /// current `self.key` to `real_key` (and update `self.key`).
    /// No-op when `self.key == real_key` or the workspace has been
    /// dropped. Delegates to
    /// [`crate::Workspace::migrate_session_task`] for the actual map
    /// shuffling so the lock-ordering rules live in one place.
    fn rekey_to(&mut self, real_key: &SessionKey) {
        if self.key.as_str() == real_key.as_str() {
            return;
        }
        let migrated = self
            .workspace
            .upgrade()
            .is_some_and(|workspace| workspace.migrate_session_task(&self.key, real_key));
        if !migrated {
            // Workspace refused the migration (collision or no workspace
            // upgrade). Keep `self.key` at the old slot so events
            // continue flowing through this task's existing channel.
            // Command dispatch against `real_key` will miss until the
            // TUI reconnects — single-user scope makes the collision
            // effectively impossible.
            return;
        }
        tracing::info!(
            target: "forge_workspace::session_task",
            from = %self.key.as_str(),
            to = %real_key.as_str(),
            "session task rekeyed onto real session UUID"
        );
        self.key = real_key.clone();
    }

    /// Drain `DomainSession.pending_peer_prompts` after the session's
    /// first `Connected` event. Each buffered peer prompt is
    /// re-dispatched as a normal `Command::Prompt` against
    /// `self.key` (now the real claude session UUID after rekey).
    /// The existing prompt-delivery path handles it identically to a
    /// user-typed prompt — the only difference is the prose body
    /// carries the `[Question id=q-…]` / `[Message id=t-…]` wrapper
    /// that the chat renderer pattern-matches into a styled peer
    /// block (lands in C16).
    ///
    /// Called once per session from the first-Connected arm of
    /// [`Self::translate_event`]. No-op when there are no buffered
    /// prompts.
    fn drain_pending_peer_prompts(&self) {
        let pending: Vec<crate::mcp::peers::types::WrappedPrompt> =
            std::mem::take(&mut self.domain.lock().pending_peer_prompts);
        if pending.is_empty() {
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else { return };
        // Same sidebar-badge bookkeeping the running-target branch of
        // `spawn::handle_deliver_peer_prompt` does: Question wrappers
        // bump the recipient's incoming counter so the sidebar `·N↓`
        // reflects the just-arrived ask. The wrappers we drain here
        // were buffered when the target was sleeping, so the bump
        // was deferred until now. Tells / Replies / notices don't
        // bump — same rule as the running-target path.
        let facade = crate::mcp::peers::facade::ProdWorkspaceFacade::from_arc(&workspace);
        for wrapped in pending {
            if matches!(wrapped.kind, crate::mcp::peers::types::WrappedKind::Question) {
                facade.bump_inflight_stats(
                    &self.key,
                    crate::mcp::peers::facade::PeerStatsDelta::IncomingPlus1,
                );
            }
            // Same typed peer-envelope echo the running-target
            // dispatch path does. Fire BEFORE the LLM-side dispatch
            // so the user-turn ordering is natural.
            crate::spawn::push_peer_user_turn_into_chat(&workspace, &self.key, &wrapped);
            let text = wrapped.to_prose();
            if let Err(err) = workspace.dispatch(crate::protocol::Command::Prompt {
                key: self.key.clone(),
                text,
                attachments: Vec::new(),
            }) {
                tracing::warn!(
                    target: "forge_workspace::session_task",
                    key = %self.key.as_str(),
                    error = ?err,
                    "drain_pending_peer_prompts: dispatch failed; prompt dropped"
                );
            }
        }
    }
}

/// Drop hook: on SessionTask exit (any reason — graceful close,
/// crash, panic), expire every in-flight peer ask targeting this
/// session. The expiration fires PeerAskFailed + a synthetic
/// DeliveryFailureNotice prompt to each caller so they aren't left
/// waiting on a session that no longer exists.
///
/// Uses the stored Weak<Workspace> reference so a Workspace drop
/// before the task drops doesn't double-fire or panic.
impl Drop for SessionTask {
    fn drop(&mut self) {
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.expire_target_inflight(
                &self.key,
                crate::mcp::peers::types::PeerFailureReason::TargetConnectionFailed,
            );
        }
    }
}

/// Forward a `Command` straight to `handle`. Pure transport — no
/// pending-interaction bookkeeping; the only commands that consult
/// `PendingInteractionSlot` (RespondPermission / RespondQuestion)
/// must be handled by the caller before delegation here.
///
/// Used by both `SessionTask::execute_command` (production: actor
/// path) and `Workspace::dispatch`'s synchronous test fallback when
/// no `SessionTask` is running for `key`.
///
/// Returns `Ok(())` on successful enqueue; `Err(...)` only on
/// AgentHandle send failure (dispatcher channel closed). Commands
/// requiring a `session_id` that the domain doesn't have yet
/// (pre-Connect) log a warning and return `Ok(())`.
pub(crate) fn execute_command_via_handle(
    handle: &Arc<AgentHandle>,
    key: &SessionKey,
    session_id: Option<&str>,
    cmd: Command,
) -> Result<(), forge_agent::AgentError> {
    match cmd {
        Command::Prompt { key: _, text, attachments } => {
            let Some(sid) = session_id else {
                warn_no_session(key, "Prompt");
                return Ok(());
            };
            handle.prompt_with_images(sid.to_owned(), text, attachments)
        }
        Command::Cancel { key: _ } => {
            let Some(sid) = session_id else {
                warn_no_session(key, "Cancel");
                return Ok(());
            };
            handle.cancel(sid.to_owned())
        }
        Command::SetMode { key: _, mode } => {
            let Some(sid) = session_id else {
                warn_no_session(key, "SetMode");
                return Ok(());
            };
            handle.set_mode(sid.to_owned(), mode.as_wire().to_owned())
        }
        Command::SetModel { key: _, model } => {
            let Some(sid) = session_id else {
                warn_no_session(key, "SetModel");
                return Ok(());
            };
            handle.set_model(sid.to_owned(), model)
        }
        Command::NewSession { key: _, cwd, launch_settings } => {
            handle.new_session(cwd, launch_settings)
        }
        Command::ResumeSession { key: _, session_id, cwd, launch_settings } => {
            handle.resume_session(session_id, cwd, launch_settings)
        }
        Command::ReconnectMcpServer { key: _, server_name } => {
            let Some(sid) = session_id else {
                warn_no_session(key, "ReconnectMcpServer");
                return Ok(());
            };
            handle.reconnect_mcp_server(sid.to_owned(), server_name)
        }
        Command::ToggleMcpServer { key: _, server_name, enabled } => {
            let Some(sid) = session_id else {
                warn_no_session(key, "ToggleMcpServer");
                return Ok(());
            };
            handle.toggle_mcp_server(sid.to_owned(), server_name, enabled)
        }
        Command::RespondPermission { .. } | Command::RespondQuestion { .. } => {
            tracing::error!(
                target: "forge_workspace::session_task",
                key = %key.as_str(),
                command = ?cmd,
                "Respond* commands must be handled by SessionTask::execute_command before delegation"
            );
            Ok(())
        }
        // App-level commands are caught in Workspace::dispatch's
        // app-level branch; they never reach this helper.
        misrouted @ (Command::SpawnProject { .. }
        | Command::SpawnSession { .. }
        | Command::StartDefault { .. }
        | Command::DeliverPeerPrompt { .. }
        | Command::SpawnWorker { .. }
        | Command::CloseWorker { .. }
        | Command::DeliverWorkerPrompt { .. }) => {
            tracing::warn!(
                target: "forge_workspace::session_task",
                key = %key.as_str(),
                command = ?misrouted,
                "App-level command unexpectedly routed via per-session path"
            );
            Ok(())
        }
    }
}

fn warn_no_session(key: &SessionKey, command: &'static str) {
    tracing::warn!(
        target: "forge_workspace::session_task",
        key = %key.as_str(),
        command,
        "command dropped: no session_id stamped on DomainSession yet",
    );
}

/// Apply an [`AgentEvent`] to a [`DomainSession`]. Pure mutation; no
/// I/O, no async, no sends. Called from inside
/// [`SessionTask::translate_event`] under the domain's lock.
///
/// Workspace only owns the `session_id` mirror used for `AgentHandle`
/// dispatch — operational state (lifecycle, cwd, turn state,
/// account info) lives on the TUI's `UiSession`, populated via the
/// `SessionUpdate` envelopes the task emits.
pub(crate) fn apply_event_to_domain(domain: &mut DomainSession, event: &AgentEvent) {
    // Always overwrite so /new / /login / /logout don't leave the
    // mirror stale on the second-Connected emission.
    if let AgentEvent::Connected { session_id, .. } = event {
        domain.session_id = Some(SessionId::new(session_id.clone()));
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
    let span = tracing::info_span!(
        "permission_response_forwarder",
        session_id = %session_id,
        tool_call_id = %tool_call_id,
    );
    tokio::task::spawn(
        async move {
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
                forge_primitives::PermissionOutcome::Selected { option_id, .. } => {
                    option_id.clone()
                }
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
        }
        .instrument(span),
    );
}

/// Forward an awaited question outcome to the agent so the bridge
/// can complete the round-trip with the CLI subprocess.
fn spawn_question_response_forwarder(
    agent: Arc<AgentHandle>,
    response_rx: tokio::sync::oneshot::Receiver<forge_primitives::QuestionOutcome>,
    session_id: String,
    tool_call_id: String,
) {
    let span = tracing::info_span!(
        "question_response_forwarder",
        session_id = %session_id,
        tool_call_id = %tool_call_id,
    );
    tokio::task::spawn(
        async move {
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
        }
        .instrument(span),
    );
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use forge_agent::Agent;

    fn empty_domain() -> DomainSession {
        let (handle, _rx) = Agent::testing_stub();
        DomainSession::new(SessionKey::from_str_for_test("test"), Some(Arc::new(handle)))
    }

    /// `apply_event_to_domain` on `AgentEvent::Connected` stamps (or
    /// overwrites) `session_id` so subsequent `AgentHandle` calls
    /// route to the live identity. See
    /// `translate_second_connected_overwrites_session_id` for the
    /// `/new`-flow overwrite case.
    #[test]
    fn translate_connected_stamps_session_id() {
        let mut domain = empty_domain();
        assert!(domain.session_id.is_none());

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
    }

    /// A second `Connected` (`/new`, `/login`, `/logout`) overwrites
    /// the session_id mirror so subsequent user commands route to the
    /// new identity.
    #[test]
    fn translate_second_connected_overwrites_session_id() {
        let mut domain = empty_domain();
        domain.session_id = Some(SessionId::new("old-uuid"));

        apply_event_to_domain(
            &mut domain,
            &AgentEvent::Connected {
                session_id: "new-uuid".to_owned(),
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
            Some("new-uuid".to_owned()),
            "second Connected must overwrite session_id mirror",
        );
    }

    /// `SessionTask::translate_event` on a second `Connected`
    /// (`connected_once = true`) drains `pending_interactions` so
    /// forwarder tasks parked on the previous identity's
    /// tool_call_ids exit instead of waiting forever.
    #[test]
    fn translate_second_connected_drains_pending_interactions() {
        use tokio::sync::oneshot;

        let (handle, _commands_rx) = Agent::testing_stub();
        let (_cmd_tx, command_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::protocol::Command>();
        let (update_tx, _update_rx) = tokio::sync::mpsc::unbounded_channel::<SessionUpdate>();
        let domain = Arc::new(parking_lot::Mutex::new(empty_domain()));
        let (response_tx, mut response_rx) =
            oneshot::channel::<forge_primitives::PermissionOutcome>();
        domain
            .lock()
            .pending_interactions
            .insert("stale_tool_id".to_owned(), PendingInteractionSlot::Permission(response_tx));
        let mut task = SessionTask {
            key: SessionKey::from_str_for_test("old-uuid"),
            handle: Arc::new(handle),
            command_rx,
            domain: Arc::clone(&domain),
            update_tx,
            spawn_key: None,
            connected_once: true,
            workspace: std::sync::Weak::new(),
        };

        task.translate_event(AgentEvent::Connected {
            session_id: "new-uuid".to_owned(),
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
        });

        assert!(
            domain.lock().pending_interactions.is_empty(),
            "second Connected must clear stale pending_interactions",
        );
        assert!(
            matches!(
                response_rx.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Closed)
            ),
            "forwarder receiver must observe Closed after the sender was dropped"
        );
    }

    /// Common harness for the `execute_command_via_handle` tests
    /// below: build a fresh stub handle + drain channel, return both.
    fn stub_handle_with_rx()
    -> (Arc<AgentHandle>, tokio::sync::mpsc::UnboundedReceiver<forge_primitives::AgentCommand>)
    {
        let (handle, rx) = Agent::testing_stub();
        (Arc::new(handle), rx)
    }

    /// `Command::Prompt` reaches the underlying agent's command
    /// dispatcher as `PromptWithImages`.
    #[test]
    fn execute_prompt_forwards_to_handle() {
        let (handle, mut rx) = stub_handle_with_rx();
        let key = SessionKey::from_str_for_test("sess");
        execute_command_via_handle(
            &handle,
            &key,
            Some("sess-1"),
            Command::Prompt { key: key.clone(), text: "hi".into(), attachments: Vec::new() },
        )
        .expect("dispatch succeeds");
        let cmd = rx.try_recv().expect("command queued");
        assert!(matches!(
            cmd,
            forge_primitives::AgentCommand::PromptWithImages { session_id, .. }
                if session_id == "sess-1"
        ));
    }

    /// `Command::Cancel` reaches the agent's command dispatcher.
    #[test]
    fn execute_cancel_forwards_to_handle() {
        let (handle, mut rx) = stub_handle_with_rx();
        let key = SessionKey::from_str_for_test("sess");
        execute_command_via_handle(
            &handle,
            &key,
            Some("sess-1"),
            Command::Cancel { key: key.clone() },
        )
        .expect("dispatch succeeds");
        let cmd = rx.try_recv().expect("command queued");
        assert!(matches!(
            cmd,
            forge_primitives::AgentCommand::Cancel { session_id } if session_id == "sess-1"
        ));
    }

    /// `Command::SetMode` translates the typed `PermissionMode` back
    /// to its wire form on the way out to the bridge.
    #[test]
    fn execute_set_mode_uses_wire_form() {
        use forge_primitives::permission::PermissionMode;
        let (handle, mut rx) = stub_handle_with_rx();
        let key = SessionKey::from_str_for_test("sess");
        execute_command_via_handle(
            &handle,
            &key,
            Some("sess-1"),
            Command::SetMode { key: key.clone(), mode: PermissionMode::Plan },
        )
        .expect("dispatch succeeds");
        let cmd = rx.try_recv().expect("command queued");
        match cmd {
            forge_primitives::AgentCommand::SetMode { session_id, mode } => {
                assert_eq!(session_id.as_str(), "sess-1");
                assert_eq!(mode, "plan");
            }
            other => panic!("expected SetMode, got {other:?}"),
        }
    }

    /// `Command::ReconnectMcpServer` reaches the bridge with the
    /// server name carried through.
    #[test]
    fn execute_reconnect_mcp_server_forwards() {
        let (handle, mut rx) = stub_handle_with_rx();
        let key = SessionKey::from_str_for_test("sess");
        execute_command_via_handle(
            &handle,
            &key,
            Some("sess-1"),
            Command::ReconnectMcpServer { key: key.clone(), server_name: "fs".into() },
        )
        .expect("dispatch succeeds");
        let cmd = rx.try_recv().expect("command queued");
        match cmd {
            forge_primitives::AgentCommand::ReconnectMcpServer { server_name, .. } => {
                assert_eq!(server_name, "fs");
            }
            other => panic!("expected ReconnectMcpServer, got {other:?}"),
        }
    }

    /// `Command::Cancel` without a `session_id` (pre-Connect) logs
    /// and returns `Ok(())` rather than panicking.
    #[test]
    fn execute_command_without_session_id_is_dropped() {
        let (handle, mut rx) = stub_handle_with_rx();
        let key = SessionKey::from_str_for_test("sess");
        execute_command_via_handle(&handle, &key, None, Command::Cancel { key: key.clone() })
            .expect("dispatch returns Ok");
        // Nothing should have been queued.
        assert!(rx.try_recv().is_err());
    }
}
