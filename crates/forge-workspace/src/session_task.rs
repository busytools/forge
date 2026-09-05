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

use crate::account::{AccountKey, UsageFetchStatus};
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
    /// The account this session spawned under. A live 429 rotates off
    /// THIS key: several accounts can share one config dir, so a
    /// reverse lookup from the dir would mark an arbitrary one. `None`
    /// off-plan; such a session cannot rotate and says so.
    pub(crate) account: Option<AccountKey>,
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

/// Server hold-down carried by a rate-limit signal, when the wire
/// provides one.
struct RateLimitHit {
    retry_after: Option<std::time::Duration>,
}

/// Detect a definitive account-rate-limit from an inbound SDK message.
/// The exact 429 status and the `RateLimit` error enum both ride the
/// `api_retry` system message, with `retry_delay_ms` as the hold-down;
/// the typed enum covers a rate-limit classified without a numeric
/// status.
fn rate_limit_hit_from_message(msg: &forge_primitives::Message) -> Option<RateLimitHit> {
    use forge_primitives::{ApiRetryError, Message};
    let Message::System { subtype, data, .. } = msg else {
        return None;
    };
    if subtype != "api_retry" {
        return None;
    }
    let update = forge_agent::translate::state_parsing::build_api_retry_update(data.as_object()?)?;
    let rate_limited = update.error_status == Some(429) || update.error == ApiRetryError::RateLimit;
    rate_limited.then(|| RateLimitHit {
        retry_after: Some(std::time::Duration::from_millis(update.retry_delay_ms)),
    })
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
                    let event = self.merge_catalog_models(event).await;
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
            key = %self.key.as_str(),
            "session task exiting (agent event channel closed)"
        );
        // The agent-event / command channel closed - this session's
        // subprocess is gone. Release it from the pool + command_senders
        // so a later cron fire / projects-pane click sees it as
        // not-running and re-spawns cleanly, instead of dispatching a
        // `Command::Prompt` to the now-closed channel (which fails with
        // `SessionClosed` and is silently dropped, quietly stopping
        // durable crons for the project). Guarded on handle identity so
        // a superseded task (its session re-spawned under the same key by
        // an `/account` switch) doesn't wipe its successor's live entries.
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

    /// Swap an OpenRouter session's discovered `available_models` for
    /// the curated catalog list before `translate_event` sees the
    /// `Connected` event (covering both the `Connected` and
    /// `SessionReplaced` emits). Awaited in the async run loop so
    /// `translate_event` itself stays synchronous; on a cold cache the
    /// inline fetch delays this session's events once per base url -
    /// the failure marker written on a miss means every connect after
    /// that serves from the cache or the discovered list.
    async fn merge_catalog_models(&self, event: AgentEvent) -> AgentEvent {
        let AgentEvent::Connected {
            session_id,
            cwd,
            current_model,
            available_models,
            mode,
            history_updates,
            compaction_count,
        } = event
        else {
            return event;
        };
        let available_models = match self.workspace.upgrade() {
            Some(workspace) => match self.handle.display_name() {
                Some(display_name) => {
                    workspace.catalog_available_models(&display_name, available_models).await
                }
                None => available_models,
            },
            None => available_models,
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

    /// Translate one `AgentEvent` into the matching `SessionUpdate`
    /// (or pair of updates for `Connected`, which also emits
    /// `KeyRenamed` if a synthetic spawn key is pending). Updates
    /// `DomainSession` in-place before each emit.
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
                compaction_count,
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
                // TODO(ved): lead sessions are currently never tagged
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
                    // Same reason, same ordering: the replaced identity
                    // never produces a Result, and once `rekey_to` moves
                    // `DomainSession.key` no later drain can name the
                    // buffer this turn's review actions sit in.
                    let caller = self.domain.lock().key.clone();
                    self.drain_review_activity_for(&caller);
                    // Session-scoped `/dictate` state dies with the
                    // identity too: the TUI mints a blank bucket for
                    // the replaced session, so a pick (or an override)
                    // left here would record on a session that no
                    // longer exists while the readout claims the
                    // configured default.
                    {
                        let mut domain = self.domain.lock();
                        domain.dictate_overrides = crate::dictate::DictateOverrides::default();
                        domain.dictate_device = None;
                    }
                    let previous_key = self.key.clone();
                    self.rekey_to(&real_key);
                    self.emit(SessionUpdate::SessionReplaced {
                        key: real_key.clone(),
                        previous_key,
                        session_id: SessionId::new(session_id),
                        cwd,
                        current_model,
                        available_models,
                        mode,
                        history,
                        compaction_count,
                    });
                    // After SessionReplaced so the echoes land on the
                    // fresh bucket the TUI minted for it: the cleared
                    // values re-affirm the blank state.
                    self.emit(SessionUpdate::DictateOverrides {
                        key: real_key.clone(),
                        overrides: crate::dictate::DictateOverrides::default(),
                    });
                    self.emit(SessionUpdate::DictateDevicePin {
                        key: real_key.clone(),
                        pick: None,
                    });
                } else {
                    self.rekey_to(&real_key);
                    self.connected_once = true;
                    // When the lead's synth key names a project with
                    // persisted workers and none are live yet, dispatch
                    // one SpawnWorker per row. Idempotent via the
                    // live_workers gate - a reconnect after a transient
                    // failure skips re-spawn.
                    if let Some(spawn_key) = self.spawn_key.as_ref()
                        && let Some(workspace) = self.workspace.upgrade()
                    {
                        let force_new = self.domain.lock().spawned_force_new;
                        maybe_respawn_workers_on_connected(
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
                        compaction_count,
                    });
                    // Drain any peer prompts buffered while this
                    // session was pre-Connected (pushed by
                    // spawn::handle_deliver_peer_prompt when a peer
                    // ask hit a sleeping target). Each gets
                    // re-dispatched as a regular Command::Prompt so
                    // the existing prompt-delivery path handles it
                    // uniformly with user-typed prompts.
                    self.drain_pending_peer_prompts();
                    // Same for cron prompts buffered for this session's owner
                    // while it was asleep: each echoes a cron block then
                    // re-dispatches (missed-marked when overdue).
                    self.drain_pending_cron_prompts(&cwd_for_tag);
                    // Same for Gotify notification envelopes buffered while
                    // the project was asleep.
                    self.drain_pending_gotify_prompts();
                }
                // Re-tag must fire on BOTH first-Connected and
                // post-/new Connected paths: a /new writes a fresh
                // JSONL, and if it isn't re-tagged the resume scan
                // picks the stale pre-/new orphan instead.
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
                    // A project-spawn that never connected still holds
                    // its buffered peer asks on the synth key - those
                    // were never delivered, so the target_session match
                    // above can't reach them.
                    workspace.expire_buffered_peer_prompts(
                        &key,
                        crate::mcp::peers::types::PeerFailureReason::TargetConnectionFailed,
                    );
                    // Same for Gotify notifications buffered at the synth
                    // key: the failed spawn strands them - drain and
                    // record the drop.
                    workspace.expire_buffered_gotify_prompts(&key);
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
                // workspace maps hold, and the synth key while the TUI
                // may still route to it); `release_session_if_current`
                // no-ops on absent keys. The run loop then exits, so
                // Drop's expiry backstop fires too.
                if let Some(workspace) = self.workspace.upgrade() {
                    workspace.release_session_if_current(&self.key, &self.handle);
                    if let Some(spawn_key) = self.spawn_key.clone()
                        && spawn_key != self.key
                    {
                        workspace.release_session_if_current(&spawn_key, &self.handle);
                    }
                }
                return false;
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
            AgentEvent::RuntimeReloadCompleted { session_id } => {
                self.emit(SessionUpdate::RuntimeReloadCompleted { session_id });
            }
            AgentEvent::RuntimeReloadFailed { session_id, message } => {
                self.emit(SessionUpdate::RuntimeReloadFailed { session_id, message });
            }
            AgentEvent::SetModeFailed { session_id, mode, message } => {
                let session_key = SessionKey::from_session_id(session_id);
                self.emit(SessionUpdate::SetModeFailed { key: session_key, mode, message });
            }
            AgentEvent::SetModelFailed { session_id, model, message } => {
                let session_key = SessionKey::from_session_id(session_id);
                self.emit(SessionUpdate::SetModelFailed { key: session_key, model, message });
            }
            AgentEvent::TurnError { session_id, message } => {
                let session_key = SessionKey::from_session_id(session_id);
                self.emit(SessionUpdate::TurnError {
                    key: session_key,
                    message,
                    class: None,
                    terminal_reason: None,
                });
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
                // Clear the turn-commit marker on the turn boundary so
                // the `/account` backstop stops refusing once the turn
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
                self.note_rate_limit_from_message(&msg);
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
        true
    }

    /// Mark this session's account rate-limited on a live 429 so the
    /// next assignment rotates off it, reacting faster than the periodic
    /// usage probe. Self-correcting: `retry_after` schedules the
    /// re-probe that clears the mark once the account recovers.
    fn note_rate_limit_from_message(&self, msg: &forge_primitives::Message) {
        let Some(hit) = rate_limit_hit_from_message(msg) else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let Some(account) = &self.account else {
            tracing::warn!(
                target: "forge_workspace::session_task",
                key = %self.key.as_str(),
                "live 429 on a session with no tracked account; cannot rotate",
            );
            return;
        };
        let rotated = {
            let mut states = workspace.account_states().lock();
            let tracked = states.config_dir(account).is_some();
            if tracked {
                states.set_last_error(account, UsageFetchStatus::RateLimited, hit.retry_after);
            }
            tracked
        };
        if rotated {
            tracing::info!(
                target: "forge_workspace::session_task",
                key = %self.key.as_str(),
                account = %account.0.as_str(),
                "marked the session's account rate-limited from a live 429; next assignment rotates off it",
            );
        } else {
            tracing::warn!(
                target: "forge_workspace::session_task",
                key = %self.key.as_str(),
                account = %account.0.as_str(),
                "live 429 names an account the map no longer tracks; cannot rotate",
            );
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
    // task (its session re-spawned under the same key by an `/account`
    // switch) can't emit a stale update onto the successor's bucket
    // during its brief post-supersession drain. Low-risk today: the
    // switch is idle-gated and the re-spawn keeps the same session_id.
    fn emit(&self, update: SessionUpdate) {
        if self.update_tx.send(update).is_err() {
            tracing::warn!(
                target: "forge_workspace::session_task",
                key = %self.key.as_str(),
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
    fn drain_review_activity_for(&self, caller: &SessionKey) {
        let Some(workspace) = self.workspace.upgrade() else { return };
        for update in workspace.drain_review_activity(caller) {
            self.emit(update);
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
            if let Err(err) = workspace.dispatch_workspace_prompt(&self.key, text) {
                tracing::warn!(
                    target: "forge_workspace::session_task",
                    key = %self.key.as_str(),
                    error = ?err,
                    "drain_pending_peer_prompts: dispatch failed; prompt dropped"
                );
                crate::spawn::send_dispatch_turn_error(&workspace, self.key.clone(), &err);
            }
        }
    }

    /// Drain the cron prompts buffered for this session's owner after its
    /// first `Connected` - the `(project, team_role)` bucket a due cron
    /// filled while the owner was asleep. Each is echoed as a cron block
    /// (missed-marked when overdue) and re-dispatched as a plain
    /// `Command::Prompt`: the lead drains its `None` bucket, a worker its
    /// own label. No-op when the bucket is empty.
    fn drain_pending_cron_prompts(&self, cwd: &str) {
        let Some(workspace) = self.workspace.upgrade() else { return };
        for cron in workspace.take_pending_crons_for_session(&self.key, cwd) {
            let text = crate::spawn::missed_cron_text(&cron.text, cron.missed);
            crate::spawn::push_cron_prompt_into_chat(&workspace, &self.key, &text);
            if let Err(err) = workspace.dispatch_workspace_prompt(&self.key, text) {
                tracing::warn!(
                    target: "forge_workspace::session_task",
                    key = %self.key.as_str(),
                    error = ?err,
                    "drain_pending_cron_prompts: dispatch failed; prompt dropped",
                );
                crate::spawn::send_dispatch_turn_error(&workspace, self.key.clone(), &err);
            }
        }
    }

    /// Drain `DomainSession.pending_gotify_prompts` after the session's
    /// first `Connected` event - Gotify notifications buffered while the
    /// project was asleep. Each is echoed into chat as a notification
    /// block and re-dispatched as a plain `Command::Prompt`, landing as an
    /// ordinary user turn. Mirrors [`Self::drain_pending_cron_prompts`]
    /// plus the peer chat-echo. No-op when the buffer is empty.
    fn drain_pending_gotify_prompts(&self) {
        let pending: Vec<crate::mcp::gotify::types::GotifyNotification> =
            std::mem::take(&mut self.domain.lock().pending_gotify_prompts);
        if pending.is_empty() {
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else { return };
        for notification in pending {
            // Echo the notification block, then re-dispatch its prose as a
            // plain user turn (mirrors the running-target path in
            // spawn::deliver_gotify_message).
            crate::spawn::push_gotify_notification_into_chat(&workspace, &self.key, &notification);
            if let Err(err) =
                workspace.dispatch_workspace_prompt(&self.key, notification.to_prose())
            {
                tracing::warn!(
                    target: "forge_workspace::session_task",
                    key = %self.key.as_str(),
                    error = ?err,
                    "drain_pending_gotify_prompts: dispatch failed; prompt dropped"
                );
                crate::spawn::send_dispatch_turn_error(&workspace, self.key.clone(), &err);
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

/// Shared worker Connected hook: if `spawn_key` is a project-lead synth
/// key and no live workers exist for the project yet, (re-)spawn its
/// workers - one `SpawnWorker` per persisted worker row. Called from
/// `SessionTask::translate_event` (production) and `on_connected_for_test`
/// (tests). Idempotent; safe to call multiple times for the same session -
/// the `live_workers.is_empty()` gate guards against double-spawn on
/// `/new` reconnects or transient retries, and
/// `respawn_workers_for_lead` no-ops when the project has no
/// worker rows.
fn maybe_respawn_workers_on_connected(
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
    let project_key = crate::target::ProjectKey::new(
        forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
            &project.path.to_string_lossy(),
        )),
    );
    if !workspace.list_live_workers(&project_key).is_empty() {
        return;
    }
    // Scan the project's catalog for previously-spawned worker
    // sessions (tagged `forge:worker:<label>`) so each worker resumes
    // its existing session instead of starting fresh. The scan is
    // async (filesystem I/O); workspace claims a per-project
    // in-flight guard synchronously so a fast double-Connected
    // can't slip a second worker-spawn through. The guard is
    // released after the SpawnWorker commands are dispatched.
    workspace.respawn_workers_for_lead(
        real_session_id.to_owned(),
        project_key,
        project.path.clone(),
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
/// underscores), so on the "<project>_<label>_<uuid>" remainder the
/// project ends at the FIRST underscore and the uuid starts after the
/// LAST. Everything between is the label, which is therefore free to
/// contain underscores of its own (`code_review`). The project_key
/// segment is what the kick hook scopes its live-worker lookup by.
pub(crate) fn parse_worker_synth_key(key: &SessionKey) -> Option<(String, String)> {
    let s = key.as_str();
    let inner =
        s.strip_prefix("__spawn_").or_else(|| s.strip_prefix("__resume_"))?.strip_suffix("__")?;
    let after_worker = inner.strip_prefix("worker_")?;
    let (project_key, rest) = after_worker.split_once('_')?;
    let (label, _uuid) = rest.rsplit_once('_')?;
    Some((project_key.to_owned(), label.to_owned()))
}

/// Shared worker-kick hook: if `spawn_key` is a worker synth key,
/// dispatch a `Command::Prompt` carrying the live worker's stored kick
/// to the freshly-Connected worker. Claude sessions don't act until a
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
fn maybe_kick_worker_on_connected(
    workspace: &Arc<crate::Workspace>,
    spawn_key: &SessionKey,
    real_session_id: &str,
) {
    let Some((project_key, label)) = parse_worker_synth_key(spawn_key) else {
        return;
    };
    let Some(view) = workspace.list_projects().into_iter().find(|v| v.key.as_str() == project_key)
    else {
        return;
    };
    // `handle_spawn_worker` inserts the entry as Spawning before the agent
    // spawn so this hook always finds it, which makes a miss an invariant
    // violation rather than a kick-less worker. Kept apart from `None`
    // kick, which is the ordinary silent case.
    let Some(entry) =
        workspace.list_live_workers(&view.key).into_iter().rev().find(|w| w.label == label)
    else {
        tracing::warn!(
            target: "forge_workspace::workers",
            label = %label,
            session_id = real_session_id,
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
    workspace.enqueue_kick(crate::workspace::KickRequest {
        session_key: SessionKey::from_session_id(real_session_id.to_owned()),
        prompt_body: kick,
    });
}

/// Test-only entry point for the Connected hooks.
/// Drives both `maybe_respawn_workers_on_connected` (lead path) and
/// `maybe_kick_worker_on_connected` (worker path) directly without
/// constructing a `SessionTask` or pumping through the actor - the
/// `connected_hook_tests` module uses this to assert the trigger logic.
/// Only one hook fires per call: the spawn_key's shape selects.
#[cfg(test)]
fn on_connected_for_test(
    workspace: &Arc<crate::Workspace>,
    synth_key: &SessionKey,
    real_session_id: &str,
) {
    // Normal (non-`--new`) Connected simulation; the force-new cascade
    // is exercised directly against respawn_workers_for_lead.
    maybe_respawn_workers_on_connected(workspace, synth_key, real_session_id, false);
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
/// Returns `Ok(())` on successful enqueue; `Err(...)` on AgentHandle
/// send failure (dispatcher channel closed) or on a command dropped
/// for having no `session_id` yet (pre-Connect).
pub(crate) fn execute_command_via_handle(
    handle: &Arc<AgentHandle>,
    key: &SessionKey,
    session_id: Option<&str>,
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
            handle.new_session(cwd, launch_settings)
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
                key = %key.as_str(),
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
        | Command::SwitchAccount { .. }
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
                key = %key.as_str(),
                command = ?misrouted,
                "App-level command unexpectedly routed via per-session path"
            );
            Ok(())
        }
    }
}

fn warn_no_session(key: &SessionKey, command: &'static str) -> forge_agent::AgentError {
    tracing::warn!(
        target: "forge_workspace::session_task",
        key = %key.as_str(),
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
        // `/account` backstop doesn't read a stale in-flight turn.
        domain.runtime_state = None;
        domain.turn_pending = false;
    }
    if let AgentEvent::Connected { session_id, .. } = event {
        domain.session_id = Some(SessionId::new(session_id.clone()));
        domain.runtime_state = None;
        domain.turn_pending = false;
    }
    // Mirror runtime liveness from `session_state_changed` so the
    // account-switch backstop (`handle_switch_account`) sees an
    // in-flight turn authoritatively, independent of the TUI gate.
    // Reuse the canonical decoder parser rather than re-inlining it.
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

    fn empty_domain() -> DomainSession {
        let (handle, _rx) = Agent::testing_stub();
        DomainSession::new(SessionKey::from_str_for_test("test"), Some(Arc::new(handle)))
    }

    fn loaded_account(name: &str) -> crate::config::LoadedAccount {
        crate::config::LoadedAccount {
            display_name: name.to_owned(),
            config_dir: std::path::PathBuf::from(format!("/fake/{name}")),
            provider: forge_primitives::account::Provider::Anthropic,
            env: std::collections::HashMap::new(),
            experimental: false,
            permission_mode: None,
        }
    }

    fn api_retry(error_status: u64, error: &str) -> forge_primitives::Message {
        forge_primitives::Message::System {
            subtype: "api_retry".to_owned(),
            session_id: Some("s1".to_owned()),
            data: serde_json::json!({
                "attempt": 1,
                "max_retries": 4,
                "retry_delay_ms": 5000,
                "error_status": error_status,
                "error": error,
            }),
        }
    }

    /// Reproduce: a live 429 (api_retry carrying error_status=429) for
    /// the active session flips its account not-usable via
    /// set_last_error, so the next assignment rotates off it without
    /// waiting for the periodic usage probe.
    #[test]
    fn live_429_flips_active_account_to_unusable() {
        use crate::account::{AccountKey, AccountStateMap};
        let mut map = AccountStateMap::new(&[loaded_account("A"), loaded_account("B")]);
        let key_a = AccountKey("A".to_owned());
        assert!(map.is_account_usable(&key_a), "account usable before the 429");

        let hit = rate_limit_hit_from_message(&api_retry(429, "rate_limit"))
            .expect("429 detected as a rate-limit");
        map.set_last_error(&key_a, UsageFetchStatus::RateLimited, hit.retry_after);

        assert!(
            !map.is_account_usable(&key_a),
            "429 must flip the active account unusable so the next assignment rotates",
        );
        assert!(
            map.is_account_usable(&AccountKey("B".to_owned())),
            "the sibling account stays usable",
        );
    }

    /// The `RateLimit` error enum is a rate-limit even when the wire
    /// omits a numeric status.
    #[test]
    fn rate_limit_enum_without_numeric_status_is_a_hit() {
        let msg = forge_primitives::Message::System {
            subtype: "api_retry".to_owned(),
            session_id: Some("s1".to_owned()),
            data: serde_json::json!({
                "attempt": 1,
                "max_retries": 4,
                "retry_delay_ms": 3000,
                "error": "rate_limit",
            }),
        };
        assert!(rate_limit_hit_from_message(&msg).is_some());
    }

    /// A non-rate-limit api_retry (e.g. a 529 server_error) must NOT
    /// rotate the account - only 429 / RateLimit does.
    #[test]
    fn non_rate_limit_retry_does_not_rotate() {
        assert!(rate_limit_hit_from_message(&api_retry(529, "server_error")).is_none());
    }

    fn workspace_with_account_config_dir(
        config_dir: &str,
    ) -> (tempfile::TempDir, Arc<crate::Workspace>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let forge = dir.path().join("forge");
        std::fs::create_dir_all(&forge).expect("forge dir");
        std::fs::write(
            forge.join("forge.toml"),
            format!(
                "[[orgs]]\nname = \"Default\"\naccounts = [\"Acct\"]\n\n\
                 [[orgs.projects]]\nname = \"forge\"\npath = \"~/Projects/forge\"\n\n\
                 [[accounts]]\ndisplay_name = \"Acct\"\nconfig_dir = \"{config_dir}\"\nprovider = \"anthropic\"\n"
            ),
        )
        .expect("write forge.toml");
        let workspace =
            Arc::new(crate::Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        (dir, workspace)
    }

    /// Build a `SessionTask` bound to `workspace` with a testing-stub
    /// handle (its `config_dir` is the `TESTING_STUB_CONFIG_DIR`
    /// `/tmp/forge-testing-stub`). Only `note_rate_limit_from_message`
    /// is exercised, so the command/update channels stay idle.
    fn session_task_for(
        workspace: &Arc<crate::Workspace>,
        account: Option<AccountKey>,
    ) -> SessionTask {
        let (handle, _cmds) = Agent::testing_stub();
        let handle = Arc::new(handle);
        let (_cmd_tx, command_rx) = mpsc::unbounded_channel();
        let (update_tx, _update_rx) = mpsc::unbounded_channel();
        let key = SessionKey::from_str_for_test("rl-test");
        let domain = Arc::new(Mutex::new(DomainSession::new(key.clone(), Some(handle.clone()))));
        SessionTask {
            key,
            handle,
            command_rx,
            domain,
            update_tx,
            spawn_key: None,
            account,
            connected_once: false,
            workspace: Arc::downgrade(workspace),
        }
    }

    /// A workspace holding one submitted review plus one un-drained
    /// reply from `worker`, i.e. a turn that has touched a review and
    /// not yet ended. Returns the tempdir (kept alive for the db), the
    /// workspace, the review's submit origin, and the replying session.
    fn workspace_with_pending_review_activity()
    -> (tempfile::TempDir, Arc<crate::Workspace>, SessionKey, SessionKey) {
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
        let reviewer = SessionKey::from_session_id("reviewer");
        let worker = SessionKey::from_session_id("worker");
        workspace.submit_review("forge", "feat", None, &["a".to_owned()], reviewer.clone());
        workspace
            .review_reply(&worker, "forge", "feat", "a", "implementer", "fixed", "t")
            .expect("reply recorded as this turn's activity");
        (dir, workspace, reviewer, worker)
    }

    /// A `SessionTask` bound to `key`, plus the receiver for what it emits.
    fn review_task_for(
        workspace: &Arc<crate::Workspace>,
        key: &SessionKey,
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
            spawn_key: None,
            account: None,
            connected_once: false,
            workspace: Arc::downgrade(workspace),
        };
        (task, update_rx)
    }

    /// The review-activity notice a task emitted, if any.
    fn drained_notice(
        update_rx: &mut mpsc::UnboundedReceiver<SessionUpdate>,
    ) -> Option<(SessionKey, String, usize, String)> {
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
    /// switch) never produces a Result, and `rekey_to` moves the key the
    /// buffer is filed under - so the drain has to happen here or the
    /// activity is unreachable for the rest of the process's life.
    #[tokio::test]
    async fn session_replacement_drains_review_activity_before_the_rekey() {
        let (_dir, workspace, reviewer, worker) = workspace_with_pending_review_activity();
        let (mut task, mut update_rx) = review_task_for(&workspace, &worker);
        task.connected_once = true;

        task.translate_event(connected_event("worker-replacement", "/tmp/proj"));

        let (key, ..) = drained_notice(&mut update_rx)
            .expect("the replaced identity flushes what its turn touched");
        assert_eq!(key, reviewer);
        assert!(
            workspace.drain_review_activity(&worker).is_empty(),
            "nothing strands under the pre-rekey key",
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
        let key = SessionKey::from_str_for_test("dead-spawn");
        let (handle, _cmds) = Agent::testing_stub();
        let handle = Arc::new(handle);
        let (cmd_tx, command_rx) = mpsc::unbounded_channel();
        let (update_tx, mut update_rx) = mpsc::unbounded_channel();
        workspace.pool.lock().insert(
            key.clone(),
            crate::workspace::PooledAgent {
                handle: Arc::clone(&handle),
                account: crate::account::AccountKey("Acct".to_owned()),
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
            spawn_key: None,
            account: None,
            connected_once: false,
            workspace: Arc::downgrade(&workspace),
        };

        let continues = task
            .translate_event(AgentEvent::ConnectionFailed { message: "spawn failed".to_owned() });

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

    /// A `ConnectionFailed` on a task that still carries its synthetic
    /// spawn key releases BOTH registrations - the synth key the TUI
    /// may still route to, and the resolved key the pool holds.
    #[tokio::test]
    async fn connection_failed_releases_spawn_key_registrations_too() {
        let (_dir, workspace) = workspace_with_account_config_dir("/tmp/forge-testing-stub");
        let key = SessionKey::from_str_for_test("real-key");
        let spawn_key = SessionKey::from_str_for_test("__spawn_proj__");
        let (handle, _cmds) = Agent::testing_stub();
        let handle = Arc::new(handle);
        let (cmd_tx, command_rx) = mpsc::unbounded_channel();
        let (update_tx, _update_rx) = mpsc::unbounded_channel();
        workspace.pool.lock().insert(
            key.clone(),
            crate::workspace::PooledAgent {
                handle: Arc::clone(&handle),
                account: crate::account::AccountKey("Acct".to_owned()),
            },
        );
        workspace.command_senders.lock().insert(key.clone(), cmd_tx);
        let mut task = SessionTask {
            key: key.clone(),
            handle: Arc::clone(&handle),
            command_rx,
            domain: Arc::new(Mutex::new(DomainSession::new(key.clone(), None))),
            update_tx,
            spawn_key: Some(spawn_key.clone()),
            account: None,
            connected_once: false,
            workspace: Arc::downgrade(&workspace),
        };

        let continues = task
            .translate_event(AgentEvent::ConnectionFailed { message: "spawn failed".to_owned() });

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

    /// A `PermissionRequest` registers a pending slot; `RespondPermission`
    /// consumes it and the outcome reaches the agent round-trip. The
    /// happy path of the can_use_tool parking lot.
    #[tokio::test]
    async fn respond_permission_round_trips_to_the_agent() {
        let (_dir, workspace) = workspace_with_account_config_dir("/tmp/forge-testing-stub");
        let (handle, mut agent_rx) = Agent::testing_stub();
        let handle = Arc::new(handle);
        let key = SessionKey::from_session_id("perm");
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
            spawn_key: None,
            account: None,
            connected_once: false,
            workspace: Arc::downgrade(&workspace),
        };

        task.translate_event(AgentEvent::PermissionRequest {
            session_id: key.as_str().to_owned(),
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
        let key = SessionKey::from_session_id("xkind");
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
            spawn_key: None,
            account: None,
            connected_once: false,
            workspace: Arc::downgrade(&workspace),
        };

        task.translate_event(AgentEvent::QuestionRequest {
            session_id: key.as_str().to_owned(),
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
            review_task_for(&workspace, &SessionKey::from_session_id("m"));

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

    /// End-to-end glue: a 429 on a session spawned under a tracked
    /// account rotates THAT account via `note_rate_limit_from_message`
    /// (upgrade + the account-map lock).
    #[tokio::test]
    async fn note_rate_limit_rotates_the_sessions_own_account() {
        use crate::account::AccountKey;
        let (_dir, workspace) = workspace_with_account_config_dir("/tmp/forge-testing-stub");
        let key = AccountKey("Acct".to_owned());
        assert!(workspace.account_states().lock().is_account_usable(&key), "usable before the 429");

        let task = session_task_for(&workspace, Some(key.clone()));
        task.note_rate_limit_from_message(&api_retry(429, "rate_limit"));

        assert!(
            !workspace.account_states().lock().is_account_usable(&key),
            "a 429 on the session rotates its own account off",
        );
    }

    /// A 429 on a session spawned off-plan (no account) must NOT
    /// rotate anything - it hits the warn path instead.
    #[tokio::test]
    async fn note_rate_limit_leaves_untracked_config_dir_accounts_alone() {
        use crate::account::AccountKey;
        let (_dir, workspace) = workspace_with_account_config_dir("/tmp/forge-test-rl-other");
        let key = AccountKey("Acct".to_owned());
        assert!(workspace.account_states().lock().is_account_usable(&key));

        let task = session_task_for(&workspace, None);
        task.note_rate_limit_from_message(&api_retry(429, "rate_limit"));

        assert!(
            workspace.account_states().lock().is_account_usable(&key),
            "a 429 on an account-less session must not rotate an unrelated one",
        );
    }

    /// Two accounts sharing one config dir is the token-mode norm: the
    /// 429 must mark the session's OWN account, never whichever
    /// sibling a config-dir reverse lookup would happen to find.
    #[tokio::test]
    async fn note_rate_limit_marks_the_sessions_account_not_a_shared_dir_sibling() {
        use crate::account::AccountKey;
        let dir = tempfile::tempdir().expect("tempdir");
        let forge = dir.path().join("forge");
        std::fs::create_dir_all(&forge).expect("forge dir");
        std::fs::write(
            forge.join("forge.toml"),
            "[[orgs]]\nname = \"Default\"\naccounts = [\"Acct\", \"Sibling\"]\n\n\
             [[orgs.projects]]\nname = \"forge\"\npath = \"~/Projects/forge\"\n\n\
             [[accounts]]\ndisplay_name = \"Acct\"\nconfig_dir = \"/tmp/forge-testing-stub\"\nprovider = \"anthropic\"\n\n\
             [[accounts]]\ndisplay_name = \"Sibling\"\nconfig_dir = \"/tmp/forge-testing-stub\"\nprovider = \"anthropic\"\n",
        )
        .expect("write forge.toml");
        let workspace =
            Arc::new(crate::Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        let session_account = AccountKey("Acct".to_owned());
        let sibling = AccountKey("Sibling".to_owned());
        assert!(workspace.account_states().lock().is_account_usable(&sibling));

        let task = session_task_for(&workspace, Some(session_account.clone()));
        task.note_rate_limit_from_message(&api_retry(429, "rate_limit"));

        assert!(
            !workspace.account_states().lock().is_account_usable(&session_account),
            "the session's own account takes the 429 mark",
        );
        assert!(
            workspace.account_states().lock().is_account_usable(&sibling),
            "the sibling sharing the config dir stays usable",
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
    /// `/account` backstop must not read a stale "turn in flight" and
    /// refuse the switch with "Finish or cancel the current turn".
    #[test]
    fn connection_failed_clears_domain_turn_state() {
        let mut domain = empty_domain();
        domain.runtime_state = Some(forge_primitives::RuntimeSessionState::Running);
        domain.turn_pending = true;

        apply_event_to_domain(
            &mut domain,
            &AgentEvent::ConnectionFailed { message: "reader died".to_owned() },
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
            key: SessionKey::from_str_for_test("old-uuid"),
            handle: Arc::new(handle),
            command_rx,
            domain: Arc::clone(&domain),
            update_tx,
            spawn_key: None,
            account: None,
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

    /// First-Connected drains `DomainSession.pending_peer_prompts` in
    /// FIFO order, dispatching one `Command::Prompt` per buffered
    /// entry, then leaves the buffer empty. Pinned via the workspace's
    /// command-intercept buffer so the full first-Connected branch of
    /// `translate_event` runs end-to-end (no poking the private drain
    /// method directly).
    #[tokio::test]
    async fn first_connected_drains_pending_peer_prompts_in_fifo_order() {
        use crate::mcp::peers::types::{AskChannel, CorrelationId, WrappedKind, WrappedPrompt};

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
                    channel: AskChannel::Peers,
                    sender_name: "forge".to_owned(),
                    sender_org: "Default".to_owned(),
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
            account: None,
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
            domain.lock().pending_peer_prompts.is_empty(),
            "pending_peer_prompts is drained after first-Connected"
        );
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

    fn cron_worker_entry(label: &str, session_id: &str) -> crate::mcp::workers::types::WorkerEntry {
        crate::mcp::workers::types::WorkerEntry {
            label: label.to_owned(),
            charter: "c".to_owned(),
            session_key: SessionKey::from_session_id(session_id),
            status: forge_primitives::WorkerLiveness::Running,
            spawned_at: std::time::SystemTime::UNIX_EPOCH,
            spawned_by_session_id: "lead-uuid".to_owned(),
            needs_tag: false,
            is_git_repo_at_spawn: false,
            diagnostic: None,
            kick: None,
        }
    }

    /// First-Connected drains the session owner's buffered cron prompts:
    /// each dispatches a plain `Command::Prompt` AND echoes a
    /// `CronPromptAppended` so an asleep-fired cron shows its block once the
    /// session connects (mirrors the gotify drain echo). Reproduce-first:
    /// the echo is absent until the drain calls `push_cron_prompt_into_chat`.
    #[tokio::test]
    async fn first_connected_drains_pending_cron_prompts_and_echoes_block() {
        let (workspace, mut update_rx) = crate::Workspace::testing_stub();
        // The session's cwd resolves to this project so the owner drain keys
        // on (project, None) - a lead session with no live-worker label.
        workspace.seed_test_project("cron-drain", "/tmp/cron-drain");
        workspace.buffer_cron_for_owner("cron-drain", None, "morning reminder".to_owned(), false);

        let session_key = SessionKey::from_session_id("cron-drain-uuid");
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
            spawn_key: None,
            account: None,
            connected_once: false,
            workspace: Arc::downgrade(&workspace),
        };

        workspace.enable_test_dispatch_intercept();
        task.translate_event(connected_event(session_key.as_str(), "/tmp/cron-drain"));

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
                SessionUpdate::CronPromptAppended { session_id, text }
                    if session_id == session_key.as_str() && text == "morning reminder"
            ) {
                echoed = true;
            }
        }
        assert!(echoed, "an asleep-fired cron echoes a CronPromptAppended on drain");

        assert!(
            workspace.take_pending_crons_for_session(&session_key, "/tmp/cron-drain").is_empty(),
            "the owner's cron bucket is drained after first-Connected",
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
        workspace.insert_live_worker(&key, cron_worker_entry("reviewer", "worker-drain-uuid"));

        // A missed cron for the worker + an on-time lead cron for the project.
        workspace.buffer_cron_for_owner("wdp", Some("reviewer"), "worker work".to_owned(), true);
        workspace.buffer_cron_for_owner("wdp", None, "lead work".to_owned(), false);

        let session_key = SessionKey::from_session_id("worker-drain-uuid");
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
            spawn_key: None,
            account: None,
            connected_once: false,
            workspace: Arc::downgrade(&workspace),
        };

        workspace.enable_test_dispatch_intercept();
        task.translate_event(connected_event("worker-drain-uuid", "/tmp/wdp"));

        let dispatched = workspace.drain_test_dispatch_buffer();
        assert!(
            dispatched.iter().any(|c| matches!(
                c, crate::protocol::Command::Prompt { key, text, .. }
                    if key == &session_key && text == "[missed cron] worker work"
            )),
            "the worker drains its own missed cron with the marker applied",
        );
        // The lead's bucket is untouched by the worker's drain.
        let lead_key = SessionKey::from_session_id("lead-uuid");
        let lead_bucket = workspace.take_pending_crons_for_session(&lead_key, "/tmp/wdp");
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
            let session_key = SessionKey::from_session_id("count-through-uuid");
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
                spawn_key: None,
                account: None,
                connected_once,
                workspace: Arc::downgrade(&workspace),
            };

            let mut event = connected_event(session_key.as_str(), "/tmp/count");
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

    /// The account-switch re-spawn seeds `connected_once = true`, so
    /// the new task's first Connected emits `SessionReplaced` (not a
    /// fresh Connected) carrying the resumed history. The TUI reducer
    /// resets the chat then re-seeds it from that history, so the same
    /// conversation stays visible across the switch.
    #[tokio::test]
    async fn connected_once_seed_emits_session_replaced_with_resumed_history() {
        use forge_primitives::Message;

        let (workspace, mut update_rx) = crate::Workspace::testing_stub();
        let session_key = SessionKey::from_session_id("switch-visible-uuid");
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
            spawn_key: None,
            account: None,
            // The seed a forced-account re-spawn installs.
            connected_once: true,
            workspace: Arc::downgrade(&workspace),
        };

        // A one-message resumed history (the --resume backfill).
        let history = vec![Message::System {
            subtype: "info".to_owned(),
            session_id: Some(session_key.as_str().to_owned()),
            data: serde_json::json!({ "body": "earlier turn" }),
        }];

        task.translate_event(AgentEvent::Connected {
            session_id: session_key.as_str().to_owned(),
            cwd: "/tmp/switch".to_owned(),
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
                    assert_eq!(key, session_key, "SessionReplaced targets the switched session");
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
        assert!(!saw_plain_connected, "an account switch must not emit a fresh Connected");
    }

    /// The replaced identity's `/dictate` state must die with it: the
    /// TUI mints a blank bucket for the new session, so a pick left on
    /// the domain would record on the previous session's device while
    /// the fresh readout claims the configured default.
    #[tokio::test]
    async fn a_replaced_identity_drops_its_dictate_state_and_echoes_the_clear() {
        let (workspace, mut update_rx) = crate::Workspace::testing_stub();
        let session_key = SessionKey::from_session_id("dictate-rekey-uuid");
        let domain =
            Arc::new(parking_lot::Mutex::new(DomainSession::new(session_key.clone(), None)));
        domain.lock().dictate_overrides.styling = Some(forge_dictate::normalize::Styling::Formal);
        domain.lock().dictate_device =
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
            spawn_key: None,
            account: None,
            connected_once: true,
            workspace: Arc::downgrade(&workspace),
        };

        task.translate_event(AgentEvent::Connected {
            session_id: session_key.as_str().to_owned(),
            cwd: "/tmp/switch".to_owned(),
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

        assert_eq!(
            domain.lock().dictate_device,
            None,
            "the replaced identity must not carry its pick onto the new session"
        );
        assert_eq!(domain.lock().dictate_overrides, crate::dictate::DictateOverrides::default());

        let mut replaced_seen = false;
        let mut pin_echoes = vec![];
        while let Ok(u) = update_rx.try_recv() {
            match u {
                SessionUpdate::SessionReplaced { .. } => replaced_seen = true,
                SessionUpdate::DictateDevicePin { pick, .. } => {
                    assert!(replaced_seen, "the echo must land after the fresh bucket is minted");
                    pin_echoes.push(pick);
                }
                _ => {}
            }
        }
        assert_eq!(pin_echoes, vec![None], "the clear echoes with the pick gone");
    }

    /// A forced-account switch tears the live session down BEFORE
    /// re-spawning, so if the re-spawned agent fails to connect the
    /// session is momentarily agent-less. That failure must be
    /// recoverable (`ConnectionFailed { fatal: false }`), and the task's
    /// exit must leave no lingering pooled agent under the key. (The
    /// synchronous `get_agent_handle` Err arm is defensive/near-
    /// unreachable - `Agent::spawn` is infallible - so this covers the
    /// realistic async failure path instead.)
    #[tokio::test]
    async fn switch_respawn_connection_failure_is_nonfatal_and_releases_the_session() {
        let (workspace, mut update_rx) = crate::Workspace::testing_stub();
        let key = SessionKey::from_session_id("switch-fail-uuid");

        let (handle, _agent_cmds) = Agent::testing_stub();
        let arc = Arc::new(handle);
        // Register the re-spawned session under `key` with `arc` pooled -
        // the SAME Arc the task holds, so the exit cleanup recognises it.
        workspace.pool.lock().insert(
            key.clone(),
            crate::workspace::PooledAgent {
                handle: Arc::clone(&arc),
                account: crate::account::AccountKey("test".to_owned()),
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
            spawn_key: None,
            account: None,
            connected_once: true, // a forced-account switch re-spawn
            workspace: Arc::downgrade(&workspace),
        };

        // The re-spawned agent fails to connect.
        task.translate_event(AgentEvent::ConnectionFailed { message: "spawn failed".to_owned() });

        let mut saw_nonfatal = false;
        while let Ok(update) = update_rx.try_recv() {
            if let SessionUpdate::ConnectionFailed { key: failed_key, fatal, .. } = update {
                assert!(!fatal, "a failed switch re-spawn is recoverable, not fatal");
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
        let err =
            execute_command_via_handle(&handle, &key, None, Command::Cancel { key: key.clone() })
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

    fn synth_lead_key(project_name: &str) -> SessionKey {
        SessionKey::from_session_id(format!("__spawn_{project_name}__"))
    }

    /// Seed `proj-x` with one persisted worker row and return the
    /// tempdir backing the store, whose lifetime must outlive the test.
    fn seed_project_with_one_worker_row(
        workspace: &Arc<Workspace>,
        label: &str,
    ) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        workspace.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        workspace.seed_test_project("proj-x", "/tmp/proj-x");
        let project_key = ProjectKey::new(
            forge_agent::userdata::catalog::scan::project_key_for_directory(Some("/tmp/proj-x")),
        );
        let _ = workspace.persist_dynamic_worker(&crate::store::dynamic_workers::DynamicWorker {
            project_key: project_key.as_str().to_owned(),
            label: label.to_owned(),
            charter: format!("charter for {label}"),
            kick: None,
            resume_kick: None,
            interactive: false,
        });
        dir
    }

    /// A lead Connected for a project with a persisted worker triggers
    /// one `Command::SpawnWorker` per row.
    #[test]
    fn lead_connected_with_a_worker_row_triggers_respawn() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        let _db = seed_project_with_one_worker_row(&workspace, "implementer");
        workspace.enable_test_dispatch_intercept();

        on_connected_for_test(&workspace, &synth_lead_key("proj-x"), "lead-uuid");

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

        on_connected_for_test(&workspace, &synth_lead_key("proj-y"), "lead-uuid");

        assert!(workspace.drain_test_dispatch_buffer().is_empty());
    }

    /// A worker synth key does NOT trigger the respawn hook - the
    /// `worker_`-prefix guard in `parse_project_lead_synth_key` is what
    /// stops it.
    ///
    /// The fixture names the project exactly what a guard-less parse
    /// would extract, so the name resolves and the guard is the only
    /// thing left between the key and a dispatch. Naming it anything
    /// else makes the assertion hold for the wrong reason: the lookup
    /// is an exact match, so an unresolvable name returns before the
    /// hook could dispatch, and deleting the guard still passes. A
    /// persisted row is necessary for this to have teeth and is not
    /// sufficient.
    #[test]
    fn worker_connected_does_not_trigger_respawn() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        let dir = tempfile::tempdir().expect("tempdir");
        workspace.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        // What `__spawn_worker_wp_planner_abc__` parses to without the guard.
        let lookalike = "worker_wp_planner_abc";
        workspace.seed_test_project(lookalike, "/tmp/wp");
        let project_key = ProjectKey::new(
            forge_agent::userdata::catalog::scan::project_key_for_directory(Some("/tmp/wp")),
        );
        let _ = workspace.persist_dynamic_worker(&crate::store::dynamic_workers::DynamicWorker {
            project_key: project_key.as_str().to_owned(),
            label: "planner".to_owned(),
            charter: "charter for planner".to_owned(),
            kick: None,
            resume_kick: None,
            interactive: false,
        });
        workspace.enable_test_dispatch_intercept();

        let worker_synth = SessionKey::from_session_id(format!("__spawn_{lookalike}__"));
        on_connected_for_test(&workspace, &worker_synth, "worker-uuid");

        let dispatched = workspace.drain_test_dispatch_buffer();
        assert!(
            dispatched.iter().all(|c| !matches!(c, Command::SpawnWorker { .. })),
            "the worker-shaped key must not be parsed as a lead",
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

        let lead_synth = synth_lead_key("proj-x");

        // First Connected: triggers the respawn (1 SpawnWorker).
        on_connected_for_test(&workspace, &lead_synth, "lead-uuid");
        let after_first = workspace.drain_test_dispatch_buffer();
        let first_spawns: usize =
            after_first.iter().filter(|c| matches!(c, Command::SpawnWorker { .. })).count();
        assert_eq!(first_spawns, 1, "first Connected respawns the workers");

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

    /// Helper: insert a live ad-hoc worker carrying `kick` under the
    /// seeded project, returning its synth key for `on_connected_for_test`.
    #[cfg(test)]
    fn seed_adhoc_worker_with_kick(
        workspace: &Arc<Workspace>,
        label: &str,
        kick: Option<String>,
    ) -> SessionKey {
        workspace.seed_test_project("forge", "/tmp/forge");
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

    /// A worker spawned with `workers__spawn(kick=...)` gets that kick
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
            assert_eq!(key.as_str(), "worker-uuid", "kick targets the worker's real session id");
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
    /// until the lead sends a workers__tell.
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
        let project_key = workspace
            .list_projects()
            .into_iter()
            .find(|v| v.name == "forge")
            .expect("seeded project")
            .key;
        let worker_synth = SessionKey::from_session_id(format!(
            "__spawn_worker_{}_scratchpad_abc__",
            project_key.as_str()
        ));

        on_connected_for_test(&workspace, &worker_synth, "worker-uuid");
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        let dispatched = workspace.drain_test_dispatch_buffer();
        assert!(
            dispatched.iter().all(|c| !matches!(c, Command::Prompt { .. })),
            "a label with no entry of its own must not be handed another worker's kick",
        );
    }

    /// The guard directly, with no preconditions to decay: a lead key
    /// yields its project name, a worker-shaped key yields nothing. The
    /// second case is what the hook relies on, and unlike the fixture
    /// test it cannot be defeated by a project that fails to resolve.
    #[test]
    fn parse_project_lead_synth_key_rejects_the_worker_shape() {
        let lead = SessionKey::from_session_id("__spawn_forge__");
        assert_eq!(parse_project_lead_synth_key(&lead).as_deref(), Some("forge"));

        let worker = SessionKey::from_session_id("__spawn_worker_wp_planner_abc__");
        assert_eq!(parse_project_lead_synth_key(&worker), None);
    }

    /// The boundary the doc claims: a project literally named `worker_*`
    /// parses as a lead until its segment count reaches the worker
    /// shape. Nothing else checks this, so the doc was the only record.
    #[test]
    fn parse_project_lead_synth_key_keeps_a_short_worker_prefixed_name() {
        let short = SessionKey::from_session_id("__spawn_worker_foo__");
        assert_eq!(parse_project_lead_synth_key(&short).as_deref(), Some("worker_foo"));
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

    /// #695: a label may contain underscores, so the label is everything
    /// BETWEEN the project and uuid segments rather than the second one.
    /// Both shapes, because a resume mis-parse costs the kick on every
    /// restart rather than once at spawn.
    #[test]
    fn parse_worker_synth_key_keeps_underscores_in_the_label() {
        assert_eq!(
            parse_worker_synth_key(&SessionKey::from_session_id(
                "__spawn_worker_forge_code_review_abc123__"
            )),
            Some(("forge".to_owned(), "code_review".to_owned())),
            "an underscore in the label must not truncate it on spawn",
        );
        assert_eq!(
            parse_worker_synth_key(&SessionKey::from_session_id(
                "__resume_worker_forge_code_review_abc123__"
            )),
            Some(("forge".to_owned(), "code_review".to_owned())),
            "an underscore in the label must not truncate it on resume",
        );
    }
}
