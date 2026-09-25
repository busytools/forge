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

use crate::SessionSlot;
use crate::domain_session::DomainSession;
use crate::protocol::{Command, PendingInteractionSlot, SessionUpdate};

pub(crate) struct SessionTask {
    pub(crate) key: SessionSlot,
    pub(crate) handle: Arc<AgentHandle>,
    pub(crate) command_rx: mpsc::UnboundedReceiver<Command>,
    pub(crate) domain: Arc<Mutex<DomainSession>>,
    pub(crate) update_tx: mpsc::UnboundedSender<SessionUpdate>,
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
            slot = %self.key.display(),
            "session task started"
        );
        // Take the agent's event receiver. The bridge seeds it on
        // spawn; if it's already been taken (future refactor double-
        // tapping), surface as ConnectionFailed + return.
        let Some(mut event_rx) = self.handle.take_events() else {
            tracing::error!(
                target: "forge_workspace::session_task",
                slot = %self.key.display(),
                "AgentHandle::take_events returned None; session task aborting"
            );
            self.emit(SessionUpdate::ConnectionFailed {
                key: self.key.clone(),
                message: "agent event receiver unavailable".to_owned(),
                fatal: false,
            });
            return;
        };

        // Forge-side display_name is known the instant the workspace
        // picks an account. Emit ForgeAccountIdentity now so welcome
        // rendering shows the right label from the first frame.
        if let Some(display_name) = self.handle.display_name() {
            self.emit(SessionUpdate::ForgeAccountIdentity { key: self.key.clone(), display_name });
        }

        loop {
            tokio::select! {
                maybe_event = event_rx.recv() => {
                    let Some(event) = maybe_event else { break; };
                    let event = self.declared_models_for_session(event);
                    if !self.translate_event(event) {
                        break;
                    }
                }
                maybe_cmd = self.command_rx.recv() => {
                    let Some(cmd) = maybe_cmd else { break; };
                    self.execute_command(cmd);
                }
            }
        }
        tracing::info!(
            target: "forge_workspace::session_task",
            slot = %self.key.display(),
            "session task exiting (agent event channel closed)"
        );
        // The agent-event / command channel closed - this session's
        // subprocess is gone. Release it from the pool + command_senders
        // so a later cron fire / projects-pane click sees it as
        // not-running and re-spawns cleanly, instead of dispatching a
        // `Command::Prompt` to the now-closed channel (which fails with
        // `SessionClosed` and is silently dropped, quietly stopping
        // durable crons for the project). Guarded on handle identity so
        // a superseded task (its session re-spawned under the same key)
        // doesn't wipe its successor's live entries.
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.release_session_if_current(&self.key, &self.handle);
        }
        // Take the bridge's client slot and run the SDK's graceful
        // shutdown. Without this, release / despawn / reader-death left
        // the `claude` subprocess (and its reader/writer/stderr tasks)
        // alive until forge exited - the bridge holds the `Client` and
        // `Client` has no `Drop` of its own.
        self.handle.disconnect().await;
    }

    /// Replace a Connected event's CLI-advertised `available_models`
    /// with the session's org's declared models before `translate_event`
    /// sees it (covering both the `Connected` and `SessionReplaced`
    /// emits). Synchronous - the rows are read from config, not fetched.
    fn declared_models_for_session(&self, event: AgentEvent) -> AgentEvent {
        let AgentEvent::Connected {
            session_id,
            cwd,
            current_model,
            available_models: _,
            mode,
            history_updates,
            compaction_count,
        } = event
        else {
            return event;
        };
        // Declared models replace discovery: the picker rows are the
        // org's accounts' declared models, authored in forge.toml.
        let (available_models, canonical_model) = match self.workspace.upgrade() {
            Some(workspace) => (
                workspace.declared_models_for_session(&self.key),
                workspace.canonical_model_for_session(&self.key),
            ),
            None => (Vec::new(), None),
        };
        // The project's declared model names the session: it is what
        // forge stamped into the CLI's model slots and what every
        // model-name surface must show, rather than the id the CLI
        // resolved on its own.
        let current_model = match canonical_model {
            Some(model) => forge_primitives::CurrentModel {
                requested_id: Some(model.clone()),
                display_name_short: model.clone(),
                display_name_long: model,
                ..current_model
            },
            None => current_model,
        };
        AgentEvent::Connected {
            session_id,
            cwd,
            current_model,
            available_models,
            mode,
            history_updates,
            compaction_count,
        }
    }

    /// Translate one `AgentEvent` into the matching `SessionUpdate`.
    /// Updates `DomainSession` in-place before each emit.
    ///
    /// Returns `true` to keep the run loop running; `false` when the
    /// event is terminal for this task and the loop must exit (so the
    /// exit path releases the session and disconnects the subprocess).
    fn translate_event(&mut self, event: AgentEvent) -> bool {
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
            // Skip the catalog mirror for workers. The projects pane
            // draws a project's own sessions from this catalog and its
            // workers from `live_workers`, so a mirrored worker would be
            // listed as one of the project's sessions - and it arrives
            // here before its JSONL carries the forge:worker tag that
            // keeps the boot scan from listing it.
            if workspace.worker_lookup_for_session(&self.key).is_none() {
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
                compaction_count,
            } => {
                let history = history_updates.unwrap_or_default();
                // The slot the task was spawned under, which the CLI's
                // own id never moves: it names the occupant, this names
                // the seat.
                let key = self.key.clone();
                // Nothing re-keys on `Connected`: the task is
                // registered under the slot the CLI never moves, so
                // `pool`, `command_senders` and `domain_handles` are
                // already under the key every `Command` addresses. The
                // id is recorded against the slot's row below, which is
                // what a restart resumes the occupant from.
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
                // worker's tag must travel to the new JSONL - a
                // post-/new JSONL left untagged is listed by the boot
                // scan as one of the project's own sessions instead of
                // being hidden as a worker's. Captured before the
                // if/else because both branches consume the `cwd` field.
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
                // TODO(ved): lead sessions are currently never tagged
                // with `forge:lead`, which the catalog tolerates but
                // nobody relies on. Explicit lead tagging is a spec gap
                // we should close (apply the same retry pattern via a
                // sibling `apply_lead_tag_or_warn` that does NOT roll
                // back on failure - just warns).
                let cwd_for_tag = cwd.clone();
                // Captured before the branches below set it: a `/new`,
                // login or logout arrives as a Connected on a task that
                // has already connected once, and its row predates it.
                let replacing = self.connected_once;
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
                    // is unreachable.
                    if let Some(workspace) = self.workspace.upgrade() {
                        workspace.expire_target_inflight(
                            &self.key,
                            crate::mcp::peers::types::PeerFailureReason::TargetConnectionFailed,
                        );
                    }
                    // Same reason: the replaced identity never produces a
                    // Result, so the buffer this turn's review actions sit
                    // in would never be drained.
                    let caller = self.domain.lock().key.clone();
                    self.drain_review_activity_for(&caller);
                    // Session-scoped `/dictate` state dies with the
                    // identity too: the TUI mints a blank bucket for
                    // the replaced session, so an override left here
                    // would style a session that no longer exists
                    // while the readout claims the crate defaults.
                    // The device pick is NOT here - it is workspace
                    // state shared by every session and outlives the
                    // replacement.
                    {
                        let mut domain = self.domain.lock();
                        domain.dictate_overrides = crate::dictate::DictateOverrides::default();
                    }
                    // The store is not the first-Connected path's alone:
                    // every replacement adopts an id forge did not choose,
                    // and a boot resolves this session from its row.
                    if let Some(workspace) = self.workspace.upgrade() {
                        workspace.note_running_session_id(&self.key, &session_id);
                    }
                    self.emit(SessionUpdate::SessionReplaced {
                        key: key.clone(),
                        session_id: SessionId::new(session_id.clone()),
                        cwd,
                        current_model,
                        available_models,
                        mode,
                        history,
                        compaction_count,
                    });
                    // After SessionReplaced so the echo lands on the
                    // bucket it kept: the cleared values re-affirm the
                    // blank state.
                    self.emit(SessionUpdate::DictateOverrides {
                        key: key.clone(),
                        overrides: crate::dictate::DictateOverrides::default(),
                    });
                } else {
                    self.connected_once = true;
                    // When the connecting session is a project's lead and
                    // persists workers that are not live yet, dispatch
                    // one SpawnWorker per row. Idempotent via the
                    // live_workers gate - a reconnect after a transient
                    // failure skips re-spawn.
                    if let Some(workspace) = self.workspace.upgrade() {
                        let force_new = self.domain.lock().spawned_force_new;
                        maybe_respawn_workers_on_connected(&workspace, &self.key, force_new);
                        maybe_kick_worker_on_connected(&workspace, &self.key);
                    }
                    // A boot resolves a session from the store, so the id
                    // the CLI adopted is recorded rather than left to the
                    // next boot to guess wrong.
                    if let Some(workspace) = self.workspace.upgrade() {
                        workspace.note_running_session_id(&self.key, &session_id);
                    }
                    self.emit(SessionUpdate::Connected {
                        key: key.clone(),
                        session_id: SessionId::new(session_id.clone()),
                        cwd,
                        current_model,
                        available_models,
                        mode,
                        history,
                        compaction_count,
                    });
                    // Everything parked for this session's slot while
                    // it was asleep, delivered in arrival order: peer
                    // prompts, then crons, then Gotify, then Slack. Each
                    // re-dispatches as a regular `Command::Prompt` so the
                    // prompt-delivery path handles it uniformly with
                    // user-typed prompts.
                    self.drain_parked();
                }
                // Re-tag must fire on BOTH first-Connected and
                // post-/new Connected paths: a /new writes a fresh
                // JSONL, and an untagged one is listed by the boot scan
                // as one of the project's own sessions.
                if let Some(workspace) = self.workspace.upgrade() {
                    // The spawn's row provenance rides the domain, so a
                    // rollback can tell a row this spawn minted from one
                    // a resume inherited. A replacement Connected is a
                    // `/new` re-tag of an established worker, whose row
                    // predates this connection and is not this one's to
                    // take.
                    let minted_row = !replacing && self.domain.lock().spawn_wrote_row;
                    workspace.apply_worker_tag_or_rollback(
                        &key,
                        &session_id,
                        &cwd_for_tag,
                        minted_row,
                    );
                }
            }
            AgentEvent::AuthRequired { method_name, method_description } => {
                self.emit(SessionUpdate::AuthRequired {
                    key: self.key.clone(),
                    method_name,
                    method_description,
                });
            }
            AgentEvent::ConnectionFailed { message, kind } => {
                let key = self.key.clone();
                // A `/new` or `/resume` that fails to respawn ends the
                // live turn without a Result, so flush the same way the
                // peer-ask expiry below does.
                let caller = self.domain.lock().key.clone();
                self.drain_review_activity_for(&caller);
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
                    // A spawn that never connected still holds everything
                    // parked for its slot, and the target_session match
                    // above cannot reach those (they were never stamped).
                    // Fail the peer asks so their callers get a delivery
                    // notice rather than waiting out the timeout.
                    workspace.expire_parked_for_slot(
                        &self.key,
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
                    workspace.handle_async_worker_spawn_failure(&key, &message, kind);
                }
                self.emit(SessionUpdate::ConnectionFailed {
                    key: key.clone(),
                    message,
                    fatal: false,
                });
                // Terminal for this task: the spawn failed before any
                // Connected, so the pooled handle is dead and must not
                // survive it - otherwise the pool fast path hands the
                // dead handle back to every later click / cron fire /
                // peer ask and the retry "succeeds" into nothing.
                // Release both registrations (the resolved key the
                // workspace maps hold; `release_session_if_current` no-ops
                // on an absent key. The run loop then exits, so Drop's
                // expiry backstop fires too.
                if let Some(workspace) = self.workspace.upgrade() {
                    workspace.release_session_if_current(&self.key, &self.handle);
                }
                return false;
            }
            AgentEvent::PermissionRequest { session_id, request } => {
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
                        key: self.key.clone(),
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
                    if let Some(pending) =
                        self.domain.lock().pending_interactions.remove(&tool_call_id)
                        && let PendingInteractionSlot::Permission(tx) = pending
                    {
                        let _ = tx.send(forge_primitives::PermissionOutcome::Cancelled);
                    }
                    tracing::warn!(
                        target: "forge_workspace::session_task",
                        slot = %self.key.display(),
                        tool_id = %tool_call_id,
                        "PermissionRequest send failed; orphaned oneshot cancelled"
                    );
                }
            }
            AgentEvent::QuestionRequest { session_id, request } => {
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
                        key: self.key.clone(),
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
                    if let Some(pending) =
                        self.domain.lock().pending_interactions.remove(&tool_call_id)
                        && let PendingInteractionSlot::Question(tx) = pending
                    {
                        let _ = tx.send(forge_primitives::QuestionOutcome::Cancelled);
                    }
                    tracing::warn!(
                        target: "forge_workspace::session_task",
                        slot = %self.key.display(),
                        tool_id = %tool_call_id,
                        "QuestionRequest send failed; orphaned oneshot cancelled"
                    );
                }
            }
            AgentEvent::McpOperationError { error, .. } => {
                self.emit(SessionUpdate::McpOperationError { key: self.key.clone(), error });
            }
            AgentEvent::RuntimeReloadCompleted { .. } => {
                self.emit(SessionUpdate::RuntimeReloadCompleted { key: self.key.clone() });
            }
            AgentEvent::RuntimeReloadFailed { message, .. } => {
                self.emit(SessionUpdate::RuntimeReloadFailed { key: self.key.clone(), message });
            }
            AgentEvent::SetModeFailed { mode, message, .. } => {
                self.emit(SessionUpdate::SetModeFailed { key: self.key.clone(), mode, message });
            }
            AgentEvent::SetModelFailed { model, message, .. } => {
                self.emit(SessionUpdate::SetModelFailed { key: self.key.clone(), model, message });
            }
            AgentEvent::TurnError { message, .. } => {
                self.emit(SessionUpdate::TurnError {
                    key: self.key.clone(),
                    message,
                    class: None,
                    terminal_reason: None,
                });
            }
            AgentEvent::SessionsListed { sessions } => {
                self.emit(SessionUpdate::SessionsListed { key: self.key.clone(), sessions });
            }
            AgentEvent::StatusSnapshot { account, forge_account, .. } => {
                self.emit(SessionUpdate::StatusSnapshot {
                    key: self.key.clone(),
                    account,
                    forge_account,
                });
            }
            AgentEvent::OauthCredentialsSnapshot { credentials, .. } => {
                self.emit(SessionUpdate::OauthCredentialsSnapshot {
                    key: self.key.clone(),
                    credentials,
                });
            }
            AgentEvent::ContextUsage { percentage, max_tokens, .. } => {
                self.emit(SessionUpdate::ContextUsageSnapshot {
                    key: self.key.clone(),
                    percentage,
                    max_tokens,
                });
            }
            AgentEvent::McpSnapshot { servers, error, .. } => {
                self.emit(SessionUpdate::McpSnapshot { key: self.key.clone(), servers, error });
            }
            AgentEvent::SdkMessage { msg, .. } => {
                // A rate-limit window that is not `allowed` proves the
                // bound account exhausted: report it so the gateway
                // cools the account down and drops this session's
                // binding - the CLI's next request re-selects. The
                // trigger is literal: `allowed_warning` and unknown
                // statuses rotate too, because a warning already means
                // the window is closing. The binding is keyed by the
                // segment the child's base URL was stamped with, which
                // is the id the child runs under.
                if let forge_primitives::Message::RateLimitEvent { rate_limit_info, .. } = &msg
                    && rate_limit_info.status != forge_primitives::RateLimitStatus::Allowed
                    && let Some(workspace) = self.workspace.upgrade()
                {
                    let reset_at = rate_limit_info.resets_at.and_then(|t| u64::try_from(t).ok());
                    if let Some(session_id) = self.session_id_string() {
                        workspace.gateway.report_rate_limit(
                            self.key.org(),
                            self.key.project(),
                            &session_id,
                            reset_at,
                        );
                    }
                }
                // Clear the turn-commit marker on the turn boundary so
                // the in-flight guards stop refusing once the turn
                // ends. `Message::Result` is the SDK's signal that the
                // assistant turn has fully completed - including a
                // cancelled one, which lands as `error_during_execution`.
                // `Message::Error` is the CLI's last-gasp transport
                // failure, after which no Result follows.
                if matches!(
                    msg,
                    forge_primitives::Message::Result { .. }
                        | forge_primitives::Message::Error { .. }
                ) {
                    let caller = {
                        let mut guard = self.domain.lock();
                        guard.turn_pending = false;
                        guard.key.clone()
                    };
                    // Turn end: flush this session's accumulated review
                    // actions into one batched notice per review, routed to
                    // each review's submit origin.
                    self.drain_review_activity_for(&caller);
                }
                self.emit(SessionUpdate::ChatAppended { key: self.key.clone(), msg });
            }
            AgentEvent::HookObservation {
                tool_use_id,
                permission_mode,
                effort,
                agent_id,
                agent_type,
                ..
            } => {
                self.emit(SessionUpdate::HookObservation {
                    key: self.key.clone(),
                    tool_use_id,
                    permission_mode,
                    effort,
                    agent_id,
                    agent_type,
                });
            }
        }
        true
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
                            slot = %self.key.display(),
                            tool_id = %tool_id,
                            "permission oneshot receiver dropped before response could be sent"
                        );
                    }
                } else if let Some(other) = guard.pending_interactions.get(&tool_id) {
                    tracing::warn!(
                        target: "forge_workspace::session_task",
                        slot = %self.key.display(),
                        tool_id = %tool_id,
                        slot = ?other,
                        "RespondPermission expected Permission slot; got different kind. Outcome dropped, slot preserved."
                    );
                } else {
                    tracing::warn!(
                        target: "forge_workspace::session_task",
                        slot = %self.key.display(),
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
                            slot = %self.key.display(),
                            tool_id = %tool_id,
                            "question oneshot receiver dropped"
                        );
                    }
                } else if let Some(other) = guard.pending_interactions.get(&tool_id) {
                    tracing::warn!(
                        target: "forge_workspace::session_task",
                        slot = %self.key.display(),
                        tool_id = %tool_id,
                        slot = ?other,
                        "RespondQuestion got non-Question slot. Outcome dropped, slot preserved."
                    );
                } else {
                    tracing::warn!(
                        target: "forge_workspace::session_task",
                        slot = %self.key.display(),
                        tool_id = %tool_id,
                        "RespondQuestion found no pending interaction"
                    );
                }
            }
            other => {
                let sid = self.session_id_string();
                // `/new` starts under an id minted here, so the row this
                // session belongs to names the new session from the moment
                // it starts rather than the one it replaces.
                let fresh = if matches!(other, Command::NewSession { .. }) {
                    self.fresh_session_id()
                } else {
                    None
                };
                // The id the respawned child runs under: the one minted
                // above for `/new`, the transcript's own for `/resume`.
                // Its gateway segment is stamped from it below, so the
                // binding names the occupant the CLI actually runs as.
                let respawn_id = match &other {
                    Command::NewSession { .. } => fresh.clone(),
                    Command::ResumeSession { session_id, .. } => Some(session_id.clone()),
                    _ => None,
                };
                // A worker's mission lives in its conversation, so a
                // `/new` that emptied it would leave the worker running
                // with no idea what it is for. The TUI builds the launch
                // settings for a `/new` and knows nothing of the charter,
                // so re-deliver the one the store holds for this slot.
                //
                // Workers only. A lead's own instructions are appended to
                // its prompt by the spawn, and the store holds no charter
                // for a lead row, so a lead that runs `/new` still comes
                // back without `LEAD_DELEGATION_PREAMBLE`.
                let mut other = other;
                if let Command::NewSession { launch_settings, .. } = &mut other
                    && let Some(charter) =
                        self.workspace.upgrade().and_then(|ws| ws.stored_charter_for(&self.key))
                {
                    launch_settings.charter = Some(charter);
                }
                if let Some(respawn_id) = respawn_id.as_deref()
                    && let Some(workspace) = self.workspace.upgrade()
                    && let Command::NewSession { launch_settings, .. }
                    | Command::ResumeSession { launch_settings, .. } = &mut other
                {
                    workspace.stamp_respawn_overrides(&self.key, respawn_id, launch_settings);
                }
                if let Err(err) = execute_command_via_handle(
                    &self.handle,
                    &self.key,
                    sid.as_deref(),
                    fresh,
                    other,
                ) {
                    tracing::warn!(
                        target: "forge_workspace::session_task",
                        slot = %self.key.display(),
                        error = %err,
                        "agent command dispatch failed; bridge channel closed?"
                    );
                    // Surface it - the TUI may already have committed
                    // optimistic state (Thinking, a flipped chip) that
                    // unwinds only when the failure is visible.
                    self.emit(SessionUpdate::TurnError {
                        key: self.key.clone(),
                        message: err.to_string(),
                        class: None,
                        terminal_reason: None,
                    });
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
    // TODO(ved): gate emits on a per-task session epoch so a superseded
    // task (its slot re-spawned by a resume or an account switch) can't
    // emit a stale update onto its successor during the brief
    // post-supersession drain. Slot keying neither creates nor widens
    // that window - the successor holds the same slot either way - and
    // the switch stays idle-gated, so it is low-risk today.
    fn emit(&self, update: SessionUpdate) {
        if self.update_tx.send(update).is_err() {
            tracing::warn!(
                target: "forge_workspace::session_task",
                slot = %self.key.display(),
                "SessionUpdate channel closed; dropping event"
            );
        }
    }

    /// Flush `caller`'s buffered review activity into its batched
    /// notices. Every path a turn can end on calls this, so it has to be
    /// idempotent: `drain_review_activity` removes the buffer, and a
    /// caller with nothing buffered yields no notices. A turn that ended
    /// normally therefore leaves the teardown drains with nothing to do.
    ///
    /// `caller` is explicit because the replacement path drains a key
    /// that `DomainSession.key` is about to move off, and the buffer
    /// entry becomes unreachable once it has.
    fn drain_review_activity_for(&self, caller: &SessionSlot) {
        let Some(workspace) = self.workspace.upgrade() else { return };
        for update in workspace.drain_review_activity(caller) {
            self.emit(update);
        }
    }

    /// The id a `/new` starts under: a fresh one, recorded against the row
    /// this session belongs to, so a restart re-enters the new session
    /// rather than the one `/new` replaced. `None` when the session has no
    /// row to record against, which leaves the CLI to pick its own id.
    fn fresh_session_id(&self) -> Option<String> {
        Some(self.workspace.upgrade()?.fresh_session_id_for(&self.key))
    }

    /// Drain everything parked for this session's slot on its first
    /// `Connected`, in arrival order. The take is the session's own
    /// `(org, project, label)` bucket, so a payload parked for a team
    /// worker never lands on the project's lead.
    fn drain_parked(&self) {
        let Some(workspace) = self.workspace.upgrade() else { return };
        let parked = workspace.take_parked_for_slot(&self.key);
        self.deliver_parked_peers(&workspace, parked.peer);
        self.deliver_parked_crons(&workspace, parked.cron);
        self.deliver_parked_gotify(&workspace, parked.gotify);
        self.deliver_parked_slack(&workspace, parked.slack);
    }

    /// Deliver the peer prompts parked for this session's slot. Each is
    /// re-dispatched as a normal `Command::Prompt` against `self.key`.
    /// The existing prompt-delivery path handles it identically to a
    /// user-typed prompt - the only difference is the prose body
    /// carries the `[Question id=q-…]` / `[Message id=t-…]` wrapper
    /// that the chat renderer pattern-matches into a styled peer
    /// block (lands in C16).
    fn deliver_parked_peers(
        &self,
        workspace: &Arc<crate::Workspace>,
        pending: Vec<crate::mcp::peers::types::WrappedPrompt>,
    ) {
        if pending.is_empty() {
            return;
        }
        // Same sidebar-badge bookkeeping the running-target branch of
        // `spawn::handle_deliver_peer_prompt` does: Question wrappers
        // bump the recipient's incoming counter so the sidebar `·N↓`
        // reflects the just-arrived ask. The wrappers we drain here
        // were buffered when the target was sleeping, so the bump
        // was deferred until now. Tells / Replies / notices don't
        // bump - same rule as the running-target path.
        let facade = crate::mcp::peers::facade::ProdWorkspaceFacade::from_arc(workspace);
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
            crate::spawn::push_peer_user_turn_into_chat(workspace, &self.key, &wrapped);
            let text = wrapped.to_prose();
            if let Err(err) = workspace.dispatch_workspace_prompt(&self.key, text) {
                tracing::warn!(
                    target: "forge_workspace::session_task",
                    slot = %self.key.display(),
                    error = ?err,
                    "deliver_parked_peers: dispatch failed; prompt dropped"
                );
                crate::spawn::send_dispatch_turn_error(workspace, self.key.clone(), &err);
            }
        }
    }

    /// Deliver the cron prompts parked for this session's slot - the
    /// bucket a due cron filled while the slot was asleep. Each is
    /// echoed as a cron block (missed-marked when overdue) and
    /// re-dispatched as a plain `Command::Prompt`.
    fn deliver_parked_crons(
        &self,
        workspace: &Arc<crate::Workspace>,
        pending: Vec<crate::crons::PendingCron>,
    ) {
        if pending.is_empty() {
            return;
        }
        for cron in pending {
            let text = crate::spawn::missed_cron_text(&cron.text, cron.missed);
            crate::spawn::push_cron_prompt_into_chat(workspace, &self.key, &text);
            if let Err(err) = workspace.dispatch_workspace_prompt(&self.key, text) {
                tracing::warn!(
                    target: "forge_workspace::session_task",
                    slot = %self.key.display(),
                    error = ?err,
                    "deliver_parked_crons: dispatch failed; prompt dropped",
                );
                crate::spawn::send_dispatch_turn_error(workspace, self.key.clone(), &err);
            }
        }
    }

    /// Deliver the Gotify notifications parked for this session's slot.
    /// Each is echoed into chat as a notification block and re-dispatched
    /// as a plain `Command::Prompt`, landing as an ordinary user turn.
    /// Mirrors [`Self::deliver_parked_crons`], which echoes its own
    /// block before dispatching.
    fn deliver_parked_gotify(
        &self,
        workspace: &Arc<crate::Workspace>,
        pending: Vec<crate::mcp::gotify::types::GotifyNotification>,
    ) {
        if pending.is_empty() {
            return;
        }
        for notification in pending {
            // Echo the notification block, then re-dispatch its prose as a
            // plain user turn (mirrors the running-target path in
            // spawn::deliver_gotify_message).
            crate::spawn::push_gotify_notification_into_chat(workspace, &self.key, &notification);
            if let Err(err) =
                workspace.dispatch_workspace_prompt(&self.key, notification.to_prose())
            {
                tracing::warn!(
                    target: "forge_workspace::session_task",
                    slot = %self.key.display(),
                    error = ?err,
                    "deliver_parked_gotify: dispatch failed; prompt dropped"
                );
                crate::spawn::send_dispatch_turn_error(workspace, self.key.clone(), &err);
            }
        }
    }

    /// Deliver the Slack messages parked for this session's slot. Each
    /// conversation's is echoed into chat as one notification block and
    /// re-dispatched as a plain `Command::Prompt`, landing as an ordinary
    /// user turn. Mirrors [`Self::deliver_parked_gotify`].
    fn deliver_parked_slack(
        &self,
        workspace: &Arc<crate::Workspace>,
        pending: Vec<forge_primitives::slack::SlackMessage>,
    ) {
        if pending.is_empty() {
            return;
        }
        // The parked bucket is flat, so regroup it into the per-conversation
        // block each batch was read as.
        let mut by_conversation: std::collections::BTreeMap<String, Vec<_>> =
            std::collections::BTreeMap::new();
        for message in pending {
            by_conversation.entry(message.conversation.clone()).or_default().push(message);
        }
        for messages in by_conversation.into_values() {
            let prose = crate::spawn::slack_bundle_to_prose(&messages);
            crate::spawn::push_slack_message_into_chat(workspace, &self.key, &prose);
            if let Err(err) = workspace.dispatch_workspace_prompt(&self.key, prose) {
                tracing::warn!(
                    target: "forge_workspace::session_task",
                    slot = %self.key.display(),
                    error = ?err,
                    "deliver_parked_slack: dispatch failed; prompt dropped"
                );
                crate::spawn::send_dispatch_turn_error(workspace, self.key.clone(), &err);
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
/// Uses the stored `Weak<Workspace>` reference so a Workspace drop
/// before the task drops doesn't double-fire or panic.
impl Drop for SessionTask {
    fn drop(&mut self) {
        // Teardown backstop for the routes out of `run` that produce no
        // terminal envelope: the command channel closing on a release or
        // despawn, and the runtime dropping the task. Child death needs
        // no backstop - `reader_loop` emits ConnectionFailed on both of
        // its exit arms (stream error and stream close), and that arm
        // drains review activity before terminating the task.
        let caller = self.domain.lock().key.clone();
        self.drain_review_activity_for(&caller);
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.expire_target_inflight(
                &self.key,
                crate::mcp::peers::types::PeerFailureReason::TargetConnectionFailed,
            );
        }
    }
}

/// Shared worker Connected hook: when the session that connected is a
/// project's lead and no live workers exist for the project yet,
/// (re-)spawn its workers - one `SpawnWorker` per persisted worker row.
/// Called from `SessionTask::translate_event` (production) and
/// `on_connected_for_test` (tests). Idempotent; safe to call multiple
/// times for the same session - the `live_workers.is_empty()` gate
/// guards against double-spawn on `/new` reconnects or transient
/// retries, and `respawn_workers_for_lead` no-ops when the project has
/// no worker rows.
///
/// The slot's label carries the role: only a lead (`None`) owns a team.
fn maybe_respawn_workers_on_connected(
    workspace: &Arc<crate::Workspace>,
    slot: &crate::SessionSlot,
    force_new: bool,
) {
    if !slot.is_lead() {
        return;
    }
    if workspace.find_project_view_by_name(slot.project()).is_none() {
        return;
    }
    let Some(project_key) = workspace.project_key_for_name(slot.project()) else {
        return;
    };
    if !workspace.list_live_workers(&project_key).is_empty() {
        return;
    }
    // Re-spawn the project's persisted workers on a lead reconnect, so
    // each resumes the id the store holds for its label instead of
    // starting fresh. Workspace claims a per-project in-flight guard
    // synchronously so a fast double-Connected can't slip a second
    // worker-spawn through; the guard is released after the SpawnWorker
    // commands are dispatched.
    workspace.respawn_workers_for_lead(slot, project_key, force_new);
}

/// Shared worker-kick hook: when the session that connected is a worker,
/// dispatch a `Command::Prompt` carrying the live worker's stored kick
/// to it. Claude sessions don't act until a
/// user-turn arrives, so without this kick a worker would sit idle
/// indefinitely after spawn (its charter would shape behaviour IF
/// prompted, but nothing prompts it).
///
/// A worker spawned without a kick gets none - it stays caller-driven
/// until its lead sends it something. The re-spawn path decides what a
/// resuming worker is kicked with, so there is nothing to decide here.
///
/// Called from `SessionTask::translate_event` (production) and
/// `on_connected_for_test` (tests). The dispatch goes through the
/// Workspace command bus to the worker's own SessionTask queue,
/// which processes the prompt on its next loop iteration once
/// translate_event returns.
fn maybe_kick_worker_on_connected(workspace: &Arc<crate::Workspace>, slot: &crate::SessionSlot) {
    // The label is the worker's; a lead has none and gets no kick.
    if slot.is_lead() {
        return;
    }
    let label = slot.label();
    let Some(project_key) = workspace.project_key_for_name(slot.project()) else {
        return;
    };
    // `handle_spawn_worker` inserts the entry as Spawning before the agent
    // spawn so this hook always finds it, which makes a miss an invariant
    // violation rather than a kick-less worker. Kept apart from `None`
    // kick, which is the ordinary silent case.
    let Some(entry) =
        workspace.list_live_workers(&project_key).into_iter().rev().find(|w| w.label == label)
    else {
        tracing::warn!(
            target: "forge_workspace::workers",
            label = %label,
            slot = %slot.display(),
            "no live worker entry for this label; the worker connects unkicked and idles",
        );
        return;
    };
    let Some(kick) = entry.kick else {
        return;
    };
    // #259: kicks route through the workspace-level dispatcher so
    // multi-worker boots don't fire N simultaneous Prompts at
    // Anthropic's per-IP burst limit. The drainer fires one per
    // `KICK_DISPATCH_INTERVAL`; the first kick of an empty queue
    // has zero added latency.
    workspace.enqueue_kick(crate::workspace::KickRequest { slot: slot.clone(), prompt_body: kick });
}

/// Test-only entry point for the Connected hooks.
/// Drives both `maybe_respawn_workers_on_connected` (lead path) and
/// `maybe_kick_worker_on_connected` (worker path) directly without
/// constructing a `SessionTask` or pumping through the actor - the
/// `connected_hook_tests` module uses this to assert the trigger logic.
/// Only one hook fires per call: the slot's label selects.
#[cfg(test)]
fn on_connected_for_test(
    workspace: &Arc<crate::Workspace>,
    slot: &crate::SessionSlot,
    _real_session_id: &str,
) {
    // Normal (non-`--new`) Connected simulation; the force-new cascade
    // is exercised directly against respawn_workers_for_lead.
    maybe_respawn_workers_on_connected(workspace, slot, false);
    maybe_kick_worker_on_connected(workspace, slot);
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
/// Returns `Ok(())` on successful enqueue; `Err(...)` on AgentHandle
/// send failure (dispatcher channel closed) or on a command dropped
/// for having no `session_id` yet (pre-Connect).
pub(crate) fn execute_command_via_handle(
    handle: &Arc<AgentHandle>,
    key: &SessionSlot,
    session_id: Option<&str>,
    new_session_id: Option<String>,
    cmd: Command,
) -> Result<(), forge_agent::AgentError> {
    match cmd {
        Command::Prompt { key: _, text, attachments } => {
            let Some(sid) = session_id else {
                return Err(warn_no_session(key, "Prompt"));
            };
            handle.prompt_with_images(sid.to_owned(), text, attachments)
        }
        Command::Cancel { key: _ } => {
            let Some(sid) = session_id else {
                return Err(warn_no_session(key, "Cancel"));
            };
            handle.cancel(sid.to_owned())
        }
        Command::SetMode { key: _, mode } => {
            let Some(sid) = session_id else {
                return Err(warn_no_session(key, "SetMode"));
            };
            handle.set_mode(sid.to_owned(), mode)
        }
        Command::SetModel { key: _, model } => {
            let Some(sid) = session_id else {
                return Err(warn_no_session(key, "SetModel"));
            };
            handle.set_model(sid.to_owned(), model)
        }
        Command::NewSession { key: _, cwd, launch_settings } => {
            handle.new_session(new_session_id, cwd, launch_settings)
        }
        Command::ResumeSession { key: _, session_id, cwd, launch_settings } => {
            handle.resume_session(session_id, cwd, launch_settings)
        }
        Command::ReconnectMcpServer { key: _, server_name } => {
            let Some(sid) = session_id else {
                return Err(warn_no_session(key, "ReconnectMcpServer"));
            };
            handle.reconnect_mcp_server(sid.to_owned(), server_name)
        }
        Command::ToggleMcpServer { key: _, server_name, enabled } => {
            let Some(sid) = session_id else {
                return Err(warn_no_session(key, "ToggleMcpServer"));
            };
            handle.toggle_mcp_server(sid.to_owned(), server_name, enabled)
        }
        Command::RespondPermission { .. } | Command::RespondQuestion { .. } => {
            tracing::error!(
                target: "forge_workspace::session_task",
                slot = %key.display(),
                command = ?cmd,
                "Respond* commands must be handled by SessionTask::execute_command before delegation"
            );
            Ok(())
        }
        // App-level commands are caught in Workspace::dispatch's
        // app-level branch; they never reach this helper.
        // Handled inline in `Workspace::dispatch`; never agent traffic.
        misrouted @ (Command::SetDictateOverride { .. }
        | Command::ResetDictateOverrides { .. }
        | Command::SetDictateDevice { .. }
        | Command::DictateStart { .. }
        | Command::DictateStop { .. }
        | Command::SpawnProject { .. }
        | Command::SpawnSession { .. }
        | Command::StartDefault { .. }
        | Command::DeliverPeerPrompt { .. }
        | Command::SpawnWorker { .. }
        | Command::CloseWorker { .. }
        | Command::DespawnWorker { .. }
        | Command::DeliverWorkerPrompt { .. }
        | Command::DeliverWorkerPromptToLead { .. }
        | Command::DeliverGotifyMessage { .. }
        | Command::RespondSlackPost { .. }
        | Command::OpenUrl { .. }
        | Command::SaveReviewThreads { .. }
        | Command::RemoveReviewThread { .. }
        | Command::SetReviewThreadStatus { .. }
        | Command::PersistSpinner { .. }
        | Command::CloseSession { .. }
        | Command::UpsertReviewThread { .. }
        | Command::SubmitReview { .. }) => {
            tracing::warn!(
                target: "forge_workspace::session_task",
                slot = %key.display(),
                command = ?misrouted,
                "App-level command unexpectedly routed via per-session path"
            );
            Ok(())
        }
    }
}

fn warn_no_session(key: &SessionSlot, command: &'static str) -> forge_agent::AgentError {
    tracing::warn!(
        target: "forge_workspace::session_task",
        slot = %key.display(),
        command,
        "command dropped: no session_id stamped on DomainSession yet",
    );
    forge_agent::AgentError::NoSession { command }
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
    if let AgentEvent::ConnectionFailed { .. } = event {
        // The subprocess is gone - drop the runtime/turn mirrors so the
        // in-flight guards don't read a stale turn.
        domain.runtime_state = None;
        domain.turn_pending = false;
    }
    if let AgentEvent::Connected { session_id, .. } = event {
        domain.session_id = Some(SessionId::new(session_id.clone()));
        domain.runtime_state = None;
        domain.turn_pending = false;
    }
    // Mirror runtime liveness from `session_state_changed` so the
    // workspace's in-flight guards see a turn authoritatively,
    // independent of the TUI gate. Reuse the canonical decoder parser
    // rather than re-inlining it.
    if let AgentEvent::SdkMessage {
        msg: forge_primitives::Message::System { subtype, data, .. },
        ..
    } = event
        && subtype == "session_state_changed"
        && let Some(state) =
            forge_agent::translate::state_parsing::parse_runtime_session_state(data.get("state"))
    {
        domain.runtime_state = Some(state);
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
    use forge_agent::client::SpawnFailureKind;

    fn empty_domain() -> DomainSession {
        let (handle, _rx) = Agent::testing_stub();
        DomainSession::new(SessionSlot::from_str_for_test("test"), Some(Arc::new(handle)))
    }

    /// The slot a test task carries. Drains resolve the parked bucket
    /// against it, so a test that parks must park under the same triple.
    fn test_slot() -> crate::SessionSlot {
        crate::SessionSlot::lead("TestOrg", "forge")
    }

    /// Drive a worker's Connected through `translate_event` with a session
    /// id the tag write cannot use, so the write fails non-NotFound and the
    /// rollback arm runs, and return the row it left behind.
    ///
    /// `connected_once` is the shape under test: false is a spawn's first
    /// Connected, true is the `/new` re-tag of a worker that is already
    /// established. The domain carries the spawn's provenance either way,
    /// since a `/new` reuses it.
    async fn tag_rollback_leftover_row(connected_once: bool) -> Option<serde_json::Value> {
        let cfg_dir = tempfile::tempdir().expect("cfg tempdir");
        std::fs::create_dir_all(cfg_dir.path().join("forge")).expect("forge dir");
        std::fs::write(
            cfg_dir.path().join("forge").join("forge.toml"),
            "[[orgs]]\nname = \"Default\"\naccounts = [\"Acct\"]\n\n\
             [[orgs.projects]]\nname = \"forge\"\npath = \"/tmp/tag-rollback-row\"\n\n\
             [[accounts]]\ndisplay_name = \"Acct\"\ntoken = \"t\"\nmodels = [\"claude-sonnet-5\"]\nprovider = \"anthropic\"\n",
        )
        .expect("write forge.toml");
        let config = crate::config::load_from_dir(cfg_dir.path()).expect("load config");
        let (workspace, mut update_rx) =
            crate::Workspace::testing_stub_with_config(cfg_dir.path().to_owned(), config)
                .expect("stub over the fixture config");
        let db_dir = tempfile::tempdir().expect("db tempdir");
        workspace.install_db_for_test(
            crate::store::Db::open(&db_dir.path().join("db.redb")).expect("open db"),
        );
        let project_key = workspace.project_key_for_name("forge").expect("the fixture project");
        let worker_slot = crate::SessionSlot::worker("Default", "forge", "steward");
        workspace.insert_live_worker(
            &project_key,
            crate::mcp::workers::types::WorkerEntry {
                label: "steward".to_owned(),
                charter: "c".to_owned(),
                slot: worker_slot.clone(),
                session_id: Some(forge_primitives::SessionId::new("steward-id")),
                status: forge_primitives::WorkerLiveness::Spawning,
                spawned_at: std::time::SystemTime::UNIX_EPOCH,
                spawned_by: crate::SessionSlot::from_str_for_test("lead-uuid"),
                // Its tag never landed, so the rollback's second fact holds.
                needs_tag: true,
                is_git_repo_at_spawn: false,
                diagnostic: None,
                kick: None,
            },
        );
        workspace
            .record_worker_row(
                &project_key,
                "steward",
                "steward-id",
                "charter",
                Some("kick"),
                None,
                false,
                false,
            )
            .expect("seed the row the rollback judges");

        let (handle, _agent_cmds) = Agent::testing_stub();
        let arc = Arc::new(handle);
        let domain = workspace.register_domain_session(worker_slot.clone(), Some(Arc::clone(&arc)));
        // The spawn that wrote the row is the one this task runs.
        domain.lock().spawn_wrote_row = true;
        let (_cmd_tx, command_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::protocol::Command>();
        let mut task = SessionTask {
            key: worker_slot.clone(),
            handle: arc,
            command_rx,
            domain,
            update_tx: workspace.update_sender(),
            connected_once,
            workspace: Arc::downgrade(&workspace),
        };

        task.translate_event(AgentEvent::Connected {
            session_id: "not-a-uuid".to_owned(),
            cwd: "/tmp/tag-rollback-row".to_owned(),
            current_model: forge_primitives::CurrentModel {
                resolved_id: "claude".to_owned(),
                display_name_short: "claude".to_owned(),
                display_name_long: "claude".to_owned(),
                requested_id: None,
                catalog_id: None,
                supports_effort: false,
                supported_effort_levels: Vec::new(),
                supports_auto_mode: None,
                supports_adaptive_thinking: None,
                is_authoritative: true,
            },
            available_models: Vec::new(),
            mode: None,
            history_updates: None,
            compaction_count: 0,
        });

        // The rollback runs in a detached task, so wait for the Removed
        // event that says it ran rather than for a wall clock.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Some(update) = update_rx.recv().await {
                if let crate::protocol::SessionUpdate::WorkerStatusChanged { action, .. } = update
                    && action == crate::protocol::WorkerStatusAction::Removed
                {
                    return;
                }
            }
        })
        .await
        .expect("the rollback emits a Removed event");

        let db = workspace.db.lock();
        crate::store::sessions::get(db.as_ref().expect("db"), "Default", "forge", "steward")
            .expect("read the row the rollback left")
            .map(|row| serde_json::json!({ "charter": row.charter, "kick": row.kick }))
    }

    /// A spawn's first Connected owns the row it minted: the rollback
    /// discards the spawn, and a row left behind re-spawns a worker the
    /// caller was told had failed.
    #[tokio::test]
    async fn a_first_connected_rolls_back_the_row_its_spawn_minted() {
        assert!(
            tag_rollback_leftover_row(false).await.is_none(),
            "the first Connected of a minting spawn takes the row with the rollback",
        );
    }

    /// A `/new` re-tag runs the same arm over a row that predates the
    /// connection, and the spawn's provenance is still stamped on the
    /// domain the `/new` reuses. The first-connect count is the only thing
    /// separating the two, so dropping it deletes a live worker's row.
    #[tokio::test]
    async fn a_new_session_retag_keeps_the_workers_row() {
        assert!(
            tag_rollback_leftover_row(true).await.is_some(),
            "a `/new` re-tag must not take the row of a worker that is already established",
        );
    }

    fn workspace_with_account_config_dir(
        _config_dir: &str,
    ) -> (tempfile::TempDir, Arc<crate::Workspace>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let forge = dir.path().join("forge");
        std::fs::create_dir_all(&forge).expect("forge dir");
        std::fs::write(
            forge.join("forge.toml"),
            "[[orgs]]\nname = \"Default\"\naccounts = [\"Acct\"]\n\n\
             [[orgs.projects]]\nname = \"forge\"\npath = \"~/Projects/forge\"\n\n\
             [[accounts]]\ndisplay_name = \"Acct\"\ntoken = \"t\"\nmodels = [\"claude-sonnet-5\"]\nprovider = \"anthropic\"\n",
        )
        .expect("write forge.toml");
        let workspace =
            Arc::new(crate::Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        (dir, workspace)
    }

    /// A workspace holding one submitted review plus one un-drained
    /// reply from `worker`, i.e. a turn that has touched a review and
    /// not yet ended. Returns the tempdir (kept alive for the db), the
    /// workspace, the review's submit origin, and the replying session.
    fn workspace_with_pending_review_activity()
    -> (tempfile::TempDir, Arc<crate::Workspace>, SessionSlot, SessionSlot) {
        use forge_primitives::review::{
            ReviewAnchor, ReviewAuthor, ReviewComment, ReviewSide, ReviewStatus, ReviewThread,
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let (workspace, _rx) =
            crate::Workspace::testing_stub_with_config_dir(dir.path().to_path_buf());
        workspace.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        workspace.save_review_threads(
            "forge",
            "feat",
            &[ReviewThread {
                id: "a".to_owned(),
                anchor: ReviewAnchor {
                    path: "src/x.rs".to_owned(),
                    side: ReviewSide::New,
                    line: 1,
                    content_hash: 1,
                    context: vec!["ctx".to_owned()],
                    base_ref: "main".to_owned(),
                },
                comments: vec![ReviewComment {
                    author: ReviewAuthor::User,
                    text: "look".to_owned(),
                    at: "t".to_owned(),
                    review_id: None,
                }],
                status: ReviewStatus::Open,
                created_at: "t".to_owned(),
                updated_at: "t".to_owned(),
                commit: None,
            }],
        );
        let reviewer = SessionSlot::from_str_for_test("reviewer");
        let worker = SessionSlot::from_str_for_test("worker");
        workspace.submit_review("forge", "feat", None, &["a".to_owned()], reviewer.clone());
        workspace
            .review_reply(&worker, "forge", "feat", "a", "implementer", "fixed", "t")
            .expect("reply recorded as this turn's activity");
        (dir, workspace, reviewer, worker)
    }

    /// A `SessionTask` bound to `key`, plus the receiver for what it emits.
    fn review_task_for(
        workspace: &Arc<crate::Workspace>,
        key: &SessionSlot,
    ) -> (SessionTask, mpsc::UnboundedReceiver<SessionUpdate>) {
        let (handle, _cmds) = Agent::testing_stub();
        let handle = Arc::new(handle);
        let (_cmd_tx, command_rx) = mpsc::unbounded_channel();
        let (update_tx, update_rx) = mpsc::unbounded_channel();
        let domain = Arc::new(Mutex::new(DomainSession::new(key.clone(), Some(handle.clone()))));
        let task = SessionTask {
            key: key.clone(),
            handle,
            command_rx,
            domain,
            update_tx,
            connected_once: false,
            workspace: Arc::downgrade(workspace),
        };
        (task, update_rx)
    }

    /// A `SessionTask` bound to `key`, plus the receiver for the agent
    /// commands it forwards. `review_task_for` drops that receiver;
    /// this keeps it for a test that asserts what the task dispatched.
    fn command_task_for(
        workspace: &Arc<crate::Workspace>,
        key: &SessionSlot,
    ) -> (SessionTask, mpsc::UnboundedReceiver<forge_primitives::AgentCommand>) {
        let (handle, cmds) = Agent::testing_stub();
        let handle = Arc::new(handle);
        let (_cmd_tx, command_rx) = mpsc::unbounded_channel();
        let (update_tx, _update_rx) = mpsc::unbounded_channel();
        let domain = Arc::new(Mutex::new(DomainSession::new(key.clone(), Some(handle.clone()))));
        let task = SessionTask {
            key: key.clone(),
            handle,
            command_rx,
            domain,
            update_tx,
            connected_once: false,
            workspace: Arc::downgrade(workspace),
        };
        (task, cmds)
    }

    /// A worker's mission lives in its conversation, so `/new` has to
    /// re-deliver the charter the store holds for its slot. The TUI
    /// builds the launch settings for a `/new` and knows nothing of the
    /// charter, so without the re-delivery a worker that ran `/new`
    /// comes back as a session that is still its slot but no longer its
    /// worker - alive-looking and doing nothing.
    #[test]
    fn a_worker_that_runs_new_is_re_delivered_its_charter() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (workspace, _rx) =
            crate::Workspace::testing_stub_with_config_dir(dir.path().to_path_buf());
        workspace.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        let slot = SessionSlot::worker("TestOrg", "forge", "reviewer");
        workspace.seed_test_session_charter(&slot, "mind the queues", false);
        let (task, mut agent_rx) = command_task_for(&workspace, &slot);

        task.execute_command(Command::NewSession {
            key: slot.clone(),
            cwd: "/tmp/forge".to_owned(),
            launch_settings: forge_agent::client::SessionLaunchSettings::default(),
        });

        let Some(forge_primitives::AgentCommand::NewSession { launch_settings, .. }) =
            agent_rx.try_recv().ok()
        else {
            panic!("the task forwards the new-session command to the handle");
        };
        assert_eq!(
            launch_settings.get("charter").and_then(serde_json::Value::as_str),
            Some("mind the queues"),
            "the worker comes back with the charter its row holds",
        );
    }

    /// A respawn moves the id the child runs under, so its gateway
    /// binding moves with it. `/new` stamps the id it mints and
    /// `/resume` the transcript it re-enters, the segment the session
    /// left behind keeps no binding, and `bound_account_for` - what the
    /// projects pane reads - follows the child that is actually running
    /// rather than the one it replaced.
    #[test]
    fn a_respawn_moves_the_gateway_binding_to_the_id_the_child_runs_under() {
        let (workspace, _rx) = crate::Workspace::testing_stub();
        let slot = SessionSlot::worker("Busytools", "forge", "reviewer");
        let account = forge_gateway::AccountKey("OpenRouter".to_owned());
        let (handle, _cmds) = Agent::testing_stub();
        workspace.pool.lock().insert(
            slot.clone(),
            crate::workspace::PooledAgent {
                handle: Arc::new(handle),
                account: account.clone(),
                permission_mode: None,
                registration: Some(forge_gateway::binding::Registration {
                    org: "Busytools".to_owned(),
                    project: "forge".to_owned(),
                    session: "spawn-id".to_owned(),
                    account: account.clone(),
                    provider: forge_primitives::account::Provider::Anthropic,
                }),
                session_id: "spawn-id".to_owned(),
            },
        );
        workspace.gateway.bindings.bind("Busytools", "forge", "spawn-id", account.clone());
        let (task, mut agent_rx) = command_task_for(&workspace, &slot);

        let stamped_base_url = |launch_settings: &serde_json::Value| -> Option<String> {
            launch_settings
                .get("env_overrides")
                .and_then(|overrides| overrides.get("ANTHROPIC_BASE_URL"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };
        let listener_base = format!("http://127.0.0.1:{}", workspace.gateway_port());
        // A respawn's settings are built by the TUI, which knows nothing of
        // the spawn-time flags, so the tools forge has replaced have to be
        // re-denied here or the new occupant gets them back.
        let denied = |launch_settings: &serde_json::Value| -> String {
            launch_settings
                .get("extra_args")
                .and_then(serde_json::Value::as_array)
                .and_then(|pairs| {
                    pairs.iter().find(|pair| {
                        pair.get(0).and_then(serde_json::Value::as_str) == Some("disallowedTools")
                    })
                })
                .and_then(|pair| pair.get(1).and_then(serde_json::Value::as_str))
                .unwrap_or_default()
                .to_owned()
        };

        task.execute_command(Command::NewSession {
            key: slot.clone(),
            cwd: "/tmp/forge".to_owned(),
            launch_settings: forge_agent::client::SessionLaunchSettings::default(),
        });

        let Some(forge_primitives::AgentCommand::NewSession {
            session_id, launch_settings, ..
        }) = agent_rx.try_recv().ok()
        else {
            panic!("the task forwards the new-session command to the handle");
        };
        let minted = session_id.expect("`/new` starts under a minted id");
        assert_eq!(
            stamped_base_url(&launch_settings).as_deref(),
            Some(format!("{listener_base}/Busytools/forge/{minted}").as_str()),
            "`/new` stamps the base URL with the id it mints for the child",
        );
        assert_eq!(
            workspace.bound_account_for(&slot),
            Some(account.clone()),
            "the account read for the slot follows the child that is running",
        );
        for tool in [
            "CronCreate",
            "CronDelete",
            "CronList",
            "TaskCreate",
            "TaskGet",
            "TaskList",
            "TaskUpdate",
            "SendMessage",
            "ListAgents",
            "Workflow",
            "RemoteTrigger",
        ] {
            assert!(
                denied(&launch_settings).split(',').any(|name| name == tool),
                "{tool} must reach a respawned session's launch; got {:?}",
                denied(&launch_settings),
            );
        }
        assert_eq!(
            workspace.gateway.bindings.binding_for("Busytools", "forge", "spawn-id"),
            None,
            "the id the session left behind keeps no binding",
        );

        // The same stamp on the routed `/resume`, whose id comes from the
        // command rather than a mint.
        task.execute_command(Command::ResumeSession {
            key: slot.clone(),
            session_id: "resumed-id".to_owned(),
            cwd: "/tmp/forge".to_owned(),
            launch_settings: forge_agent::client::SessionLaunchSettings::default(),
        });

        let Some(forge_primitives::AgentCommand::ResumeSession { launch_settings, .. }) =
            agent_rx.try_recv().ok()
        else {
            panic!("the task forwards the resume command to the handle");
        };
        assert_eq!(
            stamped_base_url(&launch_settings).as_deref(),
            Some(format!("{listener_base}/Busytools/forge/resumed-id").as_str()),
            "`/resume` stamps the base URL with the transcript it re-enters",
        );
        assert_eq!(
            workspace.bound_account_for(&slot),
            Some(account),
            "the binding follows the resumed occupant",
        );
        assert_eq!(
            workspace.gateway.bindings.binding_for("Busytools", "forge", &minted),
            None,
            "and the segment the new session left behind is dropped",
        );
    }

    /// A respawn rebuilds the launch from TUI-built settings, so every denial
    /// a worker's own spawn wrote has to come back from the row that holds
    /// its flags. `AskUserQuestion` is the one the respawn stamp used to
    /// omit: a non-interactive worker that regained it would put a question
    /// in a row nobody is watching, and block there unseen.
    #[test]
    fn a_respawned_worker_keeps_its_own_denials() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (workspace, _rx) =
            crate::Workspace::testing_stub_with_config_dir(dir.path().to_path_buf());
        workspace.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        let slot = SessionSlot::worker("TestOrg", "forge", "reviewer");
        workspace.seed_test_session_charter(&slot, "review the diff", false);
        let (task, mut agent_rx) = command_task_for(&workspace, &slot);

        task.execute_command(Command::NewSession {
            key: slot.clone(),
            cwd: "/tmp/forge".to_owned(),
            launch_settings: forge_agent::client::SessionLaunchSettings::default(),
        });

        let Some(forge_primitives::AgentCommand::NewSession { launch_settings, .. }) =
            agent_rx.try_recv().ok()
        else {
            panic!("the task forwards the new-session command to the handle");
        };
        let denied = respawn_denials(&launch_settings);
        let names: Vec<&str> = denied.split(',').collect();
        for tool in ["AskUserQuestion", "EnterWorktree", "ExitWorktree"] {
            assert!(
                names.contains(&tool),
                "{tool} must come back on a non-interactive worker's respawn; got {denied:?}",
            );
        }
    }

    /// The question denial belongs to the spawn, not to the kind: a worker
    /// the user asked to talk to directly keeps `AskUserQuestion` across a
    /// respawn, and still keeps the worktree pins that make it a worker.
    #[test]
    fn a_respawned_interactive_worker_keeps_the_question() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (workspace, _rx) =
            crate::Workspace::testing_stub_with_config_dir(dir.path().to_path_buf());
        workspace.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        let slot = SessionSlot::worker("TestOrg", "forge", "reviewer");
        workspace.seed_test_session_charter(&slot, "review the diff", true);
        let (task, mut agent_rx) = command_task_for(&workspace, &slot);

        task.execute_command(Command::NewSession {
            key: slot.clone(),
            cwd: "/tmp/forge".to_owned(),
            launch_settings: forge_agent::client::SessionLaunchSettings::default(),
        });

        let Some(forge_primitives::AgentCommand::NewSession { launch_settings, .. }) =
            agent_rx.try_recv().ok()
        else {
            panic!("the task forwards the new-session command to the handle");
        };
        let denied = respawn_denials(&launch_settings);
        let names: Vec<&str> = denied.split(',').collect();
        assert!(
            !names.contains(&"AskUserQuestion"),
            "an interactive worker asks its own row's user; got {denied:?}",
        );
        assert!(
            names.contains(&"EnterWorktree"),
            "and it is still a worker, pinned to its spawn location; got {denied:?}",
        );
    }

    /// The kind half of the same stamp: a lead respawns without the
    /// worktree pins because a lead still hops worktrees, and keeps the
    /// question because a lead's row is where the user is.
    #[test]
    fn a_respawned_lead_keeps_its_own_surface() {
        let (workspace, _rx) = crate::Workspace::testing_stub();
        let slot = SessionSlot::lead("TestOrg", "forge");
        let (task, mut agent_rx) = command_task_for(&workspace, &slot);

        task.execute_command(Command::NewSession {
            key: slot.clone(),
            cwd: "/tmp/forge".to_owned(),
            launch_settings: forge_agent::client::SessionLaunchSettings::default(),
        });

        let Some(forge_primitives::AgentCommand::NewSession { launch_settings, .. }) =
            agent_rx.try_recv().ok()
        else {
            panic!("the task forwards the new-session command to the handle");
        };
        let denied = respawn_denials(&launch_settings);
        let names: Vec<&str> = denied.split(',').collect();
        for tool in ["EnterWorktree", "ExitWorktree", "AskUserQuestion"] {
            assert!(!names.contains(&tool), "a lead keeps {tool} across a respawn; got {denied:?}");
        }
        assert!(
            names.contains(&"Workflow"),
            "the replaced set still reaches a lead's respawn; got {denied:?}",
        );
    }

    /// The `--disallowedTools` value a respawn's launch carries.
    fn respawn_denials(launch_settings: &serde_json::Value) -> String {
        launch_settings
            .get("extra_args")
            .and_then(serde_json::Value::as_array)
            .and_then(|pairs| {
                pairs.iter().find(|pair| {
                    pair.get(0).and_then(serde_json::Value::as_str) == Some("disallowedTools")
                })
            })
            .and_then(|pair| pair.get(1).and_then(serde_json::Value::as_str))
            .unwrap_or_default()
            .to_owned()
    }

    /// First-Connected drains the session's buffered Slack messages: each
    /// dispatches a plain `Command::Prompt` AND echoes a
    /// `SlackMessageAppended` so a message that arrived while the project was
    /// asleep shows its block once the session connects. Driven through
    /// Connected rather than the private drain so the ordering is pinned -
    /// the drain runs after the Connected emit, and both carry the task's
    /// own slot, which is the bucket the message was parked under.
    #[test]
    fn first_connected_drains_parked_slack_and_echoes_block() {
        let (workspace, mut update_rx) = crate::Workspace::testing_stub();
        workspace.seed_test_project("slack-drain", "/tmp/slack-drain");

        // The slot production derives for the project this fixture seeds,
        // and the bucket a delivery for that project's lead parks on.
        let session_key = SessionSlot::lead("TestOrg", "slack-drain");
        let domain =
            Arc::new(parking_lot::Mutex::new(DomainSession::new(session_key.clone(), None)));
        workspace.park_slack(
            &session_key,
            vec![forge_primitives::slack::SlackMessage {
                workspace: "acme".to_owned(),
                conversation: "D1".to_owned(),
                conversation_label: "U9".to_owned(),
                ts: "100.000001".to_owned(),
                thread_ts: None,
                user: Some("U9".to_owned()),
                author: None,
                text: "the buffered text".to_owned(),
                parent_user_id: None,
                latest_reply: None,
                files: Vec::new(),
            }],
        );

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
            connected_once: false,
            workspace: Arc::downgrade(&workspace),
        };

        workspace.enable_test_dispatch_intercept();
        task.translate_event(connected_event(&session_key.display(), "/tmp/slack-drain"));

        let dispatched = workspace.drain_test_dispatch_buffer();
        assert!(
            dispatched.iter().any(|c| matches!(
                c, crate::protocol::Command::Prompt { key, text, .. }
                    if *key == session_key && text.contains("the buffered text")
            )),
            "the buffered message arrives as the session's own prompt: {dispatched:?}",
        );

        let mut echoed = false;
        while let Ok(u) = update_rx.try_recv() {
            if matches!(
                u,
                SessionUpdate::SlackMessageAppended { key, prose }
                    if key == session_key &&prose.contains("the buffered text")
            ) {
                echoed = true;
            }
        }
        assert!(echoed, "an asleep-buffered Slack message echoes a SlackMessageAppended on drain");

        assert!(
            workspace.take_parked_for_slot(&session_key).slack.is_empty(),
            "the buffer drains once flushed",
        );
    }

    /// The drain runs after the Connected emit, so the session's own
    /// `Connected` update lands before the drained Slack echo - a drain
    /// hoisted above the emit paints the block before the session the block
    /// belongs to. Driven through Connected rather than the private drain so
    /// the ordering is observable on the update channel.
    #[test]
    fn first_connected_emits_connected_before_draining_slack() {
        let (workspace, mut update_rx) = crate::Workspace::testing_stub();
        workspace.seed_test_project("slack-rekey", "/tmp/slack-rekey");
        let slot = SessionSlot::lead("TestOrg", "slack-rekey");
        let domain = Arc::new(parking_lot::Mutex::new(DomainSession::new(slot.clone(), None)));
        workspace.park_slack(&slot, vec![buffered_slack("buffered while asleep")]);

        let (handle, _agent_cmd_rx) = Agent::testing_stub();
        let (_cmd_tx, command_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::protocol::Command>();
        let update_tx = workspace.update_sender();
        let mut task = SessionTask {
            key: slot.clone(),
            handle: Arc::new(handle),
            command_rx,
            domain: Arc::clone(&domain),
            update_tx,
            connected_once: false,
            workspace: Arc::downgrade(&workspace),
        };

        workspace.enable_test_dispatch_intercept();
        task.translate_event(connected_event(&slot.display(), "/tmp/slack-rekey"));

        let dispatched = workspace.drain_test_dispatch_buffer();
        assert!(
            dispatched.iter().any(|c| matches!(
                c, crate::protocol::Command::Prompt { key, text, .. }
                    if *key == slot && text.contains("buffered while asleep")
            )),
            "the drained prompt rides the task's own slot: {dispatched:?}",
        );

        let mut connected = false;
        let mut echo_after_connected = false;
        while let Ok(u) = update_rx.try_recv() {
            match u {
                SessionUpdate::Connected { .. } => connected = true,
                SessionUpdate::SlackMessageAppended { key, .. } if key == slot => {
                    assert!(
                        connected,
                        "the session's Connected lands before its drained Slack echo",
                    );
                    echo_after_connected = true;
                }
                _ => {}
            }
        }
        assert!(echo_after_connected, "the drained message still echoes a block");
    }

    /// The review-activity notice a task emitted, if any.
    fn drained_notice(
        update_rx: &mut mpsc::UnboundedReceiver<SessionUpdate>,
    ) -> Option<(SessionSlot, String, usize, String)> {
        let mut notice = None;
        while let Ok(update) = update_rx.try_recv() {
            if let SessionUpdate::ReviewActivityNotice { key, branch, waiting, message } = update {
                notice = Some((key, branch, waiting, message));
            }
        }
        notice
    }

    fn result_message(subtype: &str, is_error: bool) -> forge_primitives::Message {
        serde_json::from_value(serde_json::json!({
            "type": "result",
            "subtype": subtype,
            "duration_ms": 1,
            "duration_api_ms": 1,
            "is_error": is_error,
            "num_turns": 1,
            "session_id": "worker"
        }))
        .expect("parse result message")
    }

    /// Turn-end wiring: a `Message::Result` on a worker session drains its
    /// accumulated review activity into one `ReviewActivityNotice` routed to
    /// the submit origin. Guards the seam - a wrong `Message::Result` arm
    /// kills the whole notification with every unit test still green.
    #[tokio::test]
    async fn turn_end_result_drains_review_activity_to_a_notice() {
        let (_dir, workspace, reviewer, worker) = workspace_with_pending_review_activity();
        let (mut task, mut update_rx) = review_task_for(&workspace, &worker);

        task.translate_event(AgentEvent::SdkMessage {
            session_id: "worker".to_owned(),
            msg: result_message("success", false),
        });

        let (key, branch, waiting, message) = drained_notice(&mut update_rx)
            .expect("a ReviewActivityNotice emits on the turn's Result");
        assert_eq!(key, reviewer, "the notice routes to the submit origin, not the worker");
        assert_eq!(branch, "feat", "the notice names the branch it is about");
        assert_eq!(waiting, 1, "the replied-to thread now awaits the reviewer");
        assert!(message.contains("review #1"), "the notice names the review: {message}");
        assert!(message.contains("1 replied"), "the tally counts the reply: {message}");
    }

    /// A cancelled turn reaches the same `Message::Result` arm - the CLI
    /// reports the interrupt as `error_during_execution`, not as a
    /// separate terminal envelope.
    #[tokio::test]
    async fn cancelled_turn_result_drains_review_activity() {
        let (_dir, workspace, reviewer, worker) = workspace_with_pending_review_activity();
        let (mut task, mut update_rx) = review_task_for(&workspace, &worker);

        task.translate_event(AgentEvent::SdkMessage {
            session_id: "worker".to_owned(),
            msg: result_message("error_during_execution", true),
        });

        let (key, ..) = drained_notice(&mut update_rx).expect("a cancelled turn still notifies");
        assert_eq!(key, reviewer);
    }

    /// The gateway edge: a rate_limit_event whose status is not
    /// `allowed` reports the gateway, which drops the account binding the
    /// session's requests were stamped with. The binding is keyed by the id
    /// the CLI reported, so the fixture stamps one on the domain. An
    /// `allowed` frame reports nothing.
    #[test]
    fn a_rate_limit_event_not_allowed_reports_the_gateway_for_the_session_key() {
        let (workspace, _rx) = crate::Workspace::testing_stub();
        let session_id = "w-uuid";
        let key = SessionSlot::from_str_for_test(session_id);
        let (mut task, _update_rx) = review_task_for(&workspace, &key);
        task.domain.lock().session_id = Some(forge_primitives::SessionId::new(session_id));

        workspace.gateway.bindings.bind(
            key.org(),
            key.project(),
            session_id,
            forge_gateway::AccountKey("A".to_owned()),
        );

        let rejected = forge_primitives::Message::RateLimitEvent {
            rate_limit_info: forge_primitives::RateLimitInfo {
                status: forge_primitives::RateLimitStatus::Rejected,
                resets_at: Some(1_800_000_000),
                rate_limit_type: None,
                utilization: None,
                overage_status: None,
                overage_resets_at: None,
                overage_disabled_reason: None,
                raw: serde_json::Map::new(),
            },
            uuid: "u1".to_owned(),
            session_id: session_id.to_owned(),
        };
        task.translate_event(AgentEvent::SdkMessage {
            session_id: session_id.to_owned(),
            msg: rejected,
        });

        assert!(
            workspace.gateway.bindings.binding_for(key.org(), key.project(), session_id).is_none(),
            "the report drops the session's own binding",
        );

        // The re-bound key must survive the allowed frame: allowed
        // frames arrive every turn, so a lost guard here would rotate
        // a live session's binding on each one. Re-binding the task's
        // OWN key is what makes this assert kill the guard regression.
        workspace.gateway.bindings.bind(
            key.org(),
            key.project(),
            session_id,
            forge_gateway::AccountKey("C".to_owned()),
        );
        task.translate_event(AgentEvent::SdkMessage {
            session_id: session_id.to_owned(),
            msg: forge_primitives::Message::RateLimitEvent {
                rate_limit_info: forge_primitives::RateLimitInfo {
                    status: forge_primitives::RateLimitStatus::Allowed,
                    resets_at: None,
                    rate_limit_type: None,
                    utilization: None,
                    overage_status: None,
                    overage_resets_at: None,
                    overage_disabled_reason: None,
                    raw: serde_json::Map::new(),
                },
                uuid: "u2".to_owned(),
                session_id: session_id.to_owned(),
            },
        });
        assert_eq!(
            workspace.gateway.bindings.binding_for(key.org(), key.project(), session_id),
            Some(forge_gateway::AccountKey("C".to_owned())),
            "an allowed frame rotates nothing",
        );
    }

    /// `Message::Error` is the CLI's last-gasp transport failure; no
    /// Result follows it, so it has to drain on its own.
    #[tokio::test]
    async fn transport_error_drains_review_activity() {
        let (_dir, workspace, reviewer, worker) = workspace_with_pending_review_activity();
        let (mut task, mut update_rx) = review_task_for(&workspace, &worker);

        task.translate_event(AgentEvent::SdkMessage {
            session_id: "worker".to_owned(),
            msg: forge_primitives::Message::Error { error: "stream closed".to_owned() },
        });

        let (key, ..) =
            drained_notice(&mut update_rx).expect("a transport error still notifies the reviewer");
        assert_eq!(key, reviewer);
    }

    /// The session being replaced mid-turn (`/new`, `/clear`, an account
    /// switch) never produces a Result, so the drain has to happen here or
    /// the activity this turn touched sits until the task exits.
    #[tokio::test]
    async fn session_replacement_drains_review_activity() {
        let (_dir, workspace, reviewer, worker) = workspace_with_pending_review_activity();
        let (mut task, mut update_rx) = review_task_for(&workspace, &worker);
        task.connected_once = true;

        task.translate_event(connected_event("worker-replacement", "/tmp/proj"));

        let (key, ..) = drained_notice(&mut update_rx)
            .expect("the replaced identity flushes what its turn touched");
        assert_eq!(key, reviewer);
        assert!(
            workspace.drain_review_activity(&worker).is_empty(),
            "nothing strands under the worker's own slot",
        );
    }

    /// Teardown backstop: the subprocess dying or a despawn closing the
    /// command channel drops the task without any terminal envelope.
    #[tokio::test]
    async fn task_teardown_drains_review_activity() {
        let (_dir, workspace, reviewer, worker) = workspace_with_pending_review_activity();
        let (task, mut update_rx) = review_task_for(&workspace, &worker);

        drop(task);

        let (key, ..) =
            drained_notice(&mut update_rx).expect("teardown flushes the stranded activity");
        assert_eq!(key, reviewer);
    }

    /// `run` exiting its select loop reaches the drain, rather than only
    /// a hand-dropped task doing so.
    ///
    /// This does NOT model subprocess death. `Agent::testing_stub`
    /// substitutes an event channel whose sender is already dropped,
    /// which production never does - `SessionTask.handle` keeps an `Arc`
    /// chain to `BridgeInner.event_tx` alive for the task's whole life,
    /// so the event arm's `break` is unreachable there. The fixture also
    /// closes the command channel, so this test cannot say which arm
    /// broke; what it establishes is that leaving `run` drains.
    #[tokio::test]
    async fn run_exiting_on_a_closed_channel_drains_review_activity() {
        let (_dir, workspace, reviewer, worker) = workspace_with_pending_review_activity();
        let (task, mut update_rx) = review_task_for(&workspace, &worker);

        task.run().await;

        let (key, ..) =
            drained_notice(&mut update_rx).expect("the run loop's exit flushes the activity");
        assert_eq!(key, reviewer);
    }

    /// A `ConnectionFailed` event is terminal for the task: the dead
    /// spawn's pool entry and command sender are released so the next
    /// retry (projects-pane click, cron fire) re-spawns instead of
    /// dispatching into the dead handle, the TUI still receives the
    /// user-visible `SessionUpdate::ConnectionFailed`, and
    /// `translate_event` signals the run loop to exit so `Drop`'s
    /// expiry backstop fires.
    #[tokio::test]
    async fn connection_failed_releases_registrations_and_terminates() {
        let (_dir, workspace) = workspace_with_account_config_dir("/tmp/forge-testing-stub");
        let key = SessionSlot::from_str_for_test("dead-spawn");
        let (handle, _cmds) = Agent::testing_stub();
        let handle = Arc::new(handle);
        let (cmd_tx, command_rx) = mpsc::unbounded_channel();
        let (update_tx, mut update_rx) = mpsc::unbounded_channel();
        workspace.pool.lock().insert(
            key.clone(),
            crate::workspace::PooledAgent {
                handle: Arc::clone(&handle),
                account: forge_gateway::AccountKey("Acct".to_owned()),
                permission_mode: None,
                registration: None,
                session_id: "pooled-session".to_owned(),
            },
        );
        workspace.command_senders.lock().insert(key.clone(), cmd_tx);
        workspace.register_domain_session(key.clone(), Some(Arc::clone(&handle)));
        let mut task = SessionTask {
            key: key.clone(),
            handle,
            command_rx,
            domain: Arc::new(Mutex::new(DomainSession::new(key.clone(), None))),
            update_tx,
            connected_once: false,
            workspace: Arc::downgrade(&workspace),
        };

        let continues = task.translate_event(AgentEvent::ConnectionFailed {
            message: "spawn failed".to_owned(),
            kind: SpawnFailureKind::Unclassified,
        });

        assert!(!continues, "ConnectionFailed must terminate the task");
        assert!(
            !workspace.pool.lock().contains_key(&key),
            "pool entry released so a retry spawns fresh"
        );
        assert!(!workspace.command_senders.lock().contains_key(&key), "command sender released");
        let update = update_rx.try_recv().expect("an update emits");
        assert!(
            matches!(
                update,
                SessionUpdate::ConnectionFailed { key: ref emitted, .. } if *emitted == key
            ),
            "the TUI still receives the ConnectionFailed envelope"
        );
    }

    /// A `ConnectionFailed` releases the task's registrations, so a
    /// later click or delivery does not find a dead handle still
    /// registered under the key.
    #[tokio::test]
    async fn connection_failed_releases_the_session_registrations() {
        let (_dir, workspace) = workspace_with_account_config_dir("/tmp/forge-testing-stub");
        let key = SessionSlot::from_str_for_test("real-key");
        let (handle, _cmds) = Agent::testing_stub();
        let handle = Arc::new(handle);
        let (cmd_tx, command_rx) = mpsc::unbounded_channel();
        let (update_tx, _update_rx) = mpsc::unbounded_channel();
        workspace.pool.lock().insert(
            key.clone(),
            crate::workspace::PooledAgent {
                handle: Arc::clone(&handle),
                account: forge_gateway::AccountKey("Acct".to_owned()),
                permission_mode: None,
                registration: None,
                session_id: "pooled-session".to_owned(),
            },
        );
        workspace.command_senders.lock().insert(key.clone(), cmd_tx);
        let mut task = SessionTask {
            key: key.clone(),
            handle: Arc::clone(&handle),
            command_rx,
            domain: Arc::new(Mutex::new(DomainSession::new(key.clone(), None))),
            update_tx,
            connected_once: false,
            workspace: Arc::downgrade(&workspace),
        };

        let continues = task.translate_event(AgentEvent::ConnectionFailed {
            message: "spawn failed".to_owned(),
            kind: SpawnFailureKind::Unclassified,
        });

        assert!(!continues);
        assert!(
            !workspace.pool.lock().contains_key(&key),
            "pool entry under the real key released"
        );
        assert!(
            !workspace.command_senders.lock().contains_key(&key),
            "command sender under the real key released"
        );
    }

    /// A spawn that never connected strands whatever was parked for it, and
    /// the expiry reaches it by slot rather than by key: one bucket holds
    /// it, and the release below drops the domain it was once buffered on.
    /// For Slack the delivery was already committed, so nothing re-delivers
    /// it.
    #[tokio::test]
    async fn connection_failed_expires_the_buffers_parked_for_its_slot() {
        let (_dir, workspace) = workspace_with_account_config_dir("/tmp/forge-testing-stub");
        let key = test_slot();
        let (handle, _cmds) = Agent::testing_stub();
        let handle = Arc::new(handle);
        let (cmd_tx, command_rx) = mpsc::unbounded_channel();
        let (update_tx, _update_rx) = mpsc::unbounded_channel();
        workspace.pool.lock().insert(
            key.clone(),
            crate::workspace::PooledAgent {
                handle: Arc::clone(&handle),
                account: forge_gateway::AccountKey("Acct".to_owned()),
                permission_mode: None,
                registration: None,
                session_id: "pooled-session".to_owned(),
            },
        );
        workspace.command_senders.lock().insert(key.clone(), cmd_tx);
        workspace.register_domain_session(key.clone(), Some(Arc::clone(&handle)));
        workspace.park_slack(&test_slot(), vec![buffered_slack("parked while spawning")]);

        let mut task = SessionTask {
            key: key.clone(),
            handle: Arc::clone(&handle),
            command_rx,
            domain: Arc::new(Mutex::new(DomainSession::new(key.clone(), None))),
            update_tx,
            connected_once: false,
            workspace: Arc::downgrade(&workspace),
        };

        let continues = task.translate_event(AgentEvent::ConnectionFailed {
            message: "spawn failed".to_owned(),
            kind: SpawnFailureKind::Unclassified,
        });

        assert!(!continues);
        assert!(
            workspace.take_parked_for_slot(&test_slot()).slack.is_empty(),
            "the message parked for this slot is expired, not left for a later session",
        );
    }

    /// A `PermissionRequest` registers a pending slot; `RespondPermission`
    /// consumes it and the outcome reaches the agent round-trip. The
    /// happy path of the can_use_tool parking lot.
    #[tokio::test]
    async fn respond_permission_round_trips_to_the_agent() {
        let (_dir, workspace) = workspace_with_account_config_dir("/tmp/forge-testing-stub");
        let (handle, mut agent_rx) = Agent::testing_stub();
        let handle = Arc::new(handle);
        let key = SessionSlot::from_str_for_test("perm");
        let (_cmd_tx, command_rx) = mpsc::unbounded_channel();
        let (update_tx, _update_rx) = mpsc::unbounded_channel();
        let mut task = SessionTask {
            key: key.clone(),
            handle: Arc::clone(&handle),
            command_rx,
            domain: Arc::new(Mutex::new(DomainSession::new(
                key.clone(),
                Some(Arc::clone(&handle)),
            ))),
            update_tx,
            connected_once: false,
            workspace: Arc::downgrade(&workspace),
        };

        task.translate_event(AgentEvent::PermissionRequest {
            session_id: key.display(),
            request: permission_request_fixture("tu-1"),
        });
        assert!(
            task.domain.lock().pending_interactions.contains_key("tu-1"),
            "the request parks a pending permission slot"
        );

        task.execute_command(Command::RespondPermission {
            key: key.clone(),
            tool_id: "tu-1".to_owned(),
            outcome: forge_primitives::PermissionOutcome::Cancelled,
        });

        let cmd = tokio::time::timeout(std::time::Duration::from_secs(2), agent_rx.recv())
            .await
            .expect("the forwarded outcome reaches the agent promptly")
            .expect("command channel open");
        assert!(
            matches!(
                cmd,
                forge_primitives::AgentCommand::PermissionResponse { tool_call_id, .. }
                    if tool_call_id == "tu-1"
            ),
            "the outcome forwards to the bridge with the right tool id"
        );
        assert!(
            !task.domain.lock().pending_interactions.contains_key("tu-1"),
            "the slot is consumed"
        );
    }

    /// The cross-kind guard: `AskUserQuestion` reuses the can_use_tool
    /// wire, so a `RespondPermission` can arrive with a tool id whose
    /// slot is a Question. The mismatched outcome must be dropped and
    /// the REAL waiter (the question's oneshot) preserved.
    #[tokio::test]
    async fn respond_permission_against_a_question_slot_is_dropped() {
        let (_dir, workspace) = workspace_with_account_config_dir("/tmp/forge-testing-stub");
        let (handle, mut agent_rx) = Agent::testing_stub();
        let handle = Arc::new(handle);
        let key = SessionSlot::from_str_for_test("xkind");
        let (_cmd_tx, command_rx) = mpsc::unbounded_channel();
        let (update_tx, _update_rx) = mpsc::unbounded_channel();
        let mut task = SessionTask {
            key: key.clone(),
            handle: Arc::clone(&handle),
            command_rx,
            domain: Arc::new(Mutex::new(DomainSession::new(
                key.clone(),
                Some(Arc::clone(&handle)),
            ))),
            update_tx,
            connected_once: false,
            workspace: Arc::downgrade(&workspace),
        };

        task.translate_event(AgentEvent::QuestionRequest {
            session_id: key.display(),
            request: question_request_fixture("tu-q1"),
        });

        task.execute_command(Command::RespondPermission {
            key: key.clone(),
            tool_id: "tu-q1".to_owned(),
            outcome: forge_primitives::PermissionOutcome::Cancelled,
        });

        assert!(
            task.domain.lock().pending_interactions.contains_key("tu-q1"),
            "the question slot survives the mismatched permission response"
        );
        assert!(agent_rx.try_recv().is_err(), "nothing forwards to the agent on a kind mismatch");
    }

    fn permission_request_fixture(tool_id: &str) -> forge_primitives::PermissionRequest {
        forge_primitives::PermissionRequest {
            tool_call: forge_primitives::ToolCall {
                tool_call_id: tool_id.to_owned(),
                title: "Read".to_owned(),
                kind: forge_primitives::ToolKind::Read,
                status: forge_primitives::ToolCallStatus::Pending,
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
        }
    }

    fn question_request_fixture(tool_id: &str) -> forge_primitives::QuestionRequest {
        forge_primitives::QuestionRequest {
            tool_call: forge_primitives::ToolCall {
                tool_call_id: tool_id.to_owned(),
                title: "AskUserQuestion".to_owned(),
                kind: forge_primitives::ToolKind::Other,
                status: forge_primitives::ToolCallStatus::Pending,
                content: Vec::new(),
                raw_input: None,
                raw_output: None,
                output_metadata: None,
                task_metadata: None,
                locations: Vec::new(),
                meta: None,
            },
            prompt: forge_primitives::QuestionPrompt {
                question: "Which?".to_owned(),
                header: "Pick".to_owned(),
                multi_select: false,
                options: Vec::new(),
            },
            question_index: 0,
            total_questions: 1,
        }
    }

    /// The typed dispatch-failure events map onto their session-keyed
    /// envelopes: `SetModelFailed` (the /model rollback trigger) and
    /// `TurnError` (the committed-turn unwind).
    #[tokio::test]
    async fn set_model_failed_and_turn_error_map_to_keyed_updates() {
        let (_dir, workspace) = workspace_with_account_config_dir("/tmp/forge-testing-stub");
        let (mut task, mut update_rx) =
            review_task_for(&workspace, &SessionSlot::from_str_for_test("m"));

        task.translate_event(AgentEvent::SetModelFailed {
            session_id: "m".to_owned(),
            model: "claude-attempted".to_owned(),
            message: "not available".to_owned(),
        });
        assert!(
            matches!(
                update_rx.try_recv(),
                Ok(SessionUpdate::SetModelFailed { model, message, .. })
                    if model == "claude-attempted" && message == "not available"
            ),
            "SetModelFailed keeps model + message for the rollback reducer"
        );

        task.translate_event(AgentEvent::TurnError {
            session_id: "m".to_owned(),
            message: "stdin write failed".to_owned(),
        });
        assert!(
            matches!(
                update_rx.try_recv(),
                Ok(SessionUpdate::TurnError { message, .. }) if message == "stdin write failed"
            ),
            "TurnError carries the failure text so the spinner unwinds"
        );
    }

    /// The teardown drain must not double-notify a turn that already
    /// ended cleanly - every site shares one idempotent drain.
    #[tokio::test]
    async fn teardown_after_a_normal_turn_end_emits_nothing_further() {
        let (_dir, workspace, _reviewer, worker) = workspace_with_pending_review_activity();
        let (mut task, mut update_rx) = review_task_for(&workspace, &worker);

        task.translate_event(AgentEvent::SdkMessage {
            session_id: "worker".to_owned(),
            msg: result_message("success", false),
        });
        assert!(drained_notice(&mut update_rx).is_some(), "the turn's own notice fired");

        drop(task);

        assert!(drained_notice(&mut update_rx).is_none(), "teardown adds no second notice");
    }

    /// The store follows the id the CLI reports when it connects. A
    /// `/resume`, a `/clear`, a login or a logout move a session's id
    /// without forge choosing it, and a boot resolves a session from this
    /// row - so the row has to move with every Connected, not only the
    /// first.
    #[test]
    fn connected_records_the_id_the_cli_adopted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("solo");
        std::fs::create_dir_all(&root).expect("root");
        let forge_dir = crate::config::ensure_forge_data_dir(dir.path()).expect("forge dir");
        std::fs::write(
            forge_dir.join("forge.toml"),
            format!(
                r#"
[[orgs]]
name = "TestOrg"
accounts = ["Stargate"]
[[orgs.projects]]
name = "forge"
path = "{root}"
[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
                root = root.display()
            ),
        )
        .expect("write forge.toml");
        let workspace =
            Arc::new(crate::Workspace::new_for_test(dir.path().to_owned()).expect("workspace"));
        let key = SessionSlot::lead("TestOrg", "forge");
        workspace.seed_test_bound_session(&key, "Stargate");
        {
            let db = workspace.db.lock();
            let db = db.as_ref().expect("db");
            crate::store::sessions::put(
                db,
                &crate::store::sessions::SessionRecord {
                    org: "TestOrg".to_owned(),
                    project: "forge".to_owned(),
                    label: "lead".to_owned(),
                    session_id: None,
                    charter: None,
                    kick: None,
                    resume_kick: None,
                    interactive: None,
                    is_git_repo: None,
                },
            )
            .expect("seed the row");
        }

        let stored = || {
            let db = workspace.db.lock();
            let db = db.as_ref().expect("db");
            crate::store::sessions::get(db, "TestOrg", "forge", "lead")
                .expect("read")
                .and_then(|row| row.session_id)
        };

        let (mut task, _updates) = review_task_for(&workspace, &key);
        task.translate_event(connected_event("first-id", "/proj"));
        assert_eq!(
            stored().as_deref(),
            Some("first-id"),
            "the first Connected records the id the CLI adopted",
        );

        // `/resume`, `/clear`, `/login` and `/logout` all arrive as a
        // second Connected on this same task, which is the arm that used
        // to leave the row naming the session the user left.
        task.translate_event(connected_event("second-id", "/proj"));
        assert_eq!(
            stored().as_deref(),
            Some("second-id"),
            "so does a replacement, which is the id the next boot has to resume",
        );
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
                    supports_auto_mode: None,
                    supports_adaptive_thinking: None,
                    is_authoritative: true,
                },
                available_models: Vec::new(),
                mode: None,
                history_updates: None,
                compaction_count: 0,
            },
        );

        assert_eq!(
            domain.session_id.as_ref().map(std::string::ToString::to_string),
            Some("real-uuid-1".to_owned())
        );
    }

    /// `apply_event_to_domain` on `AgentEvent::ConnectionFailed`
    /// clears the runtime/turn mirrors: the subprocess is gone, so the
    /// in-flight guards must not read a stale "turn in flight".
    #[test]
    fn connection_failed_clears_domain_turn_state() {
        let mut domain = empty_domain();
        domain.runtime_state = Some(forge_primitives::RuntimeSessionState::Running);
        domain.turn_pending = true;

        apply_event_to_domain(
            &mut domain,
            &AgentEvent::ConnectionFailed {
                message: "reader died".to_owned(),
                kind: SpawnFailureKind::Unclassified,
            },
        );

        assert_eq!(domain.runtime_state, None, "runtime_state cleared on ConnectionFailed");
        assert!(!domain.turn_pending, "turn_pending cleared on ConnectionFailed");
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
                    supports_auto_mode: None,
                    supports_adaptive_thinking: None,
                    is_authoritative: true,
                },
                available_models: Vec::new(),
                mode: None,
                history_updates: None,
                compaction_count: 0,
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
            key: SessionSlot::from_str_for_test("old-uuid"),
            handle: Arc::new(handle),
            command_rx,
            domain: Arc::clone(&domain),
            update_tx,
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
                supports_auto_mode: None,
                supports_adaptive_thinking: None,
                is_authoritative: true,
            },
            available_models: Vec::new(),
            mode: None,
            history_updates: None,
            compaction_count: 0,
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

    /// First-Connected drains the slot's parked peer prompts in FIFO
    /// order, dispatching one `Command::Prompt` per parked entry, then
    /// leaves the bucket empty. Pinned via the workspace's
    /// command-intercept buffer so the full first-Connected branch of
    /// `translate_event` runs end-to-end (no poking the private drain
    /// method directly).
    #[tokio::test]
    async fn first_connected_drains_parked_peer_prompts_in_fifo_order() {
        use crate::mcp::peers::types::{CorrelationId, WrappedKind, WrappedPrompt};

        let (workspace, _update_rx) = crate::Workspace::testing_stub();

        // Park three Messages for the task's slot in known order.
        // Message kind (not Question) keeps the assertion focused on
        // FIFO dispatch; the Question-kind incoming-counter bump is
        // exercised separately.
        let session_key = test_slot();
        let domain =
            Arc::new(parking_lot::Mutex::new(DomainSession::new(session_key.clone(), None)));
        let bodies = ["first", "second", "third"];
        for body in bodies {
            workspace.park_peer_prompt(
                &session_key,
                WrappedPrompt {
                    correlation_id: CorrelationId::new_tell(),
                    kind: WrappedKind::Message,
                    sender_name: "forge".to_owned(),
                    sender_org: "Default".to_owned(),
                    body: body.to_owned(),
                },
            );
        }

        // connected_once=false → first-Connected arm that drains the
        // parked buckets. The drain resolves against the task's own slot,
        // so the test doesn't have to register against the workspace pool.
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
            connected_once: false,
            workspace: Arc::downgrade(&workspace),
        };

        workspace.enable_test_dispatch_intercept();
        task.translate_event(AgentEvent::Connected {
            session_id: session_key.display(),
            cwd: "/tmp/drain".to_owned(),
            current_model: forge_primitives::CurrentModel {
                resolved_id: "claude".to_owned(),
                display_name_short: "claude".to_owned(),
                display_name_long: "claude".to_owned(),
                requested_id: None,
                catalog_id: None,
                supports_effort: false,
                supported_effort_levels: Vec::new(),
                supports_auto_mode: None,
                supports_adaptive_thinking: None,
                is_authoritative: true,
            },
            available_models: Vec::new(),
            mode: None,
            history_updates: None,
            compaction_count: 0,
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
            workspace.take_parked_for_slot(&session_key).peer.is_empty(),
            "the parked peer prompts are drained after first-Connected"
        );
    }

    /// One Slack message, shaped as a delivery buffers it.
    fn buffered_slack(text: &str) -> forge_primitives::slack::SlackMessage {
        forge_primitives::slack::SlackMessage {
            workspace: "acme".to_owned(),
            conversation: "D1".to_owned(),
            conversation_label: "U9".to_owned(),
            ts: "100.000001".to_owned(),
            thread_ts: None,
            user: Some("U9".to_owned()),
            author: None,
            text: text.to_owned(),
            parent_user_id: None,
            latest_reply: None,
            files: Vec::new(),
        }
    }

    fn connected_event(session_id: &str, cwd: &str) -> AgentEvent {
        AgentEvent::Connected {
            session_id: session_id.to_owned(),
            cwd: cwd.to_owned(),
            current_model: forge_primitives::CurrentModel {
                resolved_id: "claude".to_owned(),
                display_name_short: "claude".to_owned(),
                display_name_long: "claude".to_owned(),
                requested_id: None,
                catalog_id: None,
                supports_effort: false,
                supported_effort_levels: Vec::new(),
                supports_auto_mode: None,
                supports_adaptive_thinking: None,
                is_authoritative: true,
            },
            available_models: Vec::new(),
            mode: None,
            history_updates: None,
            compaction_count: 0,
        }
    }

    fn cron_worker_entry(
        label: &str,
        slot: crate::SessionSlot,
    ) -> crate::mcp::workers::types::WorkerEntry {
        crate::mcp::workers::types::WorkerEntry {
            label: label.to_owned(),
            charter: "c".to_owned(),
            spawned_by: crate::SessionSlot::lead(slot.org(), slot.project()),
            slot,
            session_id: None,
            status: forge_primitives::WorkerLiveness::Running,
            spawned_at: std::time::SystemTime::UNIX_EPOCH,
            needs_tag: false,
            is_git_repo_at_spawn: false,
            diagnostic: None,
            kick: None,
        }
    }

    /// First-Connected drains the session slot's buffered cron prompts:
    /// each dispatches a plain `Command::Prompt` AND echoes a
    /// `CronPromptAppended` so an asleep-fired cron shows its block once the
    /// session connects (mirrors the gotify drain echo). Reproduce-first:
    /// the echo is absent until the drain calls `push_cron_prompt_into_chat`.
    #[tokio::test]
    async fn first_connected_drains_pending_cron_prompts_and_echoes_block() {
        let (workspace, mut update_rx) = crate::Workspace::testing_stub();
        workspace.seed_test_project("cron-drain", "/tmp/cron-drain");
        // The slot production derives for the seeded project, and the
        // bucket the drain reads.
        let session_key = SessionSlot::lead("TestOrg", "cron-drain");
        workspace.park_cron(&session_key, "morning reminder".to_owned(), false);

        let domain =
            Arc::new(parking_lot::Mutex::new(DomainSession::new(session_key.clone(), None)));

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
            connected_once: false,
            workspace: Arc::downgrade(&workspace),
        };

        workspace.enable_test_dispatch_intercept();
        task.translate_event(connected_event(&session_key.display(), "/tmp/cron-drain"));

        // The buffered cron prompt is dispatched as a plain user turn.
        let buffered = workspace.drain_test_dispatch_buffer();
        assert!(
            buffered.iter().any(|c| matches!(
                c, crate::protocol::Command::Prompt { text, .. } if text == "morning reminder"
            )),
            "the buffered cron prompt is dispatched on first-Connected",
        );

        // AND an echo lands so the drained prompt shows a cron block.
        let mut echoed = false;
        while let Ok(u) = update_rx.try_recv() {
            if matches!(
                u,
                SessionUpdate::CronPromptAppended { key, text }
                    if key == session_key && text == "morning reminder"
            ) {
                echoed = true;
            }
        }
        assert!(echoed, "an asleep-fired cron echoes a CronPromptAppended on drain");

        assert!(
            workspace.take_parked_for_slot(&session_key).cron.is_empty(),
            "the slot's cron bucket is drained after first-Connected",
        );
    }

    /// A worker session drains only its OWN `(project, label)` cron bucket
    /// on first Connect - the lead's bucket stays buffered - and a missed
    /// entry's `[missed cron] ` marker survives the buffer -> drain into the
    /// dispatched prompt (the asleep/drain missed path, distinct from the
    /// live-fire path).
    #[tokio::test]
    async fn worker_first_connected_drains_its_own_bucket_with_missed_marker() {
        let (workspace, _rx) = crate::Workspace::testing_stub();
        workspace.seed_test_project("wdp", "/tmp/wdp");
        let key =
            workspace.list_projects().into_iter().find(|v| v.name == "wdp").expect("view").key;

        // A missed cron for the worker + an on-time lead cron for the project.
        let worker_slot = crate::SessionSlot::worker("TestOrg", "wdp", "reviewer");
        let lead_slot = crate::SessionSlot::lead("TestOrg", "wdp");
        workspace.insert_live_worker(&key, cron_worker_entry("reviewer", worker_slot.clone()));
        workspace.park_cron(&worker_slot, "worker work".to_owned(), true);
        workspace.park_cron(&lead_slot, "lead work".to_owned(), false);

        let session_key = worker_slot.clone();
        let domain =
            Arc::new(parking_lot::Mutex::new(DomainSession::new(session_key.clone(), None)));
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
            connected_once: false,
            workspace: Arc::downgrade(&workspace),
        };

        workspace.enable_test_dispatch_intercept();
        task.translate_event(connected_event(&worker_slot.display(), "/tmp/wdp"));

        let dispatched = workspace.drain_test_dispatch_buffer();
        assert!(
            dispatched.iter().any(|c| matches!(
                c, crate::protocol::Command::Prompt { key, text, .. }
                    if *key == session_key && text == "[missed cron] worker work"
            )),
            "the worker drains its own missed cron with the marker applied",
        );
        // The lead's bucket is untouched by the worker's drain.
        let lead_bucket = workspace.take_parked_for_slot(&lead_slot).cron;
        assert_eq!(lead_bucket.len(), 1, "the lead's cron stays buffered");
        assert_eq!(lead_bucket[0].text, "lead work");
    }

    /// The count has coverage at both ends - the scan produces it, the
    /// TUI seeds from it - and this is the layer in between. Both emitted
    /// arms are checked because a task that has connected before emits
    /// `SessionReplaced` instead of `Connected`, and forcing the field to
    /// zero on either one is invisible to every other test here.
    #[tokio::test]
    async fn translate_event_carries_a_non_zero_compaction_count_on_both_arms() {
        for (connected_once, arm) in [(false, "Connected"), (true, "SessionReplaced")] {
            let (workspace, mut update_rx) = crate::Workspace::testing_stub();
            let session_key = SessionSlot::from_str_for_test("count-through-uuid");
            let domain =
                Arc::new(parking_lot::Mutex::new(DomainSession::new(session_key.clone(), None)));
            let (handle, _agent_cmd_rx) = Agent::testing_stub();
            let (_cmd_tx, command_rx) =
                tokio::sync::mpsc::unbounded_channel::<crate::protocol::Command>();
            let mut task = SessionTask {
                key: session_key.clone(),
                handle: Arc::new(handle),
                command_rx,
                domain: Arc::clone(&domain),
                update_tx: workspace.update_sender(),
                connected_once,
                workspace: Arc::downgrade(&workspace),
            };

            let mut event = connected_event(&session_key.display(), "/tmp/count");
            if let AgentEvent::Connected { compaction_count, .. } = &mut event {
                *compaction_count = 7;
            }
            task.translate_event(event);

            let mut seen = None;
            while let Ok(u) = update_rx.try_recv() {
                match u {
                    SessionUpdate::Connected { compaction_count, .. }
                    | SessionUpdate::SessionReplaced { compaction_count, .. } => {
                        seen = Some(compaction_count);
                    }
                    _ => {}
                }
            }
            assert_eq!(seen, Some(7), "{arm} must carry the count through unchanged");
        }
    }

    /// A re-spawn that replaces the session seeds `connected_once =
    /// true`, so the new task's first Connected emits `SessionReplaced`
    /// (not a fresh Connected) carrying the resumed history. The TUI
    /// reducer resets the chat then re-seeds it from that history, so
    /// the same conversation stays visible across the replacement.
    #[tokio::test]
    async fn connected_once_seed_emits_session_replaced_with_resumed_history() {
        use forge_primitives::Message;

        let (workspace, mut update_rx) = crate::Workspace::testing_stub();
        let session_key = SessionSlot::from_str_for_test("replacement-visible-uuid");
        let domain =
            Arc::new(parking_lot::Mutex::new(DomainSession::new(session_key.clone(), None)));

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
            // The seed a session-replacing re-spawn installs.
            connected_once: true,
            workspace: Arc::downgrade(&workspace),
        };

        // A one-message resumed history (the --resume backfill).
        let history = vec![Message::System {
            subtype: "info".to_owned(),
            session_id: Some(session_key.display()),
            data: serde_json::json!({ "body": "earlier turn" }),
        }];

        task.translate_event(AgentEvent::Connected {
            session_id: session_key.display(),
            cwd: "/tmp/respawn".to_owned(),
            current_model: forge_primitives::CurrentModel {
                resolved_id: "claude".to_owned(),
                display_name_short: "claude".to_owned(),
                display_name_long: "claude".to_owned(),
                requested_id: None,
                catalog_id: None,
                supports_effort: false,
                supported_effort_levels: Vec::new(),
                supports_auto_mode: None,
                supports_adaptive_thinking: None,
                is_authoritative: true,
            },
            available_models: Vec::new(),
            mode: None,
            history_updates: Some(history),
            compaction_count: 0,
        });

        let mut replaced_history_len = None;
        let mut saw_plain_connected = false;
        while let Ok(u) = update_rx.try_recv() {
            match u {
                SessionUpdate::SessionReplaced { key, history, .. } => {
                    assert_eq!(key, session_key, "SessionReplaced targets the re-spawned session");
                    replaced_history_len = Some(history.len());
                }
                SessionUpdate::Connected { .. } => saw_plain_connected = true,
                _ => {}
            }
        }
        assert_eq!(
            replaced_history_len,
            Some(1),
            "connected_once=true emits SessionReplaced carrying the resumed conversation",
        );
        assert!(
            !saw_plain_connected,
            "a session-replacing re-spawn must not emit a fresh Connected"
        );
    }

    /// The replaced identity's `/dictate` override axes die with it:
    /// the TUI mints a blank bucket for the new session, so an axis
    /// left on the domain would style a session that no longer exists.
    /// The device pick is workspace state shared by every session, so
    /// the replacement must leave it - and echo nothing.
    #[tokio::test]
    async fn a_replaced_identity_drops_its_overrides_but_not_the_device_pick() {
        let (workspace, mut update_rx) = crate::Workspace::testing_stub();
        let session_key = SessionSlot::from_str_for_test("dictate-rekey-uuid");
        let domain =
            Arc::new(parking_lot::Mutex::new(DomainSession::new(session_key.clone(), None)));
        domain.lock().dictate_overrides.styling = Some(forge_dictate::normalize::Styling::Formal);
        *workspace.dictate_device_pick.lock() =
            Some(crate::dictate::DictateDeviceChoice::Device("shure-id".into()));

        let (handle, _agent_cmd_rx) = Agent::testing_stub();
        let (_cmd_tx, command_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::protocol::Command>();
        let mut task = SessionTask {
            key: session_key.clone(),
            handle: Arc::new(handle),
            command_rx,
            domain: Arc::clone(&domain),
            update_tx: workspace.update_sender(),
            connected_once: true,
            workspace: Arc::downgrade(&workspace),
        };

        task.translate_event(AgentEvent::Connected {
            session_id: session_key.display(),
            cwd: "/tmp/respawn".to_owned(),
            current_model: forge_primitives::CurrentModel {
                resolved_id: "claude".to_owned(),
                display_name_short: "claude".to_owned(),
                display_name_long: "claude".to_owned(),
                requested_id: None,
                catalog_id: None,
                supports_effort: false,
                supported_effort_levels: Vec::new(),
                supports_auto_mode: None,
                supports_adaptive_thinking: None,
                is_authoritative: true,
            },
            available_models: Vec::new(),
            mode: None,
            history_updates: None,
            compaction_count: 0,
        });

        assert_eq!(domain.lock().dictate_overrides, crate::dictate::DictateOverrides::default());
        assert_eq!(
            *workspace.dictate_device_pick.lock(),
            Some(crate::dictate::DictateDeviceChoice::Device("shure-id".into())),
            "the pick is workspace state: a session replacement must not clear it"
        );

        let mut replaced_seen = false;
        while let Ok(u) = update_rx.try_recv() {
            if let SessionUpdate::DictateDevicePin { .. } = u {
                panic!("a replacement must not echo a device-pin clear: {u:?}");
            }
            replaced_seen |= matches!(u, SessionUpdate::SessionReplaced { .. });
        }
        assert!(
            replaced_seen,
            "the scenario must still be a replacement, or the test proves nothing"
        );
    }

    /// A session-replacing re-spawn tears the live session down BEFORE
    /// re-spawning, so if the re-spawned agent fails to connect the
    /// session is momentarily agent-less. That failure must be
    /// recoverable (`ConnectionFailed { fatal: false }`), and the task's
    /// exit must leave no lingering pooled agent under the key. (The
    /// synchronous `get_agent_handle` Err arm is defensive/near-
    /// unreachable - `Agent::spawn` is infallible - so this covers the
    /// realistic async failure path instead.)
    #[tokio::test]
    async fn respawn_connection_failure_is_nonfatal_and_releases_the_session() {
        let (workspace, mut update_rx) = crate::Workspace::testing_stub();
        let key = SessionSlot::from_str_for_test("respawn-fail-uuid");

        let (handle, _agent_cmds) = Agent::testing_stub();
        let arc = Arc::new(handle);
        // Register the re-spawned session under `key` with `arc` pooled -
        // the SAME Arc the task holds, so the exit cleanup recognises it.
        workspace.pool.lock().insert(
            key.clone(),
            crate::workspace::PooledAgent {
                handle: Arc::clone(&arc),
                account: forge_gateway::AccountKey("test".to_owned()),
                permission_mode: None,
                registration: None,
                session_id: "pooled-session".to_owned(),
            },
        );
        let domain = workspace.register_domain_session(key.clone(), Some(Arc::clone(&arc)));
        let (_cmd_tx, command_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::protocol::Command>();

        let mut task = SessionTask {
            key: key.clone(),
            handle: arc,
            command_rx,
            domain,
            update_tx: workspace.update_sender(),
            connected_once: true, // a session-replacing re-spawn
            workspace: Arc::downgrade(&workspace),
        };

        // The re-spawned agent fails to connect.
        task.translate_event(AgentEvent::ConnectionFailed {
            message: "spawn failed".to_owned(),
            kind: SpawnFailureKind::Unclassified,
        });

        let mut saw_nonfatal = false;
        while let Ok(update) = update_rx.try_recv() {
            if let SessionUpdate::ConnectionFailed { key: failed_key, fatal, .. } = update {
                assert!(!fatal, "a failed re-spawn is recoverable, not fatal");
                assert_eq!(failed_key, key);
                saw_nonfatal = true;
            }
        }
        assert!(saw_nonfatal, "the failed re-spawn emits ConnectionFailed with fatal=false");

        // The dead spawn's registrations are released in the arm itself,
        // before the task exits - no lingering agent under the key.
        assert!(
            !workspace.pool.lock().contains_key(&key),
            "a failed re-spawn leaves no lingering pooled agent",
        );
    }

    /// The named failure kind has to survive the join. `translate_event`
    /// is the only production site that hands it to the worker-failure
    /// handler, and the default there would put the message heuristic back
    /// in charge: a `CwdNotFound` renders the directory it could not
    /// enter, and for a worker that directory runs through
    /// `.claude/worktrees/<label>`, so the row would be deleted and the
    /// lead told the worktree could not be created.
    #[tokio::test]
    async fn a_named_spawn_failure_kind_reaches_the_worker_handler() {
        let (workspace, _update_rx) = crate::Workspace::testing_stub();
        workspace.enable_test_dispatch_intercept();
        let db_dir = tempfile::tempdir().expect("tempdir");
        workspace.install_db_for_test(
            crate::store::Db::open(&db_dir.path().join("db.redb")).expect("open db"),
        );
        let project_dir = tempfile::tempdir().expect("project dir");
        workspace.seed_test_project("proj-x", &project_dir.path().to_string_lossy());
        let project_key = workspace.project_key_for_name("proj-x").expect("seeded project");
        // The row a worktree-creation verdict would delete.
        workspace
            .record_worker_row(
                &project_key,
                "reviewer",
                "reviewer-uuid",
                "c",
                None,
                None,
                false,
                true,
            )
            .expect("seed the worker's row");

        let worker_slot = SessionSlot::from_str_for_test("worker-uuid");
        workspace.insert_live_worker(
            &project_key,
            crate::mcp::workers::types::WorkerEntry {
                label: "reviewer".to_owned(),
                charter: "c".to_owned(),
                slot: worker_slot.clone(),
                session_id: Some(forge_primitives::SessionId::new("reviewer-uuid")),
                status: forge_primitives::WorkerLiveness::Spawning,
                spawned_at: std::time::SystemTime::UNIX_EPOCH,
                spawned_by: SessionSlot::from_str_for_test("lead-uuid"),
                needs_tag: false,
                is_git_repo_at_spawn: true,
                diagnostic: None,
                kick: None,
            },
        );

        let (handle, _agent_cmds) = Agent::testing_stub();
        let arc = Arc::new(handle);
        let domain = workspace.register_domain_session(worker_slot.clone(), Some(Arc::clone(&arc)));
        let (_cmd_tx, command_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::protocol::Command>();
        let mut task = SessionTask {
            key: worker_slot.clone(),
            handle: arc,
            command_rx,
            domain,
            update_tx: workspace.update_sender(),
            connected_once: false,
            workspace: Arc::downgrade(&workspace),
        };

        task.translate_event(AgentEvent::ConnectionFailed {
            message: "forge-sdk session resume failed: claude subprocess working directory \
                      `/x/.claude/worktrees/reviewer` does not exist"
                .to_owned(),
            kind: SpawnFailureKind::WorkingDirMissing,
        });

        let rows = workspace.worker_rows_for_project(&project_key);
        assert!(
            rows.iter().any(|row| row.label == "reviewer"),
            "a named missing-working-directory failure keeps the worker's row rather than \
             deleting it as a worktree that could not be created; rows left {:?}",
            rows.iter().map(|row| row.label.clone()).collect::<Vec<_>>(),
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
        let key = SessionSlot::from_str_for_test("sess");
        execute_command_via_handle(
            &handle,
            &key,
            Some("sess-1"),
            None,
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
        let key = SessionSlot::from_str_for_test("sess");
        execute_command_via_handle(
            &handle,
            &key,
            Some("sess-1"),
            None,
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
        let key = SessionSlot::from_str_for_test("sess");
        execute_command_via_handle(
            &handle,
            &key,
            Some("sess-1"),
            None,
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
        let key = SessionSlot::from_str_for_test("sess");
        execute_command_via_handle(
            &handle,
            &key,
            Some("sess-1"),
            None,
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
        let key = SessionSlot::from_str_for_test("sess");
        let err = execute_command_via_handle(
            &handle,
            &key,
            None,
            None,
            Command::Cancel { key: key.clone() },
        )
        .expect_err("a no-session dispatch reports the drop, not Ok");
        assert!(err.to_string().contains("no active session"), "the error names the drop: {err}");
        // Nothing should have been queued.
        assert!(rx.try_recv().is_err());
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod connected_hook_tests {
    use super::*;
    use crate::Workspace;
    use crate::protocol::Command;
    use crate::target::ProjectKey;

    /// A project's lead slot: the label is what marks the role, so a
    /// project whose name looks worker-shaped is still a lead.
    fn lead_slot(project_name: &str) -> crate::SessionSlot {
        crate::SessionSlot::lead("TestOrg", project_name)
    }

    /// Seed `proj-x` with one persisted worker row and return the
    /// tempdir backing the store, whose lifetime must outlive the test.
    ///
    /// The project path is a real directory: a non-git worker runs in the
    /// project root, so the wave checks that root is there before
    /// re-spawning it.
    fn seed_project_with_one_worker_row(
        workspace: &Arc<Workspace>,
        label: &str,
    ) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        workspace.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        let project_path = dir.path().join("proj-x");
        std::fs::create_dir_all(&project_path).expect("create the project dir");
        let project_path = project_path.to_string_lossy().into_owned();
        workspace.seed_test_project("proj-x", &project_path);
        let project_key = ProjectKey::new(
            forge_agent::userdata::catalog::scan::project_key_for_directory(Some(&project_path)),
        );
        workspace
            .record_worker_row(
                &project_key,
                label,
                &format!("{label}-test-id"),
                &format!("charter for {label}"),
                None,
                None,
                false,
                false,
            )
            .expect("seed the worker row");
        dir
    }

    /// A lead Connected for a project with a persisted worker triggers
    /// one `Command::SpawnWorker` per row.
    #[test]
    fn lead_connected_with_a_worker_row_triggers_respawn() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        let _db = seed_project_with_one_worker_row(&workspace, "implementer");
        workspace.enable_test_dispatch_intercept();

        on_connected_for_test(&workspace, &lead_slot("proj-x"), "lead-uuid");

        let dispatched = workspace.drain_test_dispatch_buffer();
        let spawns: Vec<&Command> =
            dispatched.iter().filter(|c| matches!(c, Command::SpawnWorker { .. })).collect();
        assert_eq!(spawns.len(), 1, "one SpawnWorker per stored worker");
    }

    /// A lead Connected for a project with no persisted workers is
    /// a no-op - nothing dispatched.
    ///
    /// UNTESTED: this cannot tell the empty-set early return from
    /// falling through and iterating an empty slice, and deleting the
    /// store setup below passes too. What is hard is not the harness -
    /// `workspace::worker_respawn_tests` already has one - but that
    /// observing the skipped scan means asserting on private state or
    /// racing the spawned task. Recorded so the gap stays findable.
    #[test]
    fn lead_connected_without_a_worker_row_does_nothing() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        let dir = tempfile::tempdir().expect("tempdir");
        workspace.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        workspace.enable_test_dispatch_intercept();
        workspace.seed_test_project("proj-y", "/tmp/proj-y");

        on_connected_for_test(&workspace, &lead_slot("proj-y"), "lead-uuid");

        assert!(workspace.drain_test_dispatch_buffer().is_empty());
    }

    /// A worker's Connected does NOT trigger the respawn hook - the
    /// slot's label is what stops it.
    ///
    /// The project's name is the one a key-shaped parse would have read
    /// as a worker, so a reintroduced parse would classify the project
    /// itself and the hook would have to be stopped by the label alone.
    /// A persisted row is necessary for this to have teeth and is not
    /// sufficient.
    #[test]
    fn worker_connected_does_not_trigger_respawn() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        let dir = tempfile::tempdir().expect("tempdir");
        workspace.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        // A project named like the worker key shape the old parse read.
        let lookalike = "worker_wp_planner_abc";
        workspace.seed_test_project(lookalike, "/tmp/wp");
        let project_key = ProjectKey::new(
            forge_agent::userdata::catalog::scan::project_key_for_directory(Some("/tmp/wp")),
        );
        workspace
            .record_worker_row(
                &project_key,
                "planner",
                "planner-test-id",
                "charter for planner",
                None,
                None,
                false,
                false,
            )
            .expect("seed the worker row");
        workspace.enable_test_dispatch_intercept();

        let worker = crate::SessionSlot::worker("TestOrg", lookalike, "planner");
        on_connected_for_test(&workspace, &worker, "worker-uuid");

        let dispatched = workspace.drain_test_dispatch_buffer();
        assert!(
            dispatched.iter().all(|c| !matches!(c, Command::SpawnWorker { .. })),
            "a worker's own Connected does not respawn the team",
        );
    }

    /// Idempotency: a second Connected event for the same lead must
    /// not double-spawn. The first call inserts WorkerEntries into
    /// `live_workers`; the second call's `list_live_workers(...).is_empty()`
    /// gate trips and the trigger no-ops.
    #[test]
    fn second_lead_connected_does_not_double_spawn() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        let _db = seed_project_with_one_worker_row(&workspace, "implementer");
        workspace.enable_test_dispatch_intercept();

        let lead = lead_slot("proj-x");

        // First Connected: triggers the respawn (1 SpawnWorker).
        on_connected_for_test(&workspace, &lead, "lead-uuid");
        let after_first = workspace.drain_test_dispatch_buffer();
        let first_spawns: usize =
            after_first.iter().filter(|c| matches!(c, Command::SpawnWorker { .. })).count();
        assert_eq!(first_spawns, 1, "first Connected respawns the workers");

        // Simulate the workers having become live (the production
        // flow does this via `handle_spawn_worker`'s
        // `insert_live_worker`; the test intercept skipped that
        // path so we seed it manually for the idempotency gate).
        let project_key = workspace.project_key_for_name("proj-x").expect("seeded project");
        workspace.insert_live_worker(
            &project_key,
            crate::mcp::workers::types::WorkerEntry {
                label: "implementer".into(),
                charter: "test".into(),
                slot: SessionSlot::from_str_for_test("worker-uuid"),
                session_id: None,
                status: forge_primitives::WorkerLiveness::Running,
                spawned_at: std::time::SystemTime::UNIX_EPOCH,
                spawned_by: SessionSlot::from_str_for_test("lead-uuid"),
                needs_tag: false,
                is_git_repo_at_spawn: false,
                diagnostic: None,
                kick: None,
            },
        );

        // Second Connected: gate trips, no new SpawnWorker.
        on_connected_for_test(&workspace, &lead, "lead-uuid");
        let after_second = workspace.drain_test_dispatch_buffer();
        let second_spawns: usize =
            after_second.iter().filter(|c| matches!(c, Command::SpawnWorker { .. })).count();
        assert_eq!(second_spawns, 0, "second Connected must not double-spawn");
    }

    /// Helper: insert a live ad-hoc worker carrying `kick` under the
    /// seeded project, returning its slot for `on_connected_for_test`.
    #[cfg(test)]
    fn seed_adhoc_worker_with_kick(
        workspace: &Arc<Workspace>,
        label: &str,
        kick: Option<String>,
    ) -> crate::SessionSlot {
        workspace.seed_test_project("forge", "/tmp/forge");
        let project_key = workspace
            .list_projects()
            .into_iter()
            .find(|v| v.name == "forge")
            .expect("seeded project present")
            .key;
        let slot = crate::SessionSlot::worker("TestOrg", "forge", label);
        workspace.insert_live_worker(
            &project_key,
            crate::mcp::workers::types::WorkerEntry {
                label: label.to_owned(),
                charter: "ad-hoc".into(),
                slot: slot.clone(),
                session_id: None,
                status: forge_primitives::WorkerLiveness::Running,
                spawned_at: std::time::SystemTime::UNIX_EPOCH,
                spawned_by: crate::SessionSlot::lead("TestOrg", "forge"),
                needs_tag: false,
                is_git_repo_at_spawn: false,
                diagnostic: None,
                kick,
            },
        );
        slot
    }

    /// A worker spawned with `agents__spawn(kick=...)` gets that kick
    /// delivered as its first turn, verbatim, through the rate-limited
    /// dispatcher.
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
            assert_eq!(key, &synth, "kick targets the worker's own slot");
            assert_eq!(
                text, "Begin: triage the failing test now.",
                "inline kick delivered verbatim",
            );
        }
    }

    /// #695: the same inline kick with an underscore in the label as the
    /// only variable. `worker_with_inline_kick_dispatches_it_as_first_turn`
    /// above is the control - same fixture, same seed, plain label - which
    /// is what makes a failure here mean the label rather than the harness.
    #[tokio::test(start_paused = true)]
    async fn worker_with_an_underscore_label_dispatches_its_kick() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        workspace.enable_test_dispatch_intercept();
        workspace.start_kick_dispatcher();
        let synth = seed_adhoc_worker_with_kick(
            &workspace,
            "code_review",
            Some("Begin: review the open diff.".into()),
        );

        on_connected_for_test(&workspace, &synth, "worker-uuid");
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        let dispatched = workspace.drain_test_dispatch_buffer();
        let prompts: Vec<&Command> =
            dispatched.iter().filter(|c| matches!(c, Command::Prompt { .. })).collect();
        assert_eq!(prompts.len(), 1, "an underscore-labelled worker gets its kick");
        if let Command::Prompt { text, .. } = prompts[0] {
            assert_eq!(text, "Begin: review the open diff.", "the kick arrives verbatim");
        }
    }

    /// A live worker whose entry carries no kick gets none - it idles
    /// until the lead sends an agents__tell.
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
        assert!(prompts.is_empty(), "a live entry carrying no kick gets none");
    }

    /// A label with no live `WorkerEntry` of its own gets no kick, even
    /// when the project has a live worker holding one. The other entry
    /// is what gives this teeth: with an empty project the lookup has
    /// nothing to wrongly return. The drainer has to be running too -
    /// without it a mis-delivered kick only reaches the queue, and the
    /// dispatch buffer stays empty for the wrong reason.
    #[tokio::test(start_paused = true)]
    async fn worker_connected_for_an_unmatched_label_does_not_take_another_workers_kick() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        workspace.enable_test_dispatch_intercept();
        workspace.start_kick_dispatcher();
        // One live worker, carrying a kick, under a DIFFERENT label.
        seed_adhoc_worker_with_kick(&workspace, "other", Some("not yours".into()));
        // A label with no entry of its own, in the same project.
        let other_label = crate::SessionSlot::worker("TestOrg", "forge", "scratchpad");

        on_connected_for_test(&workspace, &other_label, "worker-uuid");
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        let dispatched = workspace.drain_test_dispatch_buffer();
        assert!(
            dispatched.iter().all(|c| !matches!(c, Command::Prompt { .. })),
            "a label with no entry of its own must not be handed another worker's kick",
        );
    }

    /// The guard directly, with no preconditions to decay: a lead key
    /// A label may contain underscores, and the kick hook matches on the
    /// label as a whole string rather than on any parse of it.
    #[tokio::test(start_paused = true)]
    async fn a_worker_label_with_an_underscore_keeps_its_kick() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        workspace.enable_test_dispatch_intercept();
        workspace.start_kick_dispatcher();
        let slot = seed_adhoc_worker_with_kick(&workspace, "code_review", Some("go".into()));

        on_connected_for_test(&workspace, &slot, "worker-uuid");
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        let dispatched = workspace.drain_test_dispatch_buffer();
        assert!(
            dispatched.iter().any(|c| matches!(
                c, Command::Prompt { text, .. } if text == "go"
            )),
            "an underscore in the label must not cost the kick: {dispatched:?}",
        );
    }
}
