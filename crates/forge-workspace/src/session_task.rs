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
            // Skip the catalog mirror for workers: they're tracked via
            // live_workers and their JSONL carries the forge:worker tag,
            // but a tag-less mirror here lets a just-connected worker win
            // resolve_lead_session's untagged-latest fallback during the
            // boot window before that tag lands. The WorkerEntry is keyed
            // by the synth/spawn key at this point (pre-rekey).
            let lookup_key = self.spawn_key.clone().unwrap_or_else(|| self.key.clone());
            if workspace.worker_lookup_for_session(&lookup_key).is_none() {
                workspace.record_connected_session(cwd, session_id, None);
            }
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
                // `resolve_target` - usually the lead session id, but
                // a placeholder (`__fresh__:<project_key>`) for a
                // project with no on-disk sessions, or the previous
                // session's id on a `/new` flow. Without this, the
                // TUI's `active_session_key` flips to `real_key` after
                // `Connected` and every subsequent `Command::Prompt`
                // falls off `dispatch`'s key lookup with `UnknownSession`.
                // Worker sessions get their tag JSONL row written by
                // `apply_worker_tag_or_rollback`, which spawns a
                // detached tokio task. The helper checks live_workers
                // for a matching entry; for leads + non-worker
                // sessions this is a no-op (early return on
                // `worker_lookup_for_session` returning None).
                //
                // Critical: the helper fires on BOTH first-Connected
                // and subsequent Connecteds (the /new / login /
                // logout flow that enters via `connected_once`).
                // Each Connected carries a fresh session_id and the
                // worker's tag must travel to the new JSONL -
                // without re-tagging on /new, the resume scan
                // (#157/#164) finds the orphaned pre-/new JSONL and
                // resumes that instead of the active post-/new
                // session. Captured before the if/else because both
                // branches consume the `cwd` field.
                //
                // The detached task does an initial retry loop on
                // `Io(NotFound)` (claude writes the JSONL lazily on
                // first turn, so an idle-spawned worker has no file
                // at Connected). If retries exhaust on NotFound, the
                // worker still transitions to Running with
                // `needs_tag = true`; the opportunistic retry in
                // `handle_deliver_worker_prompt` catches the tag
                // when the first turn arrives. Non-NotFound errors
                // (permission denied, disk full, etc.) DO roll back:
                // release session + emit Removed.
                //
                // TODO: lead sessions are currently never tagged
                // with `forge:lead`. The resolver falls back to
                // latest untagged so existing behaviour works, but
                // explicit lead tagging is a spec gap we should
                // close (apply the same retry pattern via a sibling
                // `apply_lead_tag_or_warn` that does NOT roll back
                // on failure - just warns).
                let cwd_for_tag = cwd.clone();
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
                    // SessionTask` below - same `TargetConnectionFailed`
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
                        key: real_key.clone(),
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
                    // Engineering team: when the lead's synth key
                    // names a project that carries a team config and
                    // no workers are live yet, programmatically
                    // dispatch one SpawnWorker per role. Idempotent
                    // via the live_workers gate - a reconnect after
                    // a transient failure skips re-spawn. See
                    // `crate::team` and `Workspace::spawn_team_for_lead`.
                    if let Some(spawn_key) = self.spawn_key.as_ref()
                        && let Some(workspace) = self.workspace.upgrade()
                    {
                        let force_new = self.domain.lock().spawned_force_new;
                        maybe_spawn_team_on_connected(
                            &workspace,
                            spawn_key,
                            real_key.as_str(),
                            force_new,
                        );
                        maybe_kick_worker_on_connected(&workspace, spawn_key, real_key.as_str());
                    }
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
                        key: real_key.clone(),
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
                    // Same for cron prompts buffered while the project was
                    // asleep (spawn::deliver_cron_prompt on a due cron
                    // whose session wasn't open). Plain user turns, so no
                    // peer-envelope echo.
                    self.drain_pending_cron_prompts();
                    // Same for Gotify notification envelopes buffered while
                    // the project was asleep.
                    self.drain_pending_gotify_prompts();
                }
                // Fire worker re-tag for both first-Connected and
                // post-/new Connected paths. #166: the previous
                // shape only called this in the else branch, so a
                // /new on a worker session wrote the new session's
                // JSONL but never tagged it - the resume scan then
                // picked the pre-/new orphan.
                if let Some(workspace) = self.workspace.upgrade() {
                    workspace.apply_worker_tag_or_rollback(&real_key, &cwd_for_tag);
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
                    // Worker async spawn failure: classify the
                    // failure, dispatch a typed
                    // WorkerSpawnFailedNotice envelope to the lead's
                    // chat if the classifier identifies a worktree-
                    // creation failure, and roll back the
                    // WorkerEntry regardless of classifier outcome
                    // (parity with the sync rollback in
                    // handle_spawn_worker). Lead-session and
                    // non-worker callers see no behavioural change
                    // - this branch is a no-op for them.
                    workspace.handle_async_worker_spawn_failure(&key, &message);
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
                    // TUI channel closed between insert and send -
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
                // is still in place - same pattern as `AuthRequired`
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
                // Peek-before-remove - mirror of RespondPermission.
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
            // TUI reconnects - single-user scope makes the collision
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
    /// user-typed prompt - the only difference is the prose body
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
        // bump - same rule as the running-target path.
        let facade = crate::mcp::peers::facade::ProdWorkspaceFacade::from_arc(&workspace);
        for wrapped in pending {
            if matches!(wrapped.kind, crate::mcp::peers::types::WrappedKind::Question) {
                facade.bump_inflight_stats(
                    &self.key,
                    crate::mcp::peers::facade::PeerStatsDelta::IncomingPlus1,
                );
                workspace.stamp_inflight_target(&wrapped.correlation_id, &self.key);
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

    /// Drain `DomainSession.pending_cron_prompts` after the session's
    /// first `Connected` event - the cron prompts buffered while the
    /// project was asleep (a due cron whose session wasn't open spawned
    /// it). Each is re-dispatched as a plain `Command::Prompt` against
    /// `self.key`, landing as an ordinary user turn (no peer envelope).
    /// No-op when there are no buffered prompts.
    fn drain_pending_cron_prompts(&self) {
        let pending: Vec<String> = std::mem::take(&mut self.domain.lock().pending_cron_prompts);
        if pending.is_empty() {
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else { return };
        for text in pending {
            if let Err(err) = workspace.dispatch(crate::protocol::Command::Prompt {
                key: self.key.clone(),
                text,
                attachments: Vec::new(),
            }) {
                tracing::warn!(
                    target: "forge_workspace::session_task",
                    key = %self.key.as_str(),
                    error = ?err,
                    "drain_pending_cron_prompts: dispatch failed; prompt dropped"
                );
            }
        }
    }

    /// Drain `DomainSession.pending_gotify_prompts` after the session's
    /// first `Connected` event - Gotify notification envelopes buffered
    /// while the project was asleep. Each is re-dispatched as a plain
    /// `Command::Prompt`, landing as an ordinary user turn. Mirrors
    /// [`Self::drain_pending_cron_prompts`]. No-op when the buffer is empty.
    fn drain_pending_gotify_prompts(&self) {
        let pending: Vec<String> = std::mem::take(&mut self.domain.lock().pending_gotify_prompts);
        if pending.is_empty() {
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else { return };
        for text in pending {
            if let Err(err) = workspace.dispatch(crate::protocol::Command::Prompt {
                key: self.key.clone(),
                text,
                attachments: Vec::new(),
            }) {
                tracing::warn!(
                    target: "forge_workspace::session_task",
                    key = %self.key.as_str(),
                    error = ?err,
                    "drain_pending_gotify_prompts: dispatch failed; prompt dropped"
                );
            }
        }
    }
}

/// Drop hook: on SessionTask exit (any reason - graceful close,
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

/// Parse a synthetic spawn key. Project lead spawn keys have shape
/// `__spawn_<project_name>__`; worker spawn keys have shape
/// `__spawn_worker_<project_key>_<label>_<uuid>__` (at least four
/// underscore-separated segments after the `worker_` prefix).
/// Returns the project name iff `key` is a lead spawn key, `None`
/// for workers or any other shape. A literal project name starting
/// with `worker_` only false-classifies when its segment count
/// happens to match the worker shape; the common short cases
/// (`worker_foo`, `worker_my_proj`) parse correctly as leads.
fn parse_project_lead_synth_key(key: &SessionKey) -> Option<String> {
    let s = key.as_str();
    let inner = s.strip_prefix("__spawn_")?.strip_suffix("__")?;
    if inner.starts_with("worker_") && inner.split('_').count() >= 4 {
        return None;
    }
    Some(inner.to_owned())
}

/// Shared engineering-team Connected hook: if `spawn_key` is a
/// project-lead synth key, the project has a team configured, and
/// no live workers exist for it yet, dispatch one `SpawnWorker`
/// per configured role. Called from `SessionTask::translate_event`
/// (production) and `on_connected_for_test` (tests). Idempotent;
/// safe to call multiple times for the same session - the
/// `live_workers.is_empty()` gate guards against double-spawn on
/// `/new` reconnects or transient retries.
fn maybe_spawn_team_on_connected(
    workspace: &Arc<crate::Workspace>,
    spawn_key: &SessionKey,
    real_session_id: &str,
    force_new: bool,
) {
    let Some(project_name) = parse_project_lead_synth_key(spawn_key) else {
        return;
    };
    let Some(project) = workspace.find_project_view_by_name(&project_name) else {
        return;
    };
    if project.team.is_empty() {
        return;
    }
    let project_key = crate::target::ProjectKey::new(
        forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
            &project.path.to_string_lossy(),
        )),
    );
    if !workspace.list_live_workers(&project_key).is_empty() {
        return;
    }
    // Scan the project's catalog for previously-spawned worker
    // sessions (tagged `forge:worker:<label>`) so each role resumes
    // its existing session instead of starting fresh. The scan is
    // async (filesystem I/O); workspace claims a per-project
    // in-flight guard synchronously so a fast double-Connected
    // can't slip a second team-spawn through. The guard is
    // released after the SpawnWorker commands are dispatched.
    workspace.spawn_team_for_lead_with_catalog_scan(
        real_session_id.to_owned(),
        project_key,
        project.path.clone(),
        project.name.clone(),
        project.team.clone(),
        force_new,
    );
}

/// Parse a worker synthetic spawn key into `(project_key, label)`.
/// Recognises both:
///
/// - `__spawn_worker_<project>_<label>_<uuid>__` (fresh-spawn shape,
///   `handle_spawn_worker` with `resume_existing = None`)
/// - `__resume_worker_<project>_<label>_<uuid>__` (resume shape,
///   `handle_spawn_worker` with `resume_existing = Some(...)`)
///
/// `None` for lead synth keys or any other shape. Project keys are
/// alphanumeric+dash only (no underscores) and uuids are hex (no
/// underscores), so `splitn(3, '_')` on the "<project>_<label>_<uuid>"
/// remainder yields exactly three parts. The project_key segment lets
/// the kick hooks recover the worker's namespace for project-first
/// role resolution.
fn parse_worker_synth_key(key: &SessionKey) -> Option<(String, String)> {
    let s = key.as_str();
    let inner =
        s.strip_prefix("__spawn_").or_else(|| s.strip_prefix("__resume_"))?.strip_suffix("__")?;
    let after_worker = inner.strip_prefix("worker_")?;
    let parts: Vec<&str> = after_worker.splitn(3, '_').collect();
    if parts.len() != 3 {
        return None;
    }
    Some((parts[0].to_owned(), parts[1].to_owned()))
}

/// True when `key` is the resume-shaped worker synth key
/// (`__resume_worker_<project>_<label>_<uuid>__`). The kick-skip
/// guard in `maybe_kick_worker_on_connected` keys on this to decide
/// whether to inspect the JSONL turn count before firing the
/// initial kick.
fn is_resume_worker_synth_key(key: &SessionKey) -> bool {
    key.as_str().starts_with("__resume_worker_")
}

/// Shared engineering-team worker-kick hook: if `spawn_key` is a
/// worker synth key AND its label matches a known role, dispatch a
/// `Command::Prompt` carrying that role's `initial_kick` text to the
/// freshly-Connected worker. Claude sessions don't act until a
/// user-turn arrives, so without this kick a team worker would sit
/// idle indefinitely after spawn (its charter would shape behaviour
/// IF prompted, but nothing prompts it).
///
/// Workers spawned with non-role labels (e.g. LLM-driven
/// `workers__spawn("scratchpad", ...)` outside the engineering-team
/// flow) get no kick - they're caller-driven by design.
///
/// Called from `SessionTask::translate_event` (production) and
/// `on_connected_for_test` (tests). The dispatch goes through the
/// Workspace command bus to the worker's own SessionTask queue,
/// which processes the prompt on its next loop iteration once
/// translate_event returns.
fn maybe_kick_worker_on_connected(
    workspace: &Arc<crate::Workspace>,
    spawn_key: &SessionKey,
    real_session_id: &str,
) {
    let Some((project_key, label)) = parse_worker_synth_key(spawn_key) else {
        return;
    };

    // Resolve the worker's project view once: its `ProjectKey` for the
    // live-workers lookup below, and its name (namespace) for the
    // file-driven role-kick resolution further down.
    let project_view =
        workspace.list_projects().into_iter().find(|v| v.key.as_str() == project_key);

    // Inline kick from `workers__spawn(kick=...)` takes precedence over
    // the file-driven `kick.md`: an ad-hoc spawn has no kick.md, so the
    // WorkerEntry-stashed kick is the only thing that gives it a first
    // turn. Delivered through the same rate-limited kick dispatcher the
    // file kicks use.
    if let Some(view) = project_view.as_ref() {
        let inline_kick = workspace
            .list_live_workers(&view.key)
            .into_iter()
            .rev()
            .find(|w| w.label == label)
            .and_then(|w| w.kick);
        if let Some(kick) = inline_kick {
            workspace.enqueue_kick(crate::workspace::KickRequest {
                session_key: SessionKey::from_session_id(real_session_id.to_owned()),
                prompt_body: kick,
            });
            return;
        }
    }

    let is_resume = is_resume_worker_synth_key(spawn_key);

    // Recover the worker's project namespace (the forge.toml name) so
    // the kick resolves project-first-then-global, matching the charter
    // the worker was spawned with. Fall back to the bare label when the
    // project is gone so global-role workers still kick.
    let resolved = project_view
        .map(|v| v.name)
        .and_then(|namespace| crate::team::roles::resolve_role(&label, &namespace))
        .unwrap_or_else(|| label.clone());

    // Resume path: prefer the resume-specific kick when the role
    // ships one. `<label>/resume-kick.md` is opt-in (absent file is
    // expected for most roles); when present, it represents the
    // explicit "you're picking up, re-orient" framing. Override the
    // past-progress guard so a re-orient lands even for workers that
    // had progressed past their initial kick - the whole point of a
    // resume-kick is to wake the worker up post-restart.
    let kick_text = if is_resume {
        match crate::team::load_resume_kick(&resolved) {
            Ok(Some(text)) => Some(text),
            Ok(None) => None,
            Err(err) => {
                tracing::warn!(
                    target: "forge_workspace::team",
                    label = %label,
                    error = %err,
                    "resume-kick lookup failed; falling back to initial-kick (or skip per past-progress guard)"
                );
                None
            }
        }
    } else {
        None
    };

    if let Some(resume_kick) = kick_text {
        // #259: kicks route through the workspace-level dispatcher so
        // multi-worker boots don't fire N simultaneous Prompts at
        // Anthropic's per-IP burst limit. The drainer fires one per
        // `KICK_DISPATCH_INTERVAL`; the first kick of an empty queue
        // has zero added latency.
        workspace.enqueue_kick(crate::workspace::KickRequest {
            session_key: SessionKey::from_session_id(real_session_id.to_owned()),
            prompt_body: resume_kick,
        });
        return;
    }

    // Fall-through: fresh spawn OR resume-without-resume-kick. Load
    // the regular `<label>/kick.md`.
    let initial_kick = match crate::team::load_initial_kick(&resolved) {
        Ok(kick) => kick,
        Err(err) => {
            tracing::warn!(
                target: "forge_workspace::team",
                label = %label,
                error = %err,
                "no initial-kick file found for worker label; worker spawn proceeds without a kick prompt (worker stays idle until lead dispatches). Populate ~/.claude/forge-team/<label>/kick.md (copy from docs/forge-team-defaults/<label>/) or use the workers__create_role MCP tool."
            );
            return;
        }
    };
    // Resume path WITHOUT a resume-kick.md: inspect the JSONL turn
    // count before re-firing the regular kick. The kick lands as a
    // USER turn; a worker that's already executed past the kick has
    // at least one MORE user turn in its history (the next prompt
    // from the lead, or an MCP-driven peer/worker message).
    // Threshold is 2: a JSONL with exactly 1 user turn means the
    // worker received the kick but crashed / didn't progress before
    // forge restarted, so we re-fire to actually start the work. 2+
    // means the worker has moved past the kick - leave it alone,
    // since a re-kick would override its in-flight state. Fresh-
    // spawn path skips this check (no JSONL exists yet;
    // user_turn_count would be 0 anyway).
    if is_resume && workspace.worker_has_progress_past_kick(real_session_id) {
        tracing::info!(
            target: "forge_workspace::team",
            label = %label,
            session_id = real_session_id,
            "skipping kick on worker resume with prior progress past initial kick (no resume-kick.md available)",
        );
        return;
    }
    // #259: same dispatcher route as the resume-kick branch above.
    workspace.enqueue_kick(crate::workspace::KickRequest {
        session_key: SessionKey::from_session_id(real_session_id.to_owned()),
        prompt_body: initial_kick,
    });
}

/// Test-only entry point for the Connected team hooks.
/// Drives both `maybe_spawn_team_on_connected` (lead path) and
/// `maybe_kick_worker_on_connected` (worker path) directly without
/// constructing a `SessionTask` or pumping through the actor - the
/// `team_hook_tests` module uses this to assert the trigger logic.
/// Only one hook fires per call: the spawn_key's shape selects.
#[cfg(any(test, feature = "testing"))]
pub fn on_connected_for_test(
    workspace: &Arc<crate::Workspace>,
    synth_key: &SessionKey,
    real_session_id: &str,
) {
    // Normal (non-`--new`) Connected simulation; the force-new cascade
    // is exercised directly against spawn_team_for_lead_with_catalog_scan.
    maybe_spawn_team_on_connected(workspace, synth_key, real_session_id, false);
    maybe_kick_worker_on_connected(workspace, synth_key, real_session_id);
}

/// Forward a `Command` straight to `handle`. Pure transport - no
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
            handle.set_mode(sid.to_owned(), mode)
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
        | Command::DespawnWorker { .. }
        | Command::DeliverWorkerPrompt { .. }
        | Command::DeliverWorkerPromptToLead { .. }
        | Command::DeliverGotifyMessage { .. }) => {
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
/// dispatch - operational state (lifecycle, cwd, turn state,
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

    /// First-Connected drains `DomainSession.pending_peer_prompts` in
    /// FIFO order, dispatching one `Command::Prompt` per buffered
    /// entry, then leaves the buffer empty. Pinned via the workspace's
    /// command-intercept buffer so the full first-Connected branch of
    /// `translate_event` runs end-to-end (no poking the private drain
    /// method directly).
    #[tokio::test]
    async fn first_connected_drains_pending_peer_prompts_in_fifo_order() {
        use crate::mcp::peers::types::{CorrelationId, WrappedKind, WrappedPrompt};

        let (workspace, _update_rx) = crate::Workspace::testing_stub();

        // Build a DomainSession at a fixed session-id key and seed
        // three Messages in known order. Message kind (not Question)
        // keeps the assertion focused on FIFO dispatch; the
        // Question-kind incoming-counter bump is exercised separately.
        let session_key = SessionKey::from_session_id("drain-session-uuid");
        let domain =
            Arc::new(parking_lot::Mutex::new(DomainSession::new(session_key.clone(), None)));
        let bodies = ["first", "second", "third"];
        {
            let mut d = domain.lock();
            for body in bodies {
                d.pending_peer_prompts.push(WrappedPrompt {
                    correlation_id: CorrelationId::new_tell(),
                    kind: WrappedKind::Message,
                    sender_name: "forge".to_owned(),
                    sender_org: "Default".to_owned(),
                    hop: 1,
                    hop_limit: 10,
                    body: body.to_owned(),
                });
            }
        }

        // spawn_key=None + connected_once=false → first-Connected arm
        // that calls drain_pending_peer_prompts. Matching self.key to
        // the event's session_id makes rekey_to a no-op so the test
        // doesn't have to register against the workspace pool.
        let (handle, _agent_cmd_rx) = Agent::testing_stub();
        let (_cmd_tx, command_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::protocol::Command>();
        let update_tx = workspace.update_sender();
        let mut task = SessionTask {
            key: session_key.clone(),
            handle: Arc::new(handle),
            command_rx,
            domain: Arc::clone(&domain),
            update_tx,
            spawn_key: None,
            connected_once: false,
            workspace: Arc::downgrade(&workspace),
        };

        workspace.enable_test_dispatch_intercept();
        task.translate_event(AgentEvent::Connected {
            session_id: session_key.as_str().to_owned(),
            cwd: "/tmp/drain".to_owned(),
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

        // Drain dispatches Command::Prompt for each buffered entry,
        // in insertion order. Filter out anything else in case
        // translate_event grows side-effects later.
        let buffered = workspace.drain_test_dispatch_buffer();
        let drained_bodies: Vec<String> = buffered
            .into_iter()
            .filter_map(|cmd| match cmd {
                crate::protocol::Command::Prompt { text, .. } => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(
            drained_bodies.len(),
            bodies.len(),
            "one Command::Prompt per buffered wrapped prompt"
        );
        for (i, expected_body) in bodies.iter().enumerate() {
            assert!(
                drained_bodies[i].contains(expected_body),
                "drain position {i}: expected body '{expected_body}' in dispatched text '{}'",
                drained_bodies[i],
            );
        }
        assert!(
            domain.lock().pending_peer_prompts.is_empty(),
            "pending_peer_prompts is drained after first-Connected"
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
                assert_eq!(mode, PermissionMode::Plan);
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

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod team_hook_tests {
    use super::*;
    use crate::Workspace;
    use crate::protocol::Command;
    use crate::target::ProjectKey;
    use crate::team::set_forge_team_root_for_test;
    use std::sync::OnceLock;

    fn synth_lead_key(project_name: &str) -> SessionKey {
        SessionKey::from_session_id(format!("__spawn_{project_name}__"))
    }

    /// One-time setup writing the canonical shipped default charters
    /// (implementer + lead) to a shared tempdir + redirecting
    /// `forge_team_root()` to it for the rest of the process. All
    /// tests in this module call `ensure_test_charter_root()` before
    /// exercising the team-spawn or kick paths.
    fn ensure_test_charter_root() {
        static ROOT: OnceLock<tempfile::TempDir> = OnceLock::new();
        let dir = ROOT.get_or_init(|| {
            let tmp = tempfile::tempdir().expect("tempdir");
            let root = tmp.path();
            // Seed using the shipped defaults under docs/forge-team-defaults/.
            // `include_str!` paths are relative to THIS source file.
            for (label, charter, kick) in [
                (
                    "implementer",
                    include_str!("../../../docs/forge-team-defaults/implementer/charter.md"),
                    include_str!("../../../docs/forge-team-defaults/implementer/kick.md"),
                ),
                (
                    "lead",
                    include_str!("../../../docs/forge-team-defaults/lead/charter.md"),
                    include_str!("../../../docs/forge-team-defaults/lead/kick.md"),
                ),
            ] {
                let dir = root.join(label);
                std::fs::create_dir_all(&dir).expect("create role dir");
                std::fs::write(dir.join("charter.md"), charter).expect("write charter");
                std::fs::write(dir.join("kick.md"), kick).expect("write kick");
            }
            set_forge_team_root_for_test(Some(root.to_owned()));
            tmp
        });
        // The first caller initialised the override; subsequent
        // callers just need to confirm the override is still set
        // (defensive against test ordering rewriting it).
        let _ = dir;
    }

    /// A lead Connected for a project carrying a team config
    /// triggers one `Command::SpawnWorker` per configured label.
    #[test]
    fn lead_connected_with_team_triggers_team_spawn() {
        ensure_test_charter_root();
        let (workspace, _update_rx) = Workspace::testing_stub();
        workspace.enable_test_dispatch_intercept();
        workspace.seed_test_project_with_team("proj-x", "/tmp/proj-x", &["implementer".to_owned()]);

        on_connected_for_test(&workspace, &synth_lead_key("proj-x"), "lead-uuid");

        let dispatched = workspace.drain_test_dispatch_buffer();
        let spawns: Vec<&Command> =
            dispatched.iter().filter(|c| matches!(c, Command::SpawnWorker { .. })).collect();
        assert_eq!(spawns.len(), 1, "one SpawnWorker per role");
    }

    /// A lead Connected for a project with NO team configuration is
    /// a no-op - nothing dispatched.
    #[test]
    fn lead_connected_without_team_does_nothing() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        workspace.enable_test_dispatch_intercept();
        workspace.seed_test_project_with_team("proj-y", "/tmp/proj-y", &[]);

        on_connected_for_test(&workspace, &synth_lead_key("proj-y"), "lead-uuid");

        assert!(workspace.drain_test_dispatch_buffer().is_empty());
    }

    /// A worker session's Connected (synth key prefixed with
    /// `__spawn_worker_...`) does NOT trigger the team-spawn hook
    /// even when the project has a team configured.
    #[test]
    fn worker_connected_does_not_trigger_team_spawn() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        workspace.enable_test_dispatch_intercept();
        workspace.seed_test_project_with_team("proj-z", "/tmp/proj-z", &["planner".to_owned()]);

        let worker_synth = SessionKey::from_session_id("__spawn_worker_proj-z_planner_abc__");
        on_connected_for_test(&workspace, &worker_synth, "worker-uuid");

        let dispatched = workspace.drain_test_dispatch_buffer();
        assert!(dispatched.iter().all(|c| !matches!(c, Command::SpawnWorker { .. })));
    }

    /// Idempotency: a second Connected event for the same lead must
    /// not double-spawn. The first call inserts WorkerEntries into
    /// `live_workers`; the second call's `list_live_workers(...).is_empty()`
    /// gate trips and the trigger no-ops.
    #[test]
    fn second_lead_connected_does_not_double_spawn() {
        ensure_test_charter_root();
        let (workspace, _update_rx) = Workspace::testing_stub();
        workspace.enable_test_dispatch_intercept();
        workspace.seed_test_project_with_team("proj-x", "/tmp/proj-x", &["implementer".to_owned()]);

        let lead_synth = synth_lead_key("proj-x");

        // First Connected: triggers team spawn (1 SpawnWorker).
        on_connected_for_test(&workspace, &lead_synth, "lead-uuid");
        let after_first = workspace.drain_test_dispatch_buffer();
        let first_spawns: usize =
            after_first.iter().filter(|c| matches!(c, Command::SpawnWorker { .. })).count();
        assert_eq!(first_spawns, 1, "first Connected spawns the team");

        // Simulate the workers having become live (the production
        // flow does this via `handle_spawn_worker`'s
        // `insert_live_worker`; the test intercept skipped that
        // path so we seed it manually for the idempotency gate).
        let project_key = ProjectKey::new(
            forge_agent::userdata::catalog::scan::project_key_for_directory(Some("/tmp/proj-x")),
        );
        workspace.insert_live_worker(
            &project_key,
            crate::mcp::workers::types::WorkerEntry {
                label: "implementer".into(),
                charter: "test".into(),
                session_key: SessionKey::from_session_id("worker-uuid"),
                status: forge_primitives::WorkerLiveness::Running,
                spawned_at: std::time::SystemTime::UNIX_EPOCH,
                spawned_by_session_id: "lead-uuid".into(),
                needs_tag: false,
                is_git_repo_at_spawn: false,
                diagnostic: None,
                kick: None,
            },
        );

        // Second Connected: gate trips, no new SpawnWorker.
        on_connected_for_test(&workspace, &lead_synth, "lead-uuid");
        let after_second = workspace.drain_test_dispatch_buffer();
        let second_spawns: usize =
            after_second.iter().filter(|c| matches!(c, Command::SpawnWorker { .. })).count();
        assert_eq!(second_spawns, 0, "second Connected must not double-spawn");
    }

    /// Worker Connected with a role-matching label enqueues a kick
    /// onto the workspace dispatcher (#259) which fans out as a
    /// `Command::Prompt` carrying the role's initial-kick text to
    /// the worker's real session_id. End-to-end: enqueue happens in
    /// `maybe_kick_worker_on_connected`; the drainer task (started
    /// here via `start_kick_dispatcher`) reads the channel and calls
    /// `Workspace::dispatch` which lands in the intercept buffer.
    /// Paused time + a yield-loop are the deterministic way to drive
    /// the drainer one step.
    #[tokio::test(start_paused = true)]
    async fn worker_connected_for_role_label_dispatches_kick_prompt() {
        ensure_test_charter_root();
        let (workspace, _update_rx) = Workspace::testing_stub();
        workspace.enable_test_dispatch_intercept();
        workspace.start_kick_dispatcher();

        let worker_synth = SessionKey::from_session_id("__spawn_worker_forge_implementer_abc123__");
        on_connected_for_test(&workspace, &worker_synth, "worker-uuid");

        // Drainer pulls the just-enqueued kick on the next runtime
        // yield; the first kick of an empty channel has zero added
        // latency by design (the sleep happens AFTER the dispatch).
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        let dispatched = workspace.drain_test_dispatch_buffer();
        let prompts: Vec<&Command> =
            dispatched.iter().filter(|c| matches!(c, Command::Prompt { .. })).collect();
        assert_eq!(prompts.len(), 1, "implementer worker gets exactly one kick");
        if let Command::Prompt { key, text, .. } = prompts[0] {
            assert_eq!(key.as_str(), "worker-uuid", "kick targets the worker's real session id");
            assert!(
                text.contains("You are now active"),
                "kick opens with activation framing; got: {text}",
            );
            assert!(
                !text.contains("gh issue list"),
                "generic kick must not self-poll GitHub issues; got: {text}",
            );
            assert!(
                text.contains("awaiting a plan"),
                "kick tells the implementer to await a lead-driven plan; got: {text}",
            );
        }
    }

    /// A project-scoped worker (bare label `steward` in `data-modules`)
    /// resolves its kick project-first to `data-modules/steward/kick.md`,
    /// even though no global `steward` exists.
    #[tokio::test]
    async fn worker_kick_resolves_project_scoped_role() {
        let tmp = tempfile::tempdir().expect("tmp");
        let steward = tmp.path().join("data-modules").join("steward");
        std::fs::create_dir_all(&steward).expect("mkdir");
        std::fs::write(steward.join("charter.md"), "description: Hub steward\n").expect("charter");
        std::fs::write(steward.join("kick.md"), "STEWARD-KICK: tend the modules\n").expect("kick");
        let prev = crate::team::set_forge_team_root_for_test(Some(tmp.path().to_path_buf()));

        let (workspace, _update_rx) = Workspace::testing_stub();
        workspace.enable_test_dispatch_intercept();
        workspace.start_kick_dispatcher();
        workspace.seed_test_project_with_team(
            "data-modules",
            "/tmp/data-modules",
            &["steward".to_owned()],
        );
        // Build the worker synth key from the seeded project's resolved
        // key so the kick hook recovers `data-modules` as the namespace.
        let project_key = workspace
            .list_projects()
            .into_iter()
            .find(|v| v.name == "data-modules")
            .expect("seeded project present")
            .key;
        let synth = SessionKey::from_session_id(format!(
            "__spawn_worker_{}_steward_deadbeef__",
            project_key.as_str()
        ));
        on_connected_for_test(&workspace, &synth, "worker-uuid");
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        let dispatched = workspace.drain_test_dispatch_buffer();
        let prompts: Vec<&Command> =
            dispatched.iter().filter(|c| matches!(c, Command::Prompt { .. })).collect();
        assert_eq!(prompts.len(), 1, "project-scoped worker gets exactly one kick");
        if let Command::Prompt { text, .. } = prompts[0] {
            assert!(
                text.contains("STEWARD-KICK"),
                "kick resolved from data-modules/steward/kick.md; got: {text}",
            );
        }

        crate::team::set_forge_team_root_for_test(prev);
    }

    /// Helper: insert a live ad-hoc worker carrying `kick` under the
    /// seeded project, returning its synth key for `on_connected_for_test`.
    #[cfg(test)]
    fn seed_adhoc_worker_with_kick(
        workspace: &Arc<Workspace>,
        label: &str,
        kick: Option<String>,
    ) -> SessionKey {
        workspace.seed_test_project_with_team("forge", "/tmp/forge", &[]);
        let project_key = workspace
            .list_projects()
            .into_iter()
            .find(|v| v.name == "forge")
            .expect("seeded project present")
            .key;
        workspace.insert_live_worker(
            &project_key,
            crate::mcp::workers::types::WorkerEntry {
                label: label.to_owned(),
                charter: "ad-hoc".into(),
                session_key: SessionKey::from_session_id("worker-uuid"),
                status: forge_primitives::WorkerLiveness::Running,
                spawned_at: std::time::SystemTime::UNIX_EPOCH,
                spawned_by_session_id: "lead".into(),
                needs_tag: false,
                is_git_repo_at_spawn: false,
                diagnostic: None,
                kick,
            },
        );
        SessionKey::from_session_id(format!(
            "__spawn_worker_{}_{label}_abc__",
            project_key.as_str()
        ))
    }

    /// An inline `workers__spawn(kick=...)` worker (non-role label, no
    /// kick.md) gets its kick delivered as the first turn, verbatim,
    /// through the same dispatcher the file kicks use.
    #[tokio::test(start_paused = true)]
    async fn worker_with_inline_kick_dispatches_it_as_first_turn() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        workspace.enable_test_dispatch_intercept();
        workspace.start_kick_dispatcher();
        let synth = seed_adhoc_worker_with_kick(
            &workspace,
            "scratch",
            Some("Begin: triage the failing test now.".into()),
        );

        on_connected_for_test(&workspace, &synth, "worker-uuid");
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        let dispatched = workspace.drain_test_dispatch_buffer();
        let prompts: Vec<&Command> =
            dispatched.iter().filter(|c| matches!(c, Command::Prompt { .. })).collect();
        assert_eq!(prompts.len(), 1, "inline-kick worker gets exactly one kick");
        if let Command::Prompt { key, text, .. } = prompts[0] {
            assert_eq!(key.as_str(), "worker-uuid", "kick targets the worker's real session id");
            assert_eq!(
                text, "Begin: triage the failing test now.",
                "inline kick delivered verbatim",
            );
        }
    }

    /// An ad-hoc worker with NO inline kick and no kick.md gets no
    /// kick - it idles until the lead sends a workers__tell (today's
    /// behavior, unchanged).
    #[tokio::test(start_paused = true)]
    async fn worker_without_inline_kick_for_adhoc_label_does_not_kick() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        workspace.enable_test_dispatch_intercept();
        workspace.start_kick_dispatcher();
        let synth = seed_adhoc_worker_with_kick(&workspace, "scratch", None);

        on_connected_for_test(&workspace, &synth, "worker-uuid");
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        let dispatched = workspace.drain_test_dispatch_buffer();
        let prompts: Vec<&Command> =
            dispatched.iter().filter(|c| matches!(c, Command::Prompt { .. })).collect();
        assert!(prompts.is_empty(), "no kick without an inline kick or a kick.md");
    }

    /// Worker Connected with a label NOT matching a built-in role
    /// (e.g. an LLM-driven `workers__spawn("scratchpad", ...)` for
    /// ad-hoc work) does not get a kick - only engineering-team
    /// roles do.
    #[test]
    fn worker_connected_for_non_role_label_does_not_kick() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        workspace.enable_test_dispatch_intercept();

        let worker_synth = SessionKey::from_session_id("__spawn_worker_forge_scratchpad_abc123__");
        on_connected_for_test(&workspace, &worker_synth, "worker-uuid");

        let dispatched = workspace.drain_test_dispatch_buffer();
        assert!(
            dispatched.iter().all(|c| !matches!(c, Command::Prompt { .. })),
            "non-role labels should not trigger a kick",
        );
    }

    /// Lead Connected does not dispatch any kick prompt - kicks are
    /// for workers only.
    #[test]
    fn lead_connected_does_not_dispatch_kick_prompt() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        workspace.enable_test_dispatch_intercept();
        // Empty team so the spawn-team path doesn't fire either; we
        // want to assert PURELY on the kick path being suppressed
        // for leads.
        workspace.seed_test_project_with_team("proj-x", "/tmp/proj-x", &[]);

        on_connected_for_test(&workspace, &synth_lead_key("proj-x"), "lead-uuid");

        let dispatched = workspace.drain_test_dispatch_buffer();
        assert!(
            dispatched.iter().all(|c| !matches!(c, Command::Prompt { .. })),
            "lead synth keys must not trigger the worker-kick path",
        );
    }

    #[test]
    fn parse_worker_synth_key_extracts_project_and_label_for_canonical_shape() {
        let key = SessionKey::from_session_id("__spawn_worker_forge_planner_abc123__");
        assert_eq!(parse_worker_synth_key(&key), Some(("forge".to_owned(), "planner".to_owned())));
    }

    #[test]
    fn parse_worker_synth_key_rejects_lead_synth_keys() {
        let key = SessionKey::from_session_id("__spawn_forge__");
        assert_eq!(parse_worker_synth_key(&key), None);
    }

    #[test]
    fn parse_worker_synth_key_rejects_unrelated_shapes() {
        assert_eq!(parse_worker_synth_key(&SessionKey::from_session_id("not-a-synth-key")), None);
        assert_eq!(
            parse_worker_synth_key(&SessionKey::from_session_id("__spawn_worker__")),
            None,
            "worker prefix with no project/label/uuid segments must reject",
        );
    }

    /// #157: the resume-shaped worker synth key
    /// (`__resume_worker_<project>_<label>_<uuid>__`) parses the same as
    /// the fresh shape - the parser accepts both.
    #[test]
    fn parse_worker_synth_key_extracts_project_and_label_for_resume_shape() {
        let key = SessionKey::from_session_id("__resume_worker_forge_planner_abc123__");
        assert_eq!(parse_worker_synth_key(&key), Some(("forge".to_owned(), "planner".to_owned())));
    }

    #[test]
    fn is_resume_worker_synth_key_identifies_resume_prefix() {
        let resume = SessionKey::from_session_id("__resume_worker_forge_planner_abc123__");
        let fresh = SessionKey::from_session_id("__spawn_worker_forge_planner_abc123__");
        assert!(is_resume_worker_synth_key(&resume));
        assert!(!is_resume_worker_synth_key(&fresh));
    }
}
