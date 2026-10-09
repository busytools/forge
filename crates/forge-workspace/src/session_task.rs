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
use crate::domain_session::{DomainSession, PendingAutoContinue};
use crate::protocol::{Command, PendingInteractionSlot, PromptSource, SessionUpdate};
use crate::update_fanout::UpdateFanout;

pub(crate) struct SessionTask {
    pub(crate) key: SessionSlot,
    pub(crate) handle: Arc<AgentHandle>,
    pub(crate) command_rx: mpsc::UnboundedReceiver<Command>,
    pub(crate) domain: Arc<Mutex<DomainSession>>,
    pub(crate) update_tx: UpdateFanout,
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
    /// The conversation this task is carrying, kept so
    /// `Command::ReplayConversation` can hand it to a consumer that joined
    /// after this session started.
    ///
    /// **Held here because here is where it is produced.** The task is handed
    /// the history at connect and emits every frame after it, so what it keeps
    /// is in one order with the frames around it. A consumer reading the
    /// transcript instead would be a second producer, and a read and a stream
    /// cannot be reconciled: the read picks up rows written while it runs,
    /// which the stream also delivers.
    ///
    /// It is the same conversation a view holds, so the cost is proportional
    /// to what is RUNNING rather than to what is being read - which is the
    /// price of having one producer rather than two.
    pub(crate) conversation: Option<(Vec<forge_primitives::Message>, u32)>,
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
            let moved = apply_event_to_domain(&mut guard, &event);
            // The two sets that move whole are announced from the scope the
            // fold wrote in, so what goes out is what the core now holds -
            // and only when the fold moved it.
            if moved.monitors {
                self.emit(SessionUpdate::MonitorsChanged {
                    key: self.key.clone(),
                    monitors: guard.monitors.clone(),
                });
            }
            if moved.background_tasks {
                self.emit(SessionUpdate::BackgroundTasksChanged {
                    key: self.key.clone(),
                    tasks: guard.background_tasks.clone(),
                });
            }
            if moved.commands {
                self.emit(SessionUpdate::SlashCommandsChanged {
                    key: self.key.clone(),
                    commands: guard.available_commands.clone(),
                });
            }
            if moved.agents {
                self.emit(SessionUpdate::SubagentsChanged {
                    key: self.key.clone(),
                    subagents: guard.available_agents.clone(),
                });
            }
            if moved.dispatches {
                self.emit(SessionUpdate::DispatchesChanged {
                    key: self.key.clone(),
                    has_dispatches: guard.has_dispatches,
                });
            }
            if moved.cards {
                self.emit(SessionUpdate::SubagentCardsChanged {
                    key: self.key.clone(),
                    cards: guard.cards_snapshot.clone(),
                });
            }
            if moved.processes {
                self.emit(SessionUpdate::ProcessesChanged {
                    key: self.key.clone(),
                    // The walk the store now holds, which the clear left
                    // empty: the shape a client draws no section from.
                    snapshot: forge_agent::env::processes::ProcessSnapshot {
                        processes: Vec::new(),
                        scanned_at: std::time::SystemTime::now(),
                    },
                });
            }
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
                // Kept before the update takes it, so a consumer that joins
                // later can be handed what this task was handed.
                //
                // **The window of it rather than a clone of it.** A resume
                // hands over the whole transcript, and what is kept here
                // answers a replay - which its reader cuts to the same window
                // anyway. Cloning the whole history to keep a window of it is
                // a transcript-sized allocation per connect.
                self.conversation =
                    Some((crate::conversation_window::tail_of(&history), compaction_count));
                // The slot the task was spawned under, which the CLI's
                // own id never moves: it names the occupant, this names
                // the seat.
                let key = self.key.clone();
                // The session came up, so whatever failed here last is
                // over. This arm is where that clears rather than the
                // spawn entry: production hoists a domain straight into
                // `domain_handles` and never registers one, so a clear on
                // registration would never run at all.
                if let Some(workspace) = self.workspace.upgrade() {
                    workspace.clear_spawn_failure(&key);
                }
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
                    // Each one is announced: a view drawing a dock from the
                    // request it folded would otherwise keep offering a
                    // prompt the core has let go.
                    let dropped: Vec<(String, Option<u64>)> = self
                        .domain
                        .lock()
                        .pending_interactions
                        .drain()
                        .map(|(tool_id, slot)| {
                            // A question names the round it was parked as, so
                            // the views dropping it drop the same one.
                            let question_index = match slot {
                                PendingInteractionSlot::Question { request, .. } => {
                                    Some(request.question_index)
                                }
                                PendingInteractionSlot::Permission { .. } => None,
                            };
                            (tool_id, question_index)
                        })
                        .collect();
                    for (tool_id, question_index) in dropped {
                        self.emit(SessionUpdate::PendingInteractionResolved {
                            key: self.key.clone(),
                            tool_id,
                            question_index,
                        });
                    }
                    // Everything parked for the replaced identity has no
                    // session left to drain it, so the bucket is dropped
                    // and any message in it is acknowledged back to its
                    // sender.
                    if let Some(workspace) = self.workspace.upgrade() {
                        workspace.expire_parked_for_slot(
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
                // Before the release below takes the session with it: a
                // view reading the slot afterwards has nothing left to say
                // the attempt happened.
                if let Some(workspace) = self.workspace.upgrade() {
                    workspace.record_spawn_failure(&key, &message);
                }
                // A `/new` or `/resume` that fails to respawn ends the
                // live turn without a Result, so flush the turn's own
                // bookkeeping here.
                let caller = self.domain.lock().key.clone();
                self.drain_review_activity_for(&caller);
                // A spawn that never connected still holds everything
                // parked for its slot: drop the bucket and acknowledge any
                // message in it back to its sender, before the
                // user-visible ConnectionFailed lands.
                if let Some(workspace) = self.workspace.upgrade() {
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
                        PendingInteractionSlot::Permission {
                            tx: response_tx,
                            request: Box::new(wire_request.clone()),
                        },
                    );
                }
                let answerable = self.update_tx.send_answering(SessionUpdate::PermissionRequest {
                    key: self.key.clone(),
                    tool_id: tool_call_id.clone(),
                    request: wire_request,
                });
                if answerable {
                    spawn_permission_response_forwarder(
                        Arc::clone(&self.handle),
                        response_rx,
                        session_id,
                        tool_call_id,
                    );
                } else {
                    // No subscriber can answer a permission prompt -
                    // either none is attached or none renders one.
                    // Resolve the orphaned oneshot with Cancelled so the
                    // SDK callback unblocks rather than hanging the
                    // `claude` subprocess turn forever.
                    if let Some(pending) =
                        self.domain.lock().pending_interactions.remove(&tool_call_id)
                        && let PendingInteractionSlot::Permission { tx, .. } = pending
                    {
                        let _ = tx.send(forge_primitives::PermissionOutcome::Cancelled);
                        // An observer that folded the request keeps drawing
                        // it otherwise, and its answer reaches nothing.
                        self.emit(SessionUpdate::PendingInteractionResolved {
                            key: self.key.clone(),
                            tool_id: tool_call_id.clone(),
                            question_index: None,
                        });
                    }
                    tracing::warn!(
                        target: "forge_workspace::session_task",
                        slot = %self.key.display(),
                        tool_id = %tool_call_id,
                        "no subscriber can answer the PermissionRequest; orphaned oneshot cancelled"
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
                        PendingInteractionSlot::Question {
                            tx: response_tx,
                            request: Box::new(wire_request.clone()),
                        },
                    );
                }
                let answerable = self.update_tx.send_answering(SessionUpdate::QuestionRequest {
                    key: self.key.clone(),
                    tool_id: tool_call_id.clone(),
                    request: wire_request,
                });
                if answerable {
                    spawn_question_response_forwarder(
                        Arc::clone(&self.handle),
                        response_rx,
                        session_id,
                        tool_call_id,
                    );
                } else {
                    // No subscriber can answer a question - either none
                    // is attached or none renders one. Resolve the
                    // orphan with Cancelled so the SDK callback
                    // unblocks rather than hanging the turn.
                    if let Some(pending) =
                        self.domain.lock().pending_interactions.remove(&tool_call_id)
                        && let PendingInteractionSlot::Question { tx, request, .. } = pending
                    {
                        let _ = tx.send(forge_primitives::QuestionOutcome::Cancelled);
                        // An observer that folded the request keeps drawing
                        // it otherwise, and its answer reaches nothing.
                        self.emit(SessionUpdate::PendingInteractionResolved {
                            key: self.key.clone(),
                            tool_id: tool_call_id.clone(),
                            question_index: Some(request.question_index),
                        });
                    }
                    tracing::warn!(
                        target: "forge_workspace::session_task",
                        slot = %self.key.display(),
                        tool_id = %tool_call_id,
                        "no subscriber can answer the QuestionRequest; orphaned oneshot cancelled"
                    );
                }
            }
            AgentEvent::McpOperationError { error, .. } => {
                self.emit(SessionUpdate::McpOperationError { key: self.key.clone(), error });
            }
            AgentEvent::PromptCancelResolved { uuid, cancelled } => {
                // A confirmed cancel drops the row here as well as at the
                // client: the CLI's own terminal frame for the prompt may be
                // the only other word, and a reader that asked to drop it must
                // not be left guessing.
                if cancelled {
                    self.domain.lock().drop_queued_prompt(&uuid);
                }
                self.emit(SessionUpdate::PromptCancelResolved {
                    key: self.key.clone(),
                    uuid,
                    cancelled,
                });
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
            AgentEvent::TurnError { message, class, .. } => {
                self.emit(SessionUpdate::TurnError {
                    key: self.key.clone(),
                    message,
                    class: Some(class),
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
                // The init frame is where the CLI says what it can do. The
                // pile needs `msg_lifecycle_v1`: without it nothing settles a
                // queued row, so the seat stops recording them rather than
                // birthing cards that can never drain. Read here rather than
                // in a view, because what a view then draws follows from what
                // the core recorded.
                if let forge_primitives::Message::System { subtype, data, .. } = &msg
                    && subtype == "init"
                {
                    let advertised =
                        data.get("capabilities").and_then(serde_json::Value::as_array).is_some_and(
                            |caps| caps.iter().any(|cap| cap.as_str() == Some("msg_lifecycle_v1")),
                        );
                    let first_sight = {
                        let mut guard = self.domain.lock();
                        let first = guard.lifecycle_frames.is_none();
                        if first {
                            guard.lifecycle_frames = Some(advertised);
                        }
                        first
                    };
                    if first_sight && !advertised {
                        tracing::warn!(
                            target: "forge_workspace::session_task",
                            slot = %self.key.display(),
                            event_name = "lifecycle_frames_absent",
                            outcome = "pile_disabled",
                            "this session's CLI does not advertise msg_lifecycle_v1, so queued \
                             prompts cannot be followed here; the pile is not drawn for this seat",
                        );
                    }
                }
                // A prompt's queue state is state, not conversation: the
                // prompt's own presence in the record is the transcript's
                // enqueue row and the `queued_command` attachment, and
                // retaining three frames per prompt would put three rows for
                // it into every view's transcript under rule 25. It advances
                // the pile and is emitted, then the conversation path below is
                // skipped entirely.
                if let forge_primitives::Message::CommandLifecycle { command_uuid, state, .. } =
                    &msg
                {
                    let known = { self.domain.lock().advance_queued_prompt(command_uuid, state) };
                    // Only the states that IMPLY a row: a prompt's `completed`
                    // or `cancelled` follows the row's own removal at
                    // `started`, so logging those would be one false alarm per
                    // prompt, and an alarm nobody can act on is noise.
                    if !known && matches!(state.as_str(), "queued" | "started") {
                        tracing::debug!(
                            target: "forge_workspace::session_task",
                            slot = %self.key.display(),
                            uuid = %command_uuid,
                            state = %state,
                            "lifecycle frame for a prompt this session never recorded",
                        );
                    }
                    self.emit(SessionUpdate::PromptLifecycle {
                        key: self.key.clone(),
                        uuid: command_uuid.clone(),
                        state: state.clone(),
                    });
                } else {
                    // The frame joins the conversation this task is carrying,
                    // BEFORE it is emitted: the replay answers with that
                    // conversation, so a frame missing from it is a frame a
                    // consumer joining later never sees - and a compaction is
                    // the case that makes it plain, because the boundary drops
                    // the transport's copy and the replay is what rebuilds it.
                    self.retain(&msg);
                    // `None`: a frame off the wire is the CLI's own, and
                    // carries no prompt origin.
                    self.emit(SessionUpdate::ChatAppended {
                        key: self.key.clone(),
                        msg,
                        origin: None,
                    });
                }
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

    /// Record a prompt as waiting and announce it: the row's words and sender
    /// ride [`SessionUpdate::PromptQueued`], where the lifecycle frames carry
    /// only the id and the state.
    ///
    /// Nothing is recorded for a session whose CLI has said it does not carry
    /// those frames: a row nothing can settle is worse than no row, and that
    /// session draws its prompt the way it did before the pile existed.
    fn record_queued(&self, uuid: &str, source: PromptSource, text: &str) {
        {
            let mut guard = self.domain.lock();
            if guard.lifecycle_frames == Some(false) {
                return;
            }
            guard.record_queued_prompt(uuid, source, text);
        }
        self.emit(SessionUpdate::PromptQueued {
            key: self.key.clone(),
            uuid: uuid.to_owned(),
            source,
            text: text.to_owned(),
        });
    }

    /// How many lines of a call's own output a read answers with, matching
    /// the terminal's Monitor tail: the row is about what the command is
    /// doing, not a transcript of everything it printed.
    const CALL_OUTPUT_MAX_LINES: usize = 12;

    fn execute_command(&self, cmd: Command) {
        // A prompt's queued row is recorded here, where its id is minted (or
        // is already the caller's) and its source is known: the lifecycle
        // frames that follow name only the id and the state, so this is the
        // one moment that can say what is waiting and who sent it.
        let cmd = match cmd {
            Command::Prompt { key, text, attachments } => {
                let uuid = forge_sdk::request_id::next_prompt_id();
                self.record_queued(&uuid, PromptSource::You, &text);
                Command::PromptUnder { key, text, attachments, uuid, source: PromptSource::You }
            }
            Command::PromptUnder { key, text, attachments, uuid, source } => {
                self.record_queued(&uuid, source, &text);
                Command::PromptUnder { key, text, attachments, uuid, source }
            }
            other => other,
        };
        match cmd {
            Command::RespondPermission { key: _, tool_id, outcome } => {
                // Peek the slot kind first; only remove on a kind
                // match so a mismatched response leaves the real
                // waiter intact.
                let mut guard = self.domain.lock();
                let kind_matches = matches!(
                    guard.pending_interactions.get(&tool_id),
                    Some(PendingInteractionSlot::Permission { .. }),
                );
                if kind_matches
                    && let Some(PendingInteractionSlot::Permission { tx, .. }) =
                        guard.pending_interactions.remove(&tool_id)
                {
                    drop(guard);
                    if tx.send(outcome).is_err() {
                        tracing::warn!(
                            target: "forge_workspace::session_task",
                            slot = %self.key.display(),
                            tool_id = %tool_id,
                            "permission oneshot receiver dropped before response could be sent"
                        );
                    }
                    self.emit(SessionUpdate::PendingInteractionResolved {
                        key: self.key.clone(),
                        tool_id,
                        question_index: None,
                    });
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
                    Some(PendingInteractionSlot::Question { .. }),
                );
                if kind_matches
                    && let Some(PendingInteractionSlot::Question { tx, request, .. }) =
                        guard.pending_interactions.remove(&tool_id)
                {
                    drop(guard);
                    if tx.send(outcome).is_err() {
                        tracing::warn!(
                            target: "forge_workspace::session_task",
                            slot = %self.key.display(),
                            tool_id = %tool_id,
                            "question oneshot receiver dropped"
                        );
                    }
                    self.emit(SessionUpdate::PendingInteractionResolved {
                        key: self.key.clone(),
                        tool_id,
                        question_index: Some(request.question_index),
                    });
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
            // Answered by this task rather than the agent: the conversation
            // is the task's own, so it never leaves this loop.
            Command::ReplayConversation { key: _ } => {
                let Some((history, compaction_count)) = self.conversation.as_ref() else {
                    tracing::debug!(
                        target: "forge_workspace::session_task",
                        slot = %self.key.display(),
                        "a replay was asked for before this session connected; nothing to hand back",
                    );
                    return;
                };
                self.emit(SessionUpdate::HistoryReplayed {
                    key: self.key.clone(),
                    history: history.clone(),
                    compaction_count: *compaction_count,
                });
            }
            // Answered here for the same reason a replay is: the frame that
            // named the call's output file is in the conversation this task
            // carries, and nowhere a read could reach without walking the
            // transcript. A call the conversation no longer names - or the
            // window has carried off - is the no-path reason, not a blank.
            Command::ReadCallOutput { key: _, call_id } => {
                let path = self.conversation.as_ref().and_then(|(history, _)| {
                    history.iter().find_map(|message| match message {
                        forge_primitives::Message::TaskNotification {
                            tool_use_id: Some(id),
                            output_file,
                            ..
                        // An empty path is a path the frame does not carry:
                        // the wire sends `output_file: ""` for a task with no
                        // file, and reading that as a path would answer
                        // "gone" about a file that never existed.
                        } if id == &call_id && !output_file.is_empty() => {
                            Some(output_file.clone())
                        }
                        _ => None,
                    })
                });
                let output = match path {
                    None => forge_primitives::CallOutput::NoPath,
                    Some(path) => match crate::output_tail::read_output_file_tail(
                        std::path::Path::new(&path),
                        Self::CALL_OUTPUT_MAX_LINES,
                    ) {
                        None => forge_primitives::CallOutput::FileGone,
                        Some(lines) => forge_primitives::CallOutput::Lines(lines),
                    },
                };
                self.emit(SessionUpdate::CallOutput { key: self.key.clone(), call_id, output });
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
                // with no idea what it is for. The launch settings for a
                // `/new` are built without it, so re-deliver the charter
                // the store holds for this slot.
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

    /// Send `update` to the workspace fan-in; log when no subscriber
    /// took it, so a regression there leaves a trail.
    // TODO(ved): gate emits on a per-task session epoch so a superseded
    // task (its slot re-spawned by a resume or an account switch) can't
    // emit a stale update onto its successor during the brief
    // post-supersession drain. Slot keying neither creates nor widens
    // that window - the successor holds the same slot either way - and
    // the switch stays idle-gated, so it is low-risk today.
    fn emit(&self, update: SessionUpdate) {
        if !self.update_tx.send(update) {
            tracing::warn!(
                target: "forge_workspace::session_task",
                slot = %self.key.display(),
                "no SessionUpdate subscriber took the event"
            );
        }
    }

    /// Add one frame to the conversation this task is carrying.
    ///
    /// **The accumulator, and it is what makes a replay a conversation rather
    /// than a photograph of the connect.** The history handed over at connect
    /// is empty for a fresh session and stops at the connect for a resumed
    /// one, so a replay answering with it alone would hand a consumer joining
    /// late a seat that never spoke - worse than the read it replaced.
    ///
    /// A compaction boundary bumps the count rather than the frame list: the
    /// CLI's boundary frame is a `ChatAppended` like any other and stays in
    /// the conversation, and the count is what a view draws its marker from.
    ///
    /// **And the copy is a window, not the run.** Its one reader cuts it to
    /// the newest turns, so a session up for days would otherwise carry a run
    /// that nothing reads (see [`crate::conversation_window`]).
    fn retain(&mut self, message: &forge_primitives::Message) {
        let Some((history, compaction_count)) = self.conversation.as_mut() else {
            // Before the first connect there is no conversation to add to,
            // and no frame to add: the CLI says nothing until it connects.
            return;
        };
        if matches!(message, forge_primitives::Message::CompactBoundary { .. }) {
            *compaction_count = compaction_count.saturating_add(1);
        }
        // Before the push, so the drop pays for the room it makes rather than
        // for a doubling it is about to throw away.
        if history.len()
            >= crate::conversation_window::CONVERSATION_CAP
                + crate::conversation_window::CONVERSATION_SLACK
        {
            crate::conversation_window::drop_past_cap(history);
        }
        history.push(message.clone());
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
    /// carries the `[Message id=m-…]` wrapper
    /// that `forge_server::envelope::detect_inbound` matches, which
    /// the chat renders as a styled peer block.
    fn deliver_parked_peers(
        &self,
        workspace: &Arc<crate::Workspace>,
        pending: Vec<crate::parked::ParkedPeer>,
    ) {
        if pending.is_empty() {
            return;
        }
        // Same typed peer-envelope echo the running-target dispatch path
        // does. Fire BEFORE the LLM-side dispatch so the user-turn
        // ordering is natural. The sender is not used here: it rides the
        // parked entry for the expiry path alone.
        for entry in pending {
            let crate::parked::ParkedPeer { wrapped, .. } = entry;
            let uuid = forge_sdk::request_id::next_prompt_id();
            crate::spawn::push_peer_user_turn_into_chat(workspace, &self.key, &wrapped, &uuid);
            let text = wrapped.to_prose();
            if let Err(err) =
                workspace.dispatch_workspace_prompt_under(&self.key, text, PromptSource::Peer, uuid)
            {
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
            let uuid = forge_sdk::request_id::next_prompt_id();
            crate::spawn::push_cron_prompt_into_chat(
                workspace,
                &self.key,
                &text,
                &uuid,
                &cron.cron_id,
                cron.description.as_deref(),
            );
            if let Err(err) =
                workspace.dispatch_workspace_prompt_under(&self.key, text, PromptSource::Cron, uuid)
            {
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
            let uuid = forge_sdk::request_id::next_prompt_id();
            crate::spawn::push_gotify_notification_into_chat(
                workspace,
                &self.key,
                &notification,
                &uuid,
            );
            if let Err(err) = workspace.dispatch_workspace_prompt_under(
                &self.key,
                notification.to_prose(),
                PromptSource::Gotify,
                uuid,
            ) {
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
            let uuid = forge_sdk::request_id::next_prompt_id();
            crate::spawn::push_slack_message_into_chat(workspace, &self.key, &prose, &uuid);
            if let Err(err) = workspace.dispatch_workspace_prompt_under(
                &self.key,
                prose,
                PromptSource::Slack,
                uuid,
            ) {
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
/// crash, panic), drop whatever is parked for this session's slot - a
/// message waiting on a session that no longer exists is acknowledged
/// back to its sender rather than delivered to nobody.
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
            workspace.expire_parked_for_slot(
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
        // Answered by the task before it reaches here: the conversation is
        // the task's own and never crosses to the agent. A caller reading
        // this as dead code should note the arm in `SessionTask::execute_command`
        // is what returns early - the match here is exhaustive, not a route.
        Command::ReplayConversation { key: _ } | Command::ReadCallOutput { .. } => Ok(()),
        Command::Prompt { key: _, text, attachments } => {
            let Some(sid) = session_id else {
                return Err(warn_no_session(key, "Prompt"));
            };
            // The live path rewrites `Prompt` into `PromptUnder` before it gets
            // here, so this arm is the test fallback's alone: it mints without
            // recording, because the fallback holds no session state to record
            // into.
            let uuid = forge_sdk::request_id::next_prompt_id();
            handle.prompt_with_images(sid.to_owned(), text, attachments, uuid)
        }
        Command::PromptUnder { key: _, text, attachments, uuid, source: _ } => {
            let Some(sid) = session_id else {
                return Err(warn_no_session(key, "PromptUnder"));
            };
            handle.prompt_with_images(sid.to_owned(), text, attachments, uuid)
        }
        Command::Cancel { key: _ } => {
            let Some(sid) = session_id else {
                return Err(warn_no_session(key, "Cancel"));
            };
            handle.cancel(sid.to_owned())
        }
        Command::CancelQueuedPrompt { key: _, uuid } => {
            let Some(sid) = session_id else {
                return Err(warn_no_session(key, "CancelQueuedPrompt"));
            };
            handle.cancel_queued(sid.to_owned(), uuid)
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
        | Command::DictateStream { .. }
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
        | Command::RespondBrowserHandOff { .. }
        | Command::DictateCatalogueCheck
        | Command::DictateInstall { .. }
        | Command::DictateActivate { .. }
        | Command::DictateDeactivate { .. }
        | Command::DictateBench { .. }
        | Command::DictateBenchStop
        | Command::DictateReadAloudStart { .. }
        | Command::DictateReadAloudStop { .. }
        | Command::DictateReadAloudDelete { .. }
        | Command::DictateBenchDelete { .. }
        | Command::DictateUninstall { .. }
        | Command::OpenUrl { .. }
        | Command::SaveReviewThreads { .. }
        | Command::RemoveReviewThread { .. }
        | Command::SetReviewThreadStatus { .. }
        | Command::CloseSession { .. }
        | Command::UpsertReviewThread { .. }
        | Command::SubmitReview { .. }
        | Command::TaskVerdict { .. }
        | Command::TaskAnswer { .. }
        | Command::TaskRank { .. }
        | Command::TaskAssign { .. }
        | Command::TaskCreate { .. }
        | Command::TaskMove { .. }) => {
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

/// The wire subtype of the retry frame, where the CLI names an
/// authentication failure on the retry path. The TUI matches the same
/// literal.
const API_RETRY_SUBTYPE: &str = "api_retry";

/// How long a failed turn waits for a reader before its nudge goes out.
/// The terminal's own continuation waits 5s before its first attempt; the
/// same window here is what gives a reader time to open the seat first.
/// One prompt, not a ladder: the terminal retries (5s / 20s / 60s), and
/// this fire descends to the once-per-unopened-failure rule instead.
pub(crate) const AUTO_CONTINUE_DELAY: std::time::Duration = std::time::Duration::from_secs(5);

/// What the failure is called in the nudge: the CLI's own errors when the
/// result frame reported any, else its subtype, else a plain fallback.
pub(crate) fn failure_reason(errors: Option<&[String]>, subtype: &str) -> String {
    if let Some(errors) = errors.filter(|errors| !errors.is_empty()) {
        return errors.join("; ");
    }
    if !subtype.is_empty() && subtype != "success" {
        return subtype.to_owned();
    }
    "an error".to_owned()
}

/// What one event MOVED in the domain's whole sets.
///
/// **A set that did not change is not news.** Each moves whole and on discrete
/// frames, and every frame of a busy seat reaches this fold - so a viewer
/// redrawing on each one would pay for nothing, and the answer is taken by
/// comparing the sets the fold was handed with the ones it left.
///
/// The walk joins them on the death only: nothing else in this fold writes it
/// (the seat's own loop and the terminal's scanner do), so the flag is the one
/// transition this fold makes - a snapshot that was held and is now gone.
pub(crate) struct Moved {
    pub monitors: bool,
    pub background_tasks: bool,
    pub processes: bool,
    pub commands: bool,
    pub agents: bool,
    pub dispatches: bool,
    pub cards: bool,
}

/// Apply an [`AgentEvent`] to a [`DomainSession`]. Pure mutation; no
/// I/O, no async, no sends. Called from inside
/// [`SessionTask::translate_event`] under the domain's lock.
///
/// Workspace owns the routing metadata every dispatch needs
/// (`session_id`) plus the facts a view reads through the view
/// surface. Operational state a view renders from the update stream
/// itself (lifecycle, cwd, account info) stays on the view.
///
/// The answer is what the caller announces, so it is the sets as they stand
/// AFTER the fold rather than an opinion about the event.
pub(crate) fn apply_event_to_domain(domain: &mut DomainSession, event: &AgentEvent) -> Moved {
    let held_monitors = domain.monitors.clone();
    let held_tasks = domain.background_tasks.clone();
    let held_walk = domain.process_snapshot.is_some();
    let held_commands = domain.available_commands.clone();
    let held_agents = domain.available_agents.clone();
    let held_dispatches = domain.has_dispatches;
    let held_cards = domain.cards_snapshot.clone();
    hold_view_facts(domain, event);
    if let AgentEvent::ConnectionFailed { .. } = event {
        // The subprocess is gone - drop the runtime/turn mirrors so the
        // in-flight guards don't read a stale turn. The failure mark goes
        // with them: the row reads its failure from the lifecycle.
        domain.runtime_state = None;
        domain.turn_pending = false;
        domain.turn_open = false;
        domain.pending_cancel = false;
        domain.failed_turn_at = None;
        domain.auto_continue = None;
        domain.auto_continue_spent = false;
        // No terminal `background_tasks_changed` follows a dead session,
        // so the last snapshot would stand forever - and the registry with
        // it, spinning rows over tasks a dead process never finished.
        domain.drop_background_tasks();
        // The walk describes the subprocess's tree, so it goes with the
        // process: left standing it draws rows for processes that are not
        // there, and nothing follows a dead session that would replace it.
        domain.process_snapshot = None;
    }
    if let AgentEvent::SdkMessage {
        msg: forge_primitives::Message::Result { is_error, subtype, errors, .. },
        ..
    } = event
    {
        // The stamp is spent on every outcome: a cancel that raced a turn
        // the CLI had already finished must not exempt the next failure.
        let cancelled = std::mem::take(&mut domain.pending_cancel);
        // An errored `Result` marks the slot for the rail, unless the
        // reader asked for the interruption - a cancel ends with the same
        // failed result a genuine error does.
        if *is_error && !cancelled {
            let at = std::time::SystemTime::now();
            domain.failed_turn_at = Some(at);
            // A transient server error is the terminal's own dead-turn
            // path, which continues it with its own prompt; arming here too
            // would double-fire. Everything else is this nudge's to answer.
            let terminal_owns_it = matches!(
                domain.last_api_retry,
                Some((forge_primitives::ApiRetryError::ServerError, _))
            );
            if !terminal_owns_it && !domain.auto_continue_spent {
                domain.auto_continue = Some(PendingAutoContinue {
                    due_at: at + AUTO_CONTINUE_DELAY,
                    reason: failure_reason(errors.as_deref(), subtype),
                });
            }
        } else if !*is_error {
            // A turn that finished ends the episode: a later failure is a
            // new one and gets its own nudge.
            domain.auto_continue = None;
            domain.auto_continue_spent = false;
            domain.last_api_retry = None;
        }
    }
    if let AgentEvent::SdkMessage { msg: forge_primitives::Message::Error { .. }, .. } = event {
        // A transport death ends the turn too; nothing marks for it, but
        // the stamp must not outlive the turn it was armed for.
        domain.pending_cancel = false;
    }
    // The snapshot carries the whole set, so mirroring it is an
    // assignment and an empty one clears.
    if let AgentEvent::SdkMessage {
        msg: forge_primitives::Message::BackgroundTasksChanged { tasks, .. },
        ..
    } = event
    {
        domain.background_work = !tasks.is_empty();
        let parsed: Vec<crate::BackgroundTask> = tasks
            .iter()
            .filter_map(|task| {
                let entry = task.as_object()?;
                Some(crate::BackgroundTask {
                    task_id: entry.get("task_id")?.as_str()?.to_owned(),
                    task_type: entry.get("task_type")?.as_str()?.to_owned(),
                    description: entry.get("description")?.as_str()?.to_owned(),
                    command: None,
                    tool_use_id: None,
                })
            })
            .collect();
        if parsed.len() != tasks.len() {
            tracing::debug!(
                target: "forge_workspace::session_task",
                event_name = "background_tasks_parse_dropped",
                dropped = tasks.len() - parsed.len(),
                entry_count = tasks.len(),
                "background_tasks_changed dropped unparseable entries; possible wire drift",
            );
        }
        domain.replace_background_tasks(parsed);
    }
    // The command a card carried, which is what the OS scan adopts a detached
    // process by and what tells a view that a rostered task has a row to draw.
    //
    // ANY card that carries a command, not only one whose input asked to run
    // in the background: the CLI backgrounds a bash three ways, and two of
    // them (`backgroundedByUser`, `assistantAutoBackgrounded`) cannot show on
    // the card. The registry and the `task_started` link are what scope it -
    // a command no roster entry resolves to is never read.
    if let AgentEvent::SdkMessage {
        msg: forge_primitives::Message::Assistant { message, .. },
        ..
    } = event
    {
        // Content means the turn went on, so a retry it recovered past is not
        // the classification of its failure. The terminal drops its own copy
        // on the same content; left standing here, a stale server_error would
        // exempt a later failure of a different kind from the nudge.
        domain.last_api_retry = None;
        for block in &message.content {
            let forge_primitives::ContentBlock::ToolUse { id, input, .. } = block else {
                continue;
            };
            if let Some(command) = input.get("command").and_then(serde_json::Value::as_str) {
                domain.hold_background_command(id.clone(), command.to_owned());
            }
        }
    }
    // `task_started` is what links a task to the card that began it, and so
    // to the command.
    if let AgentEvent::SdkMessage {
        msg: forge_primitives::Message::TaskStarted { task_id, tool_use_id: Some(tool_use_id), .. },
        ..
    } = event
    {
        domain.hold_task_tool_use(task_id.clone(), tool_use_id.clone());
    }
    if let AgentEvent::Connected { session_id, .. } = event {
        domain.session_id = Some(SessionId::new(session_id.clone()));
        domain.runtime_state = None;
        domain.turn_pending = false;
        domain.turn_open = false;
        // The failure mark named the last occupant's turn, and so did any
        // interrupt it was holding.
        domain.pending_cancel = false;
        domain.failed_turn_at = None;
        domain.last_api_retry = None;
        domain.auto_continue = None;
        domain.auto_continue_spent = false;
        // A second Connected is a new occupant in the same slot, and the
        // CLI re-sends the whole background set only when it changes: a
        // registry left standing would spin a row over a task that went
        // with the identity it belonged to.
        domain.drop_background_tasks();
        // A new occupant has advertised nothing yet: the CLI sends
        // `system/init` at the head of a turn only, so the last one's
        // catalogues would stand until this one's first message - and
        // after a login swap they came from another account's config dir.
        domain.available_commands.clear();
        domain.available_agents.clear();
        // The agent catalogue is read from the first init of a turn, and
        // a swap that ends no turn - a mid-turn `/new`, a reconnect after
        // a login - never sees the Result that re-arms it. Left armed it
        // would drop the new occupant's own init and keep the list empty
        // for a whole turn, which is the state clearing the two above is
        // meant to end.
        domain.agents_emitted_this_turn = false;
        // It connected, so it is not waiting to be let in.
        domain.awaiting_login = false;
    }
    // The login wait, from the two signals the CLI actually sends: it
    // attributes the failure to the assistant message, or names it on the
    // retry frame. `AssistantMessageError` is typed, so nothing here keys
    // on prose. `AgentEvent::AuthRequired` is constructed nowhere in the
    // tree, and its arm stays for the state's own name rather than for a
    // producer that exists.
    if let AgentEvent::SdkMessage { msg, .. } = event {
        match msg {
            forge_primitives::Message::Assistant {
                error: Some(forge_primitives::AssistantMessageError::AuthenticationFailed),
                ..
            } => domain.awaiting_login = true,
            forge_primitives::Message::System { subtype, data, .. }
                if subtype == API_RETRY_SUBTYPE =>
            {
                if let Some(update) = data
                    .as_object()
                    .and_then(forge_agent::translate::state_parsing::build_api_retry_update)
                {
                    if update.error == forge_primitives::ApiRetryError::AuthenticationFailed {
                        domain.awaiting_login = true;
                    }
                    // Kept for the failure fold below: the CLI's retries are
                    // the only place the wire says what went wrong.
                    domain.last_api_retry = Some((update.error, update.error_status));
                }
            }
            _ => {}
        }
    }
    if let AgentEvent::AuthRequired { .. } = event {
        domain.awaiting_login = true;
    }
    // A `TurnError` the classifier reads as auth-required holds the session
    // on `/login`. The class is derived where the event is built, so it is
    // read here rather than searched out of the message again, and a class
    // the producer could not name stays unclassified rather than being
    // guessed at.
    if let AgentEvent::TurnError { class, .. } = event
        && *class == forge_primitives::TurnErrorClass::AuthRequired
    {
        domain.awaiting_login = true;
    }
    // A turn that finished proves the credential works, so the wait is
    // over before any reconnect. Success only: a turn that died on the
    // missing credential ends with its own failed `Result`, and clearing
    // on that would undo the mark the frames above just set. The same
    // two-line test exists in the TUI and on the view surface, and folding
    // the three into one is its own piece.
    if let AgentEvent::SdkMessage {
        msg: forge_primitives::Message::Result { is_error, subtype, .. },
        ..
    } = event
        && !*is_error
        && subtype == "success"
    {
        domain.awaiting_login = false;
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
    // Mirror the frames' own claim about the turn, which is the one signal
    // that covers a turn's OPENING: `turn_pending` is spent by the previous
    // Result and `session_state_changed` no longer arrives, so thinking and
    // prose alone left the header reading idle while the model worked
    // (`DomainSession::turn_open` carries the measured window).
    if let AgentEvent::SdkMessage { msg, .. } = event
        && let Some(said) = crate::domain_session::liveness_of(msg)
    {
        domain.note_liveness(said);
    }
    // The two catalogues the composer's autocomplete reads, as the CLI
    // last advertised them. Held on the session so a view arriving after
    // the turn started reads one answer rather than waiting for the next
    // init frame, which is a whole turn away.
    if let AgentEvent::SdkMessage {
        msg: forge_primitives::Message::System { subtype, data, .. },
        ..
    } = event
        && subtype == "init"
        && let Some(record) = data.as_object()
    {
        if let Some(entries) = record.get("slash_commands").and_then(serde_json::Value::as_array) {
            let commands =
                forge_agent::translate::commands::map_available_commands_from_json(entries);
            // An init frame that advertises none carries nothing about
            // them, and the CLI re-fires one every turn.
            if !commands.is_empty() {
                domain.available_commands = commands;
            }
        }
        if !domain.agents_emitted_this_turn
            && let Some(agents) = record.get("agents")
        {
            domain.available_agents =
                forge_agent::translate::agents::map_available_agents_from_names(Some(agents));
            domain.agents_emitted_this_turn = true;
        }
    }
    if let AgentEvent::SdkMessage {
        msg: forge_primitives::Message::CommandsChanged { commands, .. },
        ..
    } = event
    {
        let parsed = forge_agent::translate::commands::map_available_commands_from_json(commands);
        // A payload carrying entries that parse to none means the CLI's
        // entry shape changed under us, and storing it would wipe the
        // list; a legitimately empty one clears it.
        let drift = parsed.is_empty() && !commands.is_empty();
        if drift {
            // A view reading the surface alone would otherwise lose the
            // signal the TUI's own copy of this guard logs.
            tracing::warn!(
                target: "forge_workspace::session_task",
                slot = %domain.key.display(),
                event_name = "commands_changed_parse_empty",
                message = "commands_changed carried entries but none parsed; likely wire drift, keeping prior list",
                outcome = "skipped",
                entry_count = commands.len(),
            );
        } else {
            domain.available_commands = parsed;
        }
    }
    // The turn boundary re-arms the agent read.
    if let AgentEvent::SdkMessage {
        msg: forge_primitives::Message::Result { .. } | forge_primitives::Message::Error { .. },
        ..
    } = event
    {
        domain.agents_emitted_this_turn = false;
    }
    // A frame that dispatches a sub-agent raises the flag, and it never goes
    // back: the section its flag gates lists every dispatch the conversation
    // holds, so a later frame that narrates no dispatch says nothing about
    // the ones already made.
    if let AgentEvent::SdkMessage { msg, .. } = event
        && is_dispatch(msg)
    {
        domain.has_dispatches = true;
    }
    // The instance list folds frame by frame, in this same walk: the frames
    // that open and settle a card are matched here either way, so the join
    // costs a lookup rather than a second pass over the conversation. The
    // push is change-gated on the list itself - a frame that moved nothing
    // announces nothing.
    if let AgentEvent::SdkMessage { msg, .. } = event {
        domain.card_tracker.apply(msg);
        let cards = domain.card_tracker.cards();
        if cards != domain.cards_snapshot {
            domain.cards_snapshot = cards;
        }
    }
    Moved {
        monitors: domain.monitors != held_monitors,
        background_tasks: domain.background_tasks != held_tasks,
        processes: held_walk && domain.process_snapshot.is_none(),
        commands: domain.available_commands != held_commands,
        agents: domain.available_agents != held_agents,
        // Only a FRAME raises news. A connect assigns the flag from the
        // history it carries, and the read on that same event already answers
        // it - announcing it would be a frame about a record the page is
        // being handed anyway.
        dispatches: !held_dispatches
            && domain.has_dispatches
            && matches!(event, AgentEvent::SdkMessage { .. }),
        // Live news only, same as the flag above: a connect seeds the list
        // from the history it carries and the read on that event answers it.
        cards: domain.cards_snapshot != held_cards
            && matches!(event, AgentEvent::SdkMessage { .. }),
    }
}

/// Whether one frame dispatches a sub-agent: an assistant frame that is not a
/// sub-agent's own, carrying a `Task` or `Agent` call.
///
/// **Moved here from `forge-server`'s conversation, where the record's flag
/// was computed.** The fold that can announce the raise is this one, so the
/// rule lives where the flag does rather than being mirrored a crate away -
/// same predicate, same seed over the connect's history, same raise on a
/// frame, so a record's flag and this fold's cannot disagree.
fn is_dispatch(message: &forge_primitives::Message) -> bool {
    let forge_primitives::Message::Assistant { message, parent_tool_use_id, .. } = message else {
        return false;
    };
    if forge_primitives::names_a_dispatch(parent_tool_use_id.as_deref()) {
        return false;
    }
    message.content.iter().any(|block| {
        matches!(block, forge_primitives::ContentBlock::ToolUse { name, .. }
            if name == "Task" || name == "Agent")
    })
}

/// Drop every fact that describes one run of a session: the hook's mode
/// and effort, the model it resolved, the two bridge snapshots that
/// describe a subprocess tree, and the monitor set that run started.
///
/// This mirrors the view's own reset on the same events, so what it holds
/// is what a view draws. Two facts are deliberately not here, because the
/// view's reset does not touch them either: the sub-agent attribution
/// outlives the run it came from, and the process walk is cleared on the
/// death alone - where the tree it describes goes with the subprocess -
/// rather than on every identity this clears.
fn clear_runtime_identity(domain: &mut DomainSession) {
    domain.observed_permission_mode = None;
    domain.observed_effort = None;
    domain.current_model = None;
    domain.mcp_servers = None;
    domain.context_usage = None;
    domain.monitors.clear();
}

/// The facts this event carries that a view other than the TUI reads
/// through the view surface: the hook observation's mode and effort, the
/// two bridge snapshots, the resolved model, and the monitor set.
///
/// One source, two readers: each fact is folded from the same event the
/// `SessionUpdate` for it is built from, so the held copy cannot drift
/// from the streamed one.
fn hold_view_facts(domain: &mut DomainSession, event: &AgentEvent) {
    if let AgentEvent::Connected { current_model, available_models, history_updates, .. } = event {
        // A replacement occupant inherits nothing the last one held:
        // its hook mirrors describe a session that is gone, and its
        // bridge snapshots describe subprocesses that went with it.
        clear_runtime_identity(domain);
        domain.current_model = Some(current_model.clone());
        domain.available_models.clone_from(available_models);
        // **The dispatch flag is ASSIGNED on every connect, history or not.**
        // The history a connect carries IS the conversation it replaces - so a
        // seat resumed after dispatching keeps the section its flag gates - and
        // a fresh `/new` carries none, which is a conversation that dispatched
        // nothing. A flag left standing there would draw the subagents section
        // over an empty conversation. Assigned rather than raised: the read on
        // this same event already carries it.
        domain.has_dispatches =
            history_updates.as_ref().is_some_and(|history| history.iter().any(is_dispatch));
        // The instance fold is seeded from the same history, so a resumed
        // seat answers the card list its history holds. Assigned like the
        // flag above, and for the same reason.
        domain.card_tracker = crate::subagent_cards::CardTracker::default();
        if let Some(history) = history_updates.as_ref() {
            for message in history {
                domain.card_tracker.apply(message);
            }
        }
        domain.cards_snapshot = domain.card_tracker.cards();
        // A monitor started before this process did is in the transcript
        // the connect carries, so the same fold runs over it: a view
        // opening the session sees the monitor rather than nothing.
        if let Some(history) = history_updates {
            for msg in history {
                fold_monitor(domain, msg, MonitorOrigin::Transcript);
            }
            // A transcript's monitors are all settled, so the seed drains in
            // the one call: a resumed session shows no section rather than a
            // row per monitor it ever ran.
            drain_settled_monitors(domain);
        }
    }
    // The two events that end a run leave the session with no runtime
    // identity: a login wait, and a connection that died. The TUI blanks
    // its own copy on both, so the core blanks what a view reads
    // through it - a mode or a model left standing describes a run that
    // is gone.
    if matches!(event, AgentEvent::ConnectionFailed { .. } | AgentEvent::AuthRequired { .. }) {
        clear_runtime_identity(domain);
    }
    if let AgentEvent::HookObservation { permission_mode, effort, .. } = event {
        if let Some(mode) =
            permission_mode.as_deref().and_then(forge_primitives::PermissionMode::from_wire)
        {
            domain.observed_permission_mode = Some(mode);
        }
        // An unreadable level is a level forge cannot name, not a reason
        // to forget the one already held.
        if let Some(level) = effort.as_deref().and_then(forge_primitives::EffortLevel::from_stored)
        {
            domain.observed_effort = Some(level);
        }
    }
    if let AgentEvent::McpSnapshot { servers, error, .. } = event {
        domain.mcp_servers = Some(crate::domain_session::McpServers {
            servers: servers.clone(),
            error: error.clone(),
        });
    }
    if let AgentEvent::ContextUsage { percentage, max_tokens, .. } = event {
        domain.context_usage = Some(crate::domain_session::ContextUsage {
            percent: *percentage,
            max_tokens: *max_tokens,
        });
    }
    if let AgentEvent::SdkMessage { msg, .. } = event {
        fold_monitor(domain, msg, MonitorOrigin::Wire);
        if let forge_primitives::Message::System { subtype, data, .. } = msg
            && subtype == "init"
        {
            reconcile_model_from_init(domain, data);
        }
    }
}

/// Take the model a turn's `system/init` names, which is how a switch
/// reaches a reader: `/model` re-fires the frame with the model the turn
/// runs under, and the frame arrives at the head of every turn besides.
///
/// A frame naming the model the session is already on changes nothing, so
/// a session that did not switch keeps the name its connect resolved
/// rather than being renamed to the CLI's own spelling of the same model.
fn reconcile_model_from_init(domain: &mut DomainSession, data: &serde_json::Value) {
    let Some(model_id) = data.get("model").and_then(serde_json::Value::as_str).map(str::trim)
    else {
        return;
    };
    if model_id.is_empty()
        || domain.current_model.as_ref().is_some_and(|held| held.resolved_id == model_id)
    {
        return;
    }
    // No requested id: the pin forge stamped described the model the
    // session connected on, and the CLI has just named a different one, so
    // the CLI's own answer is what names this model.
    domain.current_model = Some(crate::session_lifecycle::resolve_current_model_from_inputs(
        model_id,
        None,
        None,
        &domain.available_models,
    ));
}

/// The status a terminal `task_updated` names, in the vocabulary the
/// renderers use. `None` for a status that is not terminal, and for one
/// forge does not know: a patch that only stamps an end time carries no
/// status at all.
fn monitor_status_from_wire(status: &str) -> Option<forge_primitives::MonitorStatus> {
    match status {
        "completed" => Some(forge_primitives::MonitorStatus::Completed),
        "failed" | "killed" | "stopped" => Some(forge_primitives::MonitorStatus::Stopped),
        _ => None,
    }
}

/// Which side of the session a monitor was folded from, which decides the
/// state it starts in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MonitorOrigin {
    /// The live wire, where a `Monitor` tool call means the monitor runs
    /// until a command frame settles it.
    Wire,
    /// The transcript a connect carries. It holds no lifecycle frame - the
    /// replay synthesizer emits user and assistant messages only - so a
    /// monitor found here is one whose task is over as far as this process
    /// can tell, and nothing that follows can settle it, because every
    /// settlement is keyed on the task id a transcript cannot carry.
    Transcript,
}

/// Fold one wire message into the session's monitor set: the `Monitor`
/// tool call that starts one, the task id the CLI assigns it, and the
/// lifecycle message that settles it.
///
/// The CLI's task ids are what the terminal transitions are keyed by,
/// so a record's `task_id` is what lets a later `task_updated` find it
/// at all.
fn fold_monitor(
    domain: &mut DomainSession,
    msg: &forge_primitives::Message,
    origin: MonitorOrigin,
) {
    match msg {
        forge_primitives::Message::Assistant { message, .. } => {
            for block in &message.content {
                let forge_primitives::ContentBlock::ToolUse { id, name, input, .. } = block else {
                    continue;
                };
                if name != "Monitor" {
                    continue;
                }
                let Some(parsed) = forge_agent::user_interaction::parse_monitor_input(input) else {
                    continue;
                };
                if domain.monitors.iter().any(|held| &held.tool_use_id == id) {
                    continue;
                }
                let status = match origin {
                    MonitorOrigin::Wire => forge_primitives::MonitorStatus::Running,
                    MonitorOrigin::Transcript => forge_primitives::MonitorStatus::Completed,
                };
                domain.monitors.push(forge_primitives::MonitorRecord {
                    tool_use_id: id.clone(),
                    task_id: None,
                    description: parsed.description,
                    command: parsed.command,
                    persistent: parsed.persistent,
                    timeout_ms: parsed.timeout_ms,
                    status,
                    output_file: None,
                    ended_at: None,
                });
            }
        }
        forge_primitives::Message::TaskStarted { task_id, tool_use_id: Some(id), .. } => {
            let Some(record) = domain.monitors.iter_mut().find(|held| &held.tool_use_id == id)
            else {
                return;
            };
            record.task_id.get_or_insert_with(|| task_id.clone());
        }
        forge_primitives::Message::TaskUpdated { task_id, patch, .. } => {
            // A status forge does not classify is one it cannot act on,
            // so only the instant is stamped then: it is a fact the frame
            // states whether or not this build can name how it ended.
            settle_monitor(
                domain,
                task_id,
                patch.status.as_deref().and_then(monitor_status_from_wire),
                None,
                patch.end_time,
            );
        }
        forge_primitives::Message::TaskNotification { task_id, status, output_file, .. } => {
            let settled = match status {
                forge_primitives::TaskNotificationStatus::Completed => {
                    Some(forge_primitives::MonitorStatus::Completed)
                }
                forge_primitives::TaskNotificationStatus::Failed
                | forge_primitives::TaskNotificationStatus::Stopped => {
                    Some(forge_primitives::MonitorStatus::Stopped)
                }
                // A status forge cannot name settles nothing, but it is
                // still a notification and the drain below is unconditional.
                forge_primitives::TaskNotificationStatus::Unknown => None,
            };
            if let Some(status) = settled {
                settle_monitor(domain, task_id, Some(status), Some(output_file.clone()), None);
            }
            // The notification is the last frame a monitor sends, so it is
            // where the set drains once nothing in it is running: the same
            // rule the terminal applies, so a session that ran a monitor an
            // hour ago draws no section in either view.
            drain_settled_monitors(domain);
        }
        _ => {}
    }
}

/// Drop the monitor set once every entry in it is terminal.
fn drain_settled_monitors(domain: &mut DomainSession) {
    if !domain.monitors.is_empty()
        && domain.monitors.iter().all(|monitor| monitor.status.is_terminal())
    {
        domain.monitors.clear();
    }
}

fn settle_monitor(
    domain: &mut DomainSession,
    task_id: &str,
    status: Option<forge_primitives::MonitorStatus>,
    output_file: Option<String>,
    end_time_ms: Option<u64>,
) {
    let Some(record) =
        domain.monitors.iter_mut().find(|held| held.task_id.as_deref() == Some(task_id))
    else {
        return;
    };
    if let Some(status) = status {
        record.status = status;
    }
    if let Some(path) = output_file {
        record.output_file = Some(path);
    }
    if let Some(end_ms) = end_time_ms {
        record.ended_at =
            Some(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(end_ms));
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
    use crate::update_fanout::SubscriberRole;
    use forge_agent::Agent;
    use forge_agent::client::SpawnFailureKind;
    // For the write! macros the output-file fixtures build their lines with.
    use std::fmt::Write as _;

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
                None,
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
            conversation: None,
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
        let update_tx = UpdateFanout::default();
        let update_rx = update_tx.subscribe(SubscriberRole::Answering);
        let domain = Arc::new(Mutex::new(DomainSession::new(key.clone(), Some(handle.clone()))));
        let task = SessionTask {
            key: key.clone(),
            handle,
            command_rx,
            domain,
            update_tx,
            connected_once: false,
            workspace: Arc::downgrade(workspace),
            conversation: None,
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
        let update_tx = UpdateFanout::default();
        let _update_rx = update_tx.subscribe(SubscriberRole::Answering);
        let domain = Arc::new(Mutex::new(DomainSession::new(key.clone(), Some(handle.clone()))));
        let task = SessionTask {
            key: key.clone(),
            handle,
            command_rx,
            domain,
            update_tx,
            connected_once: false,
            workspace: Arc::downgrade(workspace),
            conversation: None,
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
            conversation: None,
        };

        workspace.enable_test_dispatch_intercept();
        task.translate_event(connected_event(&session_key.display(), "/tmp/slack-drain"));

        let dispatched = workspace.drain_test_dispatch_buffer();
        assert!(
            dispatched.iter().any(|c| matches!(
                c, crate::protocol::Command::Prompt { key, text, .. }
                    | crate::protocol::Command::PromptUnder { key, text, .. }
                    if *key == session_key && text.contains("the buffered text")
            )),
            "the buffered message arrives as the session's own prompt: {dispatched:?}",
        );

        let mut echoed = false;
        while let Ok(u) = update_rx.try_recv() {
            if matches!(
                u,
                SessionUpdate::SlackMessageAppended { key, prose, .. }
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
            conversation: None,
        };

        workspace.enable_test_dispatch_intercept();
        task.translate_event(connected_event(&slot.display(), "/tmp/slack-rekey"));

        let dispatched = workspace.drain_test_dispatch_buffer();
        assert!(
            dispatched.iter().any(|c| matches!(
                c, crate::protocol::Command::Prompt { key, text, .. }
                    | crate::protocol::Command::PromptUnder { key, text, .. }
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

    /// An errored `Result` carrying the CLI's own error strings, which are
    /// what the nudge's reason is composed from.
    fn result_message_with_errors(errors: &[&str]) -> forge_primitives::Message {
        serde_json::from_value(serde_json::json!({
            "type": "result",
            "subtype": "error_during_execution",
            "duration_ms": 1,
            "duration_api_ms": 1,
            "is_error": true,
            "num_turns": 1,
            "session_id": "worker",
            "errors": errors,
        }))
        .expect("parse result message")
    }

    /// An assistant frame: content the turn produced. The CLI sends one
    /// after a retry it recovered past, which is what makes the retry's
    /// classification stale.
    fn assistant_message() -> forge_primitives::Message {
        serde_json::from_value(serde_json::json!({
            "type": "assistant",
            "session_id": "worker",
            "message": {
                "id": "msg-1",
                "role": "assistant",
                "model": "claude-sonnet-5",
                "content": [{"type": "text", "text": "carrying on"}],
            },
        }))
        .expect("parse assistant message")
    }

    /// A wire `api_retry` frame, the only place the CLI says what went
    /// wrong on a retried request.
    fn api_retry_message(error: &str, status: Option<u16>) -> forge_primitives::Message {
        serde_json::from_value(serde_json::json!({
            "type": "system",
            "subtype": "api_retry",
            "session_id": "worker",
            "attempt": 1,
            "max_retries": 4,
            "retry_delay_ms": 500,
            "error_status": status,
            "error": error,
        }))
        .expect("parse api_retry message")
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

    /// A turn that ends in error marks the slot - the fact the rail's
    /// failure mark reads - and a result the reader asked to interrupt
    /// does not: a cancel ends with the same failed `Result`.
    #[test]
    fn an_errored_result_marks_the_slot_unless_the_reader_cancelled() {
        let (workspace, _rx) = crate::Workspace::testing_stub();
        let key = SessionSlot::from_str_for_test("failed-rail");
        let (mut task, _update_rx) = review_task_for(&workspace, &key);

        task.translate_event(AgentEvent::SdkMessage {
            session_id: "failed-rail".to_owned(),
            msg: result_message("error_during_execution", true),
        });
        assert!(
            task.domain.lock().failed_turn_at.is_some(),
            "a genuine error marks the slot for the rail",
        );

        // The reader cancels the next turn: armed at Cancel routing when
        // the turn is in flight, spent at the result, which must not mark.
        task.domain.lock().failed_turn_at = None;
        task.domain.lock().pending_cancel = true;
        task.translate_event(AgentEvent::SdkMessage {
            session_id: "failed-rail".to_owned(),
            msg: result_message("error_during_execution", true),
        });
        assert!(
            task.domain.lock().failed_turn_at.is_none(),
            "a cancelled turn is the reader's own act, not a failure to flag",
        );
        assert!(!task.domain.lock().pending_cancel, "the stamp is spent at the result");

        // And spent when unused: a cancel that raced a turn the CLI had
        // already finished must not exempt the next genuine failure.
        task.domain.lock().pending_cancel = true;
        task.translate_event(AgentEvent::SdkMessage {
            session_id: "failed-rail".to_owned(),
            msg: result_message("success", false),
        });
        task.translate_event(AgentEvent::SdkMessage {
            session_id: "failed-rail".to_owned(),
            msg: result_message("error_during_execution", true),
        });
        assert!(
            task.domain.lock().failed_turn_at.is_some(),
            "a success spends the stamp, so the next real failure marks",
        );
    }

    /// A failure the terminal's own dead-turn path leaves alone arms the
    /// seat's nudge: the CLI's own error strings as the reason, and the
    /// terminal's first backoff as the delay before it goes out.
    #[test]
    fn a_plain_failure_arms_the_continuation_with_its_reason() {
        let (workspace, _rx) = crate::Workspace::testing_stub();
        let key = SessionSlot::from_str_for_test("nudge-rail");
        let (mut task, _update_rx) = review_task_for(&workspace, &key);

        task.translate_event(AgentEvent::SdkMessage {
            session_id: "nudge-rail".to_owned(),
            msg: result_message_with_errors(&["API Error: 400 ...", "request too large"]),
        });

        let domain = task.domain.lock();
        let pending = domain.auto_continue.clone().expect("a plain failure arms the nudge");
        assert_eq!(
            pending.reason, "API Error: 400 ...; request too large",
            "the CLI's own errors are the reason, in the order it reported them",
        );
        let failed_at = domain.failed_turn_at.expect("the rail's mark is set with it");
        assert!(pending.due_at > failed_at, "the nudge is not immediate - it waits for a reader");
        assert_eq!(
            pending.due_at,
            failed_at + super::AUTO_CONTINUE_DELAY,
            "and its wait is the delay, counted from the failure itself",
        );
    }

    /// A turn the reader cancelled ends with the same failed `Result` a
    /// genuine error does, and arms nothing - there is no failure to pick
    /// back up, only the interruption the reader asked for.
    #[test]
    fn a_cancelled_turn_arms_no_continuation() {
        let (workspace, _rx) = crate::Workspace::testing_stub();
        let key = SessionSlot::from_str_for_test("cancelled-rail");
        let (mut task, _update_rx) = review_task_for(&workspace, &key);
        task.domain.lock().pending_cancel = true;

        task.translate_event(AgentEvent::SdkMessage {
            session_id: "cancelled-rail".to_owned(),
            msg: result_message_with_errors(&["aborted_streaming"]),
        });

        let domain = task.domain.lock();
        assert!(domain.failed_turn_at.is_none(), "a cancelled turn is not a failure to mark");
        assert!(domain.auto_continue.is_none(), "and nothing is armed to pick up");
    }

    /// The terminal's own dead-turn path continues a transient server
    /// error with its own prompt; arming one here too would double-fire on
    /// the same failure. The rail still marks it.
    #[test]
    fn a_transient_server_error_arms_no_continuation() {
        let (workspace, _rx) = crate::Workspace::testing_stub();
        let key = SessionSlot::from_str_for_test("transient-rail");
        let (mut task, _update_rx) = review_task_for(&workspace, &key);

        task.translate_event(AgentEvent::SdkMessage {
            session_id: "transient-rail".to_owned(),
            msg: api_retry_message("server_error", Some(529)),
        });
        task.translate_event(AgentEvent::SdkMessage {
            session_id: "transient-rail".to_owned(),
            msg: result_message("error_during_execution", true),
        });

        let domain = task.domain.lock();
        assert!(domain.failed_turn_at.is_some(), "the rail still marks the failure");
        assert!(
            domain.auto_continue.is_none(),
            "the terminal continues this one; the core must not fire beside it",
        );
    }

    /// A retry the turn RECOVERED past is not a classification of its
    /// failure: the content that follows the retry is the turn going on. The
    /// terminal drops its own copy there, and left standing here the stale
    /// `server_error` would exempt a later failure of a different kind - the
    /// seat would then get nothing from either path.
    #[test]
    fn a_retry_the_turn_recovered_past_does_not_exempt_a_later_failure() {
        let (workspace, _rx) = crate::Workspace::testing_stub();
        let key = SessionSlot::from_str_for_test("recovered-rail");
        let (mut task, _update_rx) = review_task_for(&workspace, &key);

        task.translate_event(AgentEvent::SdkMessage {
            session_id: "recovered-rail".to_owned(),
            msg: api_retry_message("server_error", Some(529)),
        });
        task.translate_event(AgentEvent::SdkMessage {
            session_id: "recovered-rail".to_owned(),
            msg: assistant_message(),
        });
        task.translate_event(AgentEvent::SdkMessage {
            session_id: "recovered-rail".to_owned(),
            msg: result_message_with_errors(&["API Error: 400 ..."]),
        });

        let domain = task.domain.lock();
        assert_eq!(domain.last_api_retry, None, "the recovered retry is not this failure");
        assert!(domain.auto_continue.is_some(), "so a failure of another kind is nudged");
    }

    /// A result the CLI sent no errors for is named by its subtype in the
    /// nudge - the book's contract for the words - and an empty list names
    /// nothing either.
    #[test]
    fn a_result_without_errors_is_named_by_its_subtype_in_the_nudge() {
        let (workspace, _rx) = crate::Workspace::testing_stub();
        let key = SessionSlot::from_str_for_test("subtype-rail");
        let (mut task, _update_rx) = review_task_for(&workspace, &key);

        task.translate_event(AgentEvent::SdkMessage {
            session_id: "subtype-rail".to_owned(),
            msg: result_message("error_max_turns", true),
        });
        let reason = task.domain.lock().auto_continue.clone().expect("armed").reason;
        assert_eq!(reason, "error_max_turns", "no errors reported, so the subtype names it");

        task.domain.lock().auto_continue = None;
        task.translate_event(AgentEvent::SdkMessage {
            session_id: "subtype-rail".to_owned(),
            msg: result_message_with_errors(&[]),
        });
        let reason = task.domain.lock().auto_continue.clone().expect("armed").reason;
        assert_eq!(reason, "error_during_execution", "an empty list names nothing either");
    }

    /// The words of the reason, branch by branch: the CLI's errors win when
    /// it reported any, then the result's subtype, and a plain wording when
    /// neither names anything.
    #[test]
    fn the_failures_reason_prefers_the_cli_errors_then_the_subtype() {
        assert_eq!(
            super::failure_reason(Some(&["API Error: 400 ...".to_owned()]), "error_max_turns"),
            "API Error: 400 ...",
            "the CLI's own errors are the reason",
        );
        assert_eq!(
            super::failure_reason(None, "error_max_turns"),
            "error_max_turns",
            "a result with no errors is named by its subtype",
        );
        assert_eq!(
            super::failure_reason(None, "success"),
            "an error",
            "a subtype that names nothing falls back to plain wording",
        );
        assert_eq!(super::failure_reason(None, ""), "an error", "and so does an empty subtype");
    }

    /// A turn that finished ends the episode: the spend is dropped and so
    /// is the classification it was read against, so a later unrelated
    /// failure is nudged on its own terms.
    #[test]
    fn a_completed_turn_clears_the_spend_for_the_next_failure() {
        let (workspace, _rx) = crate::Workspace::testing_stub();
        let key = SessionSlot::from_str_for_test("spent-rail");
        let (mut task, _update_rx) = review_task_for(&workspace, &key);
        task.domain.lock().auto_continue_spent = true;
        task.domain.lock().last_api_retry =
            Some((forge_primitives::ApiRetryError::InvalidRequest, Some(400)));

        task.translate_event(AgentEvent::SdkMessage {
            session_id: "spent-rail".to_owned(),
            msg: result_message("success", false),
        });

        let domain = task.domain.lock();
        assert!(!domain.auto_continue_spent, "a completed turn ends the episode");
        assert!(domain.last_api_retry.is_none(), "and drops the classification it was read for");
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
            extras: serde_json::Map::new(),
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
                extras: serde_json::Map::new(),
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
            msg: forge_primitives::Message::Error {
                error: "stream closed".to_owned(),
                extras: serde_json::Map::new(),
            },
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
        let update_tx = UpdateFanout::default();
        let mut update_rx = update_tx.subscribe(SubscriberRole::Answering);
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
            conversation: None,
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
        let update_tx = UpdateFanout::default();
        let _update_rx = update_tx.subscribe(SubscriberRole::Answering);
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
            conversation: None,
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
        let update_tx = UpdateFanout::default();
        let _update_rx = update_tx.subscribe(SubscriberRole::Answering);
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
            conversation: None,
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
        let update_tx = UpdateFanout::default();
        let _update_rx = update_tx.subscribe(SubscriberRole::Answering);
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
            conversation: None,
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

    /// A subscriber that only reads must not keep a permission request
    /// parked. With no subscriber that can answer it, the guard resolves
    /// the slot `Cancelled` rather than leaving the turn waiting on a
    /// reply nobody will send. Catches the guard keying on "somebody
    /// took the update" instead of "somebody can answer it", which the
    /// observer's arrival would otherwise turn into a hang.
    #[tokio::test]
    async fn a_permission_request_with_only_an_observer_fails_closed() {
        let (_dir, workspace) = workspace_with_account_config_dir("/tmp/forge-testing-stub");
        let (handle, _agent_rx) = Agent::testing_stub();
        let handle = Arc::new(handle);
        let key = SessionSlot::from_str_for_test("perm-observer");
        let (_cmd_tx, command_rx) = mpsc::unbounded_channel();
        let update_tx = UpdateFanout::default();
        let mut observer = update_tx.subscribe(SubscriberRole::Observing);
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
            conversation: None,
        };

        task.translate_event(AgentEvent::PermissionRequest {
            session_id: key.display(),
            request: permission_request_fixture("tu-obs"),
        });

        assert!(
            !task.domain.lock().pending_interactions.contains_key("tu-obs"),
            "the slot is resolved rather than parked, so the turn cannot hang",
        );
        assert!(
            matches!(
                observer.try_recv(),
                Ok(SessionUpdate::PermissionRequest { tool_id, .. }) if tool_id == "tu-obs"
            ),
            "the observer is still delivered the request it cannot answer",
        );
    }

    /// The question guard keys on an answering subscriber the same way
    /// the permission one does. Catches pointing this site back at
    /// `send`: the permission test drives a different event, so an
    /// observer's arrival here would park the question on a reply nobody
    /// will send and no other test would notice.
    #[tokio::test]
    async fn a_question_request_with_only_an_observer_fails_closed() {
        let (_dir, workspace) = workspace_with_account_config_dir("/tmp/forge-testing-stub");
        let (handle, _agent_rx) = Agent::testing_stub();
        let handle = Arc::new(handle);
        let key = SessionSlot::from_str_for_test("question-observer");
        let (_cmd_tx, command_rx) = mpsc::unbounded_channel();
        let update_tx = UpdateFanout::default();
        let mut observer = update_tx.subscribe(SubscriberRole::Observing);
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
            conversation: None,
        };

        task.translate_event(AgentEvent::QuestionRequest {
            session_id: key.display(),
            request: question_request_fixture("tu-q-obs"),
        });

        assert!(
            !task.domain.lock().pending_interactions.contains_key("tu-q-obs"),
            "the question slot is resolved rather than parked, so the turn cannot hang",
        );
        assert!(
            matches!(
                observer.try_recv(),
                Ok(SessionUpdate::QuestionRequest { tool_id, .. }) if tool_id == "tu-q-obs"
            ),
            "the observer is still delivered the question it cannot answer",
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
        let update_tx = UpdateFanout::default();
        let _update_rx = update_tx.subscribe(SubscriberRole::Answering);
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
            conversation: None,
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

    /// The round rides the resolution: a batch reuses one tool id and
    /// advances the question index, and a view's clear needs the pair to
    /// dequeue a batch's rounds one at a time (the loss itself is the
    /// record's single ask slot - #1717's queue, its own piece).
    ///
    /// **This pins the ANSWER path** - one of the sites that emits the
    /// frame; the identity drain and the orphan arms carry the same field,
    /// and the compiler holds every site to naming it.
    #[tokio::test]
    async fn an_answered_questions_resolution_names_its_round() {
        let (_dir, workspace) = workspace_with_account_config_dir("/tmp/forge-testing-stub");
        let (handle, _agent_rx) = Agent::testing_stub();
        let handle = Arc::new(handle);
        let key = SessionSlot::from_str_for_test("ask-round");
        let (_cmd_tx, command_rx) = mpsc::unbounded_channel();
        let update_tx = UpdateFanout::default();
        let mut observer = update_tx.subscribe(SubscriberRole::Answering);
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
            conversation: None,
        };

        let mut request = question_request_fixture("tu-round");
        request.question_index = 3;
        task.translate_event(AgentEvent::QuestionRequest { session_id: key.display(), request });

        task.execute_command(Command::RespondQuestion {
            key: key.clone(),
            tool_id: "tu-round".to_owned(),
            outcome: forge_primitives::QuestionOutcome::Cancelled,
        });

        let mut resolved = None;
        while let Ok(update) = observer.try_recv() {
            if let SessionUpdate::PendingInteractionResolved { question_index, .. } = update {
                resolved = Some(question_index);
            }
        }
        assert_eq!(resolved, Some(Some(3)), "the resolution names the round it ends");
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
            class: forge_primitives::TurnErrorClass::Other,
        });
        assert!(
            matches!(
                update_rx.try_recv(),
                Ok(SessionUpdate::TurnError { message, .. }) if message == "stdin write failed"
            ),
            "TurnError carries the failure text so the spinner unwinds"
        );
    }

    /// The class an event was built with rides the update stream, so a
    /// view that reads only `SessionUpdate` can tell an auth failure from
    /// any other error instead of searching the message itself.
    #[tokio::test]
    async fn turn_error_carries_its_class_onto_the_update_stream() {
        let (_dir, workspace) = workspace_with_account_config_dir("/tmp/forge-testing-stub");
        let (mut task, mut update_rx) =
            review_task_for(&workspace, &SessionSlot::from_str_for_test("m"));

        for class in [
            forge_primitives::TurnErrorClass::AuthRequired,
            forge_primitives::TurnErrorClass::Other,
        ] {
            task.translate_event(AgentEvent::TurnError {
                session_id: "m".to_owned(),
                message: "interrupt not acknowledged by the CLI".to_owned(),
                class,
            });
            assert!(
                matches!(
                    update_rx.try_recv(),
                    Ok(SessionUpdate::TurnError { class: Some(on_wire), .. }) if on_wire == class
                ),
                "the class has to reach the stream as {class:?}, not as None a reader must guess past",
            );
        }
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
                    mcp_families: None,
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

    /// The monitor sets these updates announced, in the order they went out.
    fn announced_monitors(
        updates: &mut mpsc::UnboundedReceiver<SessionUpdate>,
    ) -> Vec<Vec<forge_primitives::MonitorRecord>> {
        let mut announced = Vec::new();
        while let Ok(update) = updates.try_recv() {
            if let SessionUpdate::MonitorsChanged { monitors, .. } = update {
                announced.push(monitors);
            }
        }
        announced
    }

    /// The task sets these updates announced, in the order they went out.
    fn announced_tasks(
        updates: &mut mpsc::UnboundedReceiver<SessionUpdate>,
    ) -> Vec<Vec<forge_workspace::BackgroundTask>> {
        let mut announced = Vec::new();
        while let Ok(update) = updates.try_recv() {
            if let SessionUpdate::BackgroundTasksChanged { tasks, .. } = update {
                announced.push(tasks);
            }
        }
        announced
    }

    /// An assistant frame carrying one tool call, under `parent_tool_use_id`.
    fn assistant_tool_use(
        tool: &str,
        parent_tool_use_id: Option<&str>,
    ) -> forge_primitives::Message {
        serde_json::from_value(serde_json::json!({
            "type": "assistant",
            "message": {
                "id": "msg-dispatch",
                "role": "assistant",
                "model": "claude-sonnet-5",
                "content": [{
                    "type": "tool_use",
                    "id": "tu-dispatch",
                    "name": tool,
                    "input": {"description": "investigate"},
                }],
            },
            "session_id": "s",
            "parent_tool_use_id": parent_tool_use_id,
        }))
        .expect("parse an assistant tool_use")
    }

    /// An assistant frame carrying a `Task` call, which is a dispatch.
    fn dispatch_frame(parent_tool_use_id: Option<&str>) -> forge_primitives::Message {
        assistant_tool_use("Task", parent_tool_use_id)
    }

    /// **The dispatch rule, as it was computed and as this fold computes it.**
    /// It was `forge-server`'s, over the conversation it held; the fold that
    /// can announce the raise is this one, so the rule moved here with the
    /// flag - same predicate over the same field, so the flag a record reads
    /// and the flag this fold raises cannot disagree.
    ///
    /// The rule: an assistant frame whose `parent_tool_use_id` is absent or
    /// blank - `names_a_dispatch`'s non-empty-string guard - carrying a `Task`
    /// or `Agent` call. A sub-agent's own calls are the sub-agent's and do not
    /// count for the session that dispatched it.
    #[test]
    fn a_dispatch_is_an_assistant_frame_calling_task_or_agent() {
        assert!(is_dispatch(&assistant_tool_use("Task", None)), "a Task call is a dispatch");
        assert!(is_dispatch(&assistant_tool_use("Agent", None)), "and so is an Agent call");
        assert!(!is_dispatch(&assistant_tool_use("Bash", None)), "a Bash call is not one");
        assert!(
            !is_dispatch(&assistant_tool_use("Task", Some("tu-parent"))),
            "and a sub-agent's own Task call is the sub-agent's, not this session's",
        );
        assert!(
            is_dispatch(&assistant_tool_use("Task", Some("  "))),
            "a blank parent is not a dispatch's, so the call is this session's",
        );
    }

    /// The dispatch answers these updates announced, in the order they went out.
    fn announced_dispatches(updates: &mut mpsc::UnboundedReceiver<SessionUpdate>) -> Vec<bool> {
        let mut announced = Vec::new();
        while let Ok(update) = updates.try_recv() {
            if let SessionUpdate::DispatchesChanged { has_dispatches, .. } = update {
                announced.push(has_dispatches);
            }
        }
        announced
    }

    /// The card lists a task announced, in order.
    fn announced_cards(
        updates: &mut mpsc::UnboundedReceiver<SessionUpdate>,
    ) -> Vec<Vec<forge_primitives::runtime::SubagentCard>> {
        let mut announced = Vec::new();
        while let Ok(update) = updates.try_recv() {
            if let SessionUpdate::SubagentCardsChanged { cards, .. } = update {
                announced.push(cards);
            }
        }
        announced
    }

    /// **The instance list is pushed as it moves.** The card fold rides the
    /// same frame walk that raises the dispatch flag, and the push is
    /// change-gated on the list: the dispatch announces a running card, a
    /// roster frame that moves nothing announces nothing, and the terminal
    /// row announces the settled list.
    #[test]
    fn a_dispatch_announces_a_card_and_the_roster_settles_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (workspace, _rx) =
            crate::Workspace::testing_stub_with_config_dir(dir.path().to_path_buf());
        let slot = SessionSlot::lead("TestOrg", "forge");
        let (mut task, _agent_rx) = command_task_for(&workspace, &slot);
        let mut updates = task.update_tx.subscribe(SubscriberRole::Answering);

        task.translate_event(sdk_message(dispatch_frame(None)));
        let announced = announced_cards(&mut updates);
        assert_eq!(announced.len(), 1, "the dispatch announces the list once");
        assert_eq!(announced[0].len(), 1, "with one instance");
        assert!(announced[0][0].running, "running, because nothing has said it ended");
        assert_eq!(announced[0][0].name, "investigate", "named by the dispatch's description");

        // The roster opening the task moves no fact the card draws - it was
        // already running - so the list is equal and nothing is announced.
        task.translate_event(sdk_message(forge_primitives::Message::TaskStarted {
            task_id: "t-a".to_owned(),
            description: "investigate".to_owned(),
            uuid: "u-start".to_owned(),
            session_id: "s".to_owned(),
            tool_use_id: Some("tu-dispatch".to_owned()),
            task_type: Some("local_agent".to_owned()),
            extras: serde_json::Map::new(),
        }));
        assert!(
            announced_cards(&mut updates).is_empty(),
            "a frame that moves no fact on the list announces nothing",
        );

        // The terminal row settles the card, and that IS news.
        task.translate_event(sdk_message(forge_primitives::Message::TaskUpdated {
            task_id: "t-a".to_owned(),
            patch: forge_primitives::messages::TaskUpdatePatch {
                status: Some("completed".to_owned()),
                end_time: Some(1_700_000_000_123),
                extras: serde_json::Map::new(),
            },
            uuid: "u-upd".to_owned(),
            session_id: "s".to_owned(),
            extras: serde_json::Map::new(),
        }));
        let announced = announced_cards(&mut updates);
        assert_eq!(announced.len(), 1, "the ending announces the list once");
        assert!(!announced[0][0].running, "and the card is settled");
        assert_eq!(announced[0][0].ended_at_ms, Some(1_700_000_000_123));
    }

    /// **A connect seeds the list from its history and announces nothing**,
    /// the same rule the dispatch flag follows: the read on that event
    /// answers it, and a fresh `/new` carries none.
    #[test]
    fn a_connect_seeds_the_cards_without_announcing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (workspace, _rx) =
            crate::Workspace::testing_stub_with_config_dir(dir.path().to_path_buf());
        let slot = SessionSlot::lead("TestOrg", "forge");
        let (mut task, _agent_rx) = command_task_for(&workspace, &slot);
        let mut updates = task.update_tx.subscribe(SubscriberRole::Answering);

        task.translate_event(AgentEvent::Connected {
            session_id: "resumed-uuid".to_owned(),
            cwd: "/proj".to_owned(),
            current_model: forge_primitives::CurrentModel::new("claude-opus-5", "Opus", "Claude"),
            available_models: Vec::new(),
            mode: None,
            history_updates: Some(vec![dispatch_frame(None)]),
            compaction_count: 0,
        });

        assert_eq!(
            task.domain.lock().cards_snapshot.len(),
            1,
            "a history that dispatched seeds one card",
        );
        assert!(
            announced_cards(&mut updates).is_empty(),
            "and the seed is not announced: the read on this event answers it",
        );

        task.translate_event(AgentEvent::Connected {
            session_id: "new-uuid".to_owned(),
            cwd: "/proj".to_owned(),
            current_model: forge_primitives::CurrentModel::new("claude-opus-5", "Opus", "Claude"),
            available_models: Vec::new(),
            mode: None,
            history_updates: None,
            compaction_count: 0,
        });

        assert!(task.domain.lock().cards_snapshot.is_empty(), "a fresh /new leaves none");
        assert!(announced_cards(&mut updates).is_empty(), "and the clear is not announced");
    }

    /// **A dispatch made in front of a viewer is news, and one whose card the
    /// sub-agents section already lists is not.** The flag gates a section the
    /// record carries, so the frame that raises it is what turns that section
    /// on without a read - and a conversation that dispatched cannot stop
    /// having dispatched, so a later dispatch frame says nothing.
    #[test]
    fn a_dispatch_frame_raises_the_flag_and_announces_it_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (workspace, _rx) =
            crate::Workspace::testing_stub_with_config_dir(dir.path().to_path_buf());
        let slot = SessionSlot::lead("TestOrg", "forge");
        let (mut task, _agent_rx) = command_task_for(&workspace, &slot);
        let mut updates = task.update_tx.subscribe(SubscriberRole::Answering);
        assert!(!task.domain.lock().has_dispatches, "the seat starts with none");

        task.translate_event(sdk_message(dispatch_frame(None)));
        assert!(task.domain.lock().has_dispatches, "the dispatch raises the flag");
        assert_eq!(
            announced_dispatches(&mut updates),
            vec![true],
            "and the raise is announced once",
        );

        task.translate_event(sdk_message(dispatch_frame(None)));
        assert!(
            announced_dispatches(&mut updates).is_empty(),
            "a second dispatch is not a move, so nothing is announced",
        );

        // A sub-agent's OWN frame names a parent, so it is not a dispatch of
        // this conversation - and it must not raise a flag that is already up
        // in a way a test could not tell.
        task.translate_event(sdk_message(dispatch_frame(Some("tu-parent"))));
        assert!(
            announced_dispatches(&mut updates).is_empty(),
            "a sub-agent's own frame is not a dispatch",
        );
    }

    /// **A connect assigns the flag from the history it carries, and
    /// announces nothing** - the page's own read on that same event carries
    /// it. A seat resumed after dispatching keeps the section its flag gates,
    /// which is what the assignment is for; a fresh `/new` carries NO history
    /// (the producer only sends one for a non-empty resume), which is a
    /// conversation that dispatched nothing - and the flag goes with the
    /// occupant that raised it.
    #[test]
    fn a_connect_assigns_the_flag_from_its_history_and_announces_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (workspace, _rx) =
            crate::Workspace::testing_stub_with_config_dir(dir.path().to_path_buf());
        let slot = SessionSlot::lead("TestOrg", "forge");
        let (mut task, _agent_rx) = command_task_for(&workspace, &slot);
        let mut updates = task.update_tx.subscribe(SubscriberRole::Answering);

        task.translate_event(AgentEvent::Connected {
            session_id: "resumed-uuid".to_owned(),
            cwd: "/proj".to_owned(),
            current_model: forge_primitives::CurrentModel::new("claude-opus-5", "Opus", "Claude"),
            available_models: Vec::new(),
            mode: None,
            history_updates: Some(vec![dispatch_frame(None)]),
            compaction_count: 0,
        });

        assert!(task.domain.lock().has_dispatches, "a history that dispatched seeds the flag true");
        assert!(
            announced_dispatches(&mut updates).is_empty(),
            "and the seed is not announced: the read on this event answers it",
        );

        // `/new`: the connect carries no history at all, and that is the
        // conversation this seat now has. The transition is not announced
        // either, for the same reason - and a flag left standing here would
        // draw the subagents section over an empty conversation.
        task.translate_event(AgentEvent::Connected {
            session_id: "new-uuid".to_owned(),
            cwd: "/proj".to_owned(),
            current_model: forge_primitives::CurrentModel::new("claude-opus-5", "Opus", "Claude"),
            available_models: Vec::new(),
            mode: None,
            history_updates: None,
            compaction_count: 0,
        });

        assert!(!task.domain.lock().has_dispatches, "a fresh /new leaves no dispatch standing");
        assert!(
            announced_dispatches(&mut updates).is_empty(),
            "and the clear is not announced either",
        );
    }

    /// The command catalogues these updates announced, in the order they went out.
    fn announced_commands(
        updates: &mut mpsc::UnboundedReceiver<SessionUpdate>,
    ) -> Vec<Vec<forge_primitives::runtime::AvailableCommand>> {
        let mut announced = Vec::new();
        while let Ok(update) = updates.try_recv() {
            if let SessionUpdate::SlashCommandsChanged { commands, .. } = update {
                announced.push(commands);
            }
        }
        announced
    }

    /// The agent catalogues these updates announced, in the order they went out.
    fn announced_agents(
        updates: &mut mpsc::UnboundedReceiver<SessionUpdate>,
    ) -> Vec<Vec<forge_primitives::runtime::AvailableAgent>> {
        let mut announced = Vec::new();
        while let Ok(update) = updates.try_recv() {
            if let SessionUpdate::SubagentsChanged { subagents, .. } = update {
                announced.push(subagents);
            }
        }
        announced
    }

    /// The `/` menu's catalogue moves on the frames that can move it - a turn's
    /// init and a plugin reload's `commands_changed` - and a frame that leaves
    /// it as it was says nothing.
    ///
    /// **The no-move half is what a turn's init makes necessary.** The CLI
    /// re-fires init every turn, so the case the composer would pay for most
    /// often is the same list again.
    #[test]
    fn a_command_catalogue_that_moved_is_announced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (workspace, _rx) =
            crate::Workspace::testing_stub_with_config_dir(dir.path().to_path_buf());
        let slot = SessionSlot::lead("TestOrg", "forge");
        let (mut task, _agent_rx) = command_task_for(&workspace, &slot);
        let mut updates = task.update_tx.subscribe(SubscriberRole::Answering);

        task.translate_event(sdk_message(init_frame(&["/help"], &[])));
        let announced = announced_commands(&mut updates);
        assert_eq!(announced.len(), 1, "the init that fills the catalogue announces it");
        assert_eq!(
            announced[0],
            task.domain.lock().available_commands,
            "and what it announces is what the core holds",
        );

        task.translate_event(sdk_message(result_message("success", false)));
        task.translate_event(sdk_message(init_frame(&["/help"], &[])));
        assert_eq!(
            announced_commands(&mut updates).len(),
            0,
            "a turn's init repeating the same list is not a move, so nothing is announced",
        );

        task.translate_event(sdk_message(forge_primitives::Message::CommandsChanged {
            commands: vec![serde_json::json!({"name": "/reload", "description": "Reloaded"})],
            uuid: "cmd-uuid".to_owned(),
            session_id: "s".to_owned(),
            extras: serde_json::Map::new(),
        }));
        let reloaded = announced_commands(&mut updates);
        assert_eq!(reloaded.len(), 1, "the reload's list is a move");
        assert_eq!(reloaded[0], task.domain.lock().available_commands);
    }

    /// The agent catalogue moves on the one frame that carries it, and an init
    /// that advertises what the last turn did says nothing.
    #[test]
    fn an_agent_catalogue_that_moved_is_announced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (workspace, _rx) =
            crate::Workspace::testing_stub_with_config_dir(dir.path().to_path_buf());
        let slot = SessionSlot::lead("TestOrg", "forge");
        let (mut task, _agent_rx) = command_task_for(&workspace, &slot);
        let mut updates = task.update_tx.subscribe(SubscriberRole::Answering);

        task.translate_event(sdk_message(init_frame(&[], &["reviewer"])));
        let announced = announced_agents(&mut updates);
        assert_eq!(announced.len(), 1, "the init that fills the catalogue announces it");
        assert_eq!(
            announced[0],
            task.domain.lock().available_agents,
            "and what it announces is what the core holds",
        );

        task.translate_event(sdk_message(result_message("success", false)));
        task.translate_event(sdk_message(init_frame(&[], &["reviewer"])));
        assert_eq!(
            announced_agents(&mut updates).len(),
            0,
            "an init repeating the same catalogue is not a move, so nothing is announced",
        );

        task.translate_event(sdk_message(result_message("success", false)));
        task.translate_event(sdk_message(init_frame(&[], &["reviewer", "researcher"])));
        let grown = announced_agents(&mut updates);
        assert_eq!(grown.len(), 1, "an agent the CLI started advertising is a move");
        assert_eq!(grown[0], task.domain.lock().available_agents);
    }

    /// The monitor set moves on discrete task frames, so the frame that moved
    /// it announces the whole set and a frame that moved nothing says nothing.
    ///
    /// **The no-move half is the one worth having.** Every frame here reaches
    /// the fold, and a stamp that repeats an id already stamped must not
    /// redraw every viewer of the seat.
    #[test]
    fn a_monitor_set_that_moved_is_announced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (workspace, _rx) =
            crate::Workspace::testing_stub_with_config_dir(dir.path().to_path_buf());
        let slot = SessionSlot::lead("TestOrg", "forge");
        let (mut task, _agent_rx) = command_task_for(&workspace, &slot);
        let mut updates = task.update_tx.subscribe(SubscriberRole::Answering);

        // The call that starts a watch enters the set.
        task.translate_event(sdk_message(monitor_tool_use("tu-mon", "ci-watch")));
        let announced = announced_monitors(&mut updates);
        assert_eq!(announced.len(), 1, "the call that starts a watch announces the set");
        assert_eq!(
            announced[0],
            task.domain.lock().monitors,
            "and what it announces is what the core holds",
        );

        // The task id the CLI stamps is a move; the same stamp again is not.
        task.translate_event(sdk_message(task_started("t-mon", Some("tu-mon"))));
        assert_eq!(
            announced_monitors(&mut updates).len(),
            1,
            "stamping the task id moves the set, so it is announced",
        );
        task.translate_event(sdk_message(task_started("t-mon", Some("tu-mon"))));
        assert_eq!(
            announced_monitors(&mut updates).len(),
            0,
            "and a frame that moves nothing says nothing",
        );

        // A STATUS-ONLY change is a move too, and it is the one the shared
        // comparison has to catch on its own: the record's id is untouched,
        // so nothing but the status separates a settle from a repeat.
        task.translate_event(sdk_message(task_updated("t-mon", "completed")));
        let settled = announced_monitors(&mut updates);
        assert_eq!(settled.len(), 1, "settling a monitor announces the settled set");
        assert_eq!(
            settled[0][0].status,
            forge_primitives::MonitorStatus::Completed,
            "with the status the frame gave it",
        );
        task.translate_event(sdk_message(task_updated("t-mon", "completed")));
        assert_eq!(
            announced_monitors(&mut updates).len(),
            0,
            "and the same settle again, on a record already settled, says nothing",
        );

        // The notification is the last frame a monitor sends, and the drain
        // that empties the set is a move like any other.
        task.translate_event(sdk_message(task_notification("t-mon")));
        let drained = announced_monitors(&mut updates);
        assert_eq!(drained.len(), 1, "the drain announces");
        assert!(drained[0].is_empty(), "with the empty set, which is what the core now holds");
    }

    /// The background registry is the CLI's whole set on every change, so a
    /// frame that moved it announces it and one that did not says nothing.
    #[test]
    fn a_background_registry_that_moved_is_announced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (workspace, _rx) =
            crate::Workspace::testing_stub_with_config_dir(dir.path().to_path_buf());
        let slot = SessionSlot::lead("TestOrg", "forge");
        let (mut task, _agent_rx) = command_task_for(&workspace, &slot);
        let mut updates = task.update_tx.subscribe(SubscriberRole::Answering);

        task.translate_event(sdk_message(background_tasks(one_live_task())));
        let announced = announced_tasks(&mut updates);
        assert_eq!(announced.len(), 1, "the frame that fills the registry announces it");
        assert_eq!(
            announced[0],
            task.domain.lock().background_tasks,
            "and what it announces is what the core holds",
        );

        task.translate_event(sdk_message(background_tasks(one_live_task())));
        assert_eq!(
            announced_tasks(&mut updates).len(),
            0,
            "the same set again is not a move, so nothing is announced",
        );

        // A connection that died leaves nothing standing: a registry that
        // outlived its process spins rows over tasks nobody is running.
        task.translate_event(AgentEvent::ConnectionFailed {
            message: "reader died".to_owned(),
            kind: SpawnFailureKind::Unclassified,
        });
        let cleared = announced_tasks(&mut updates);
        assert_eq!(cleared.len(), 1, "and the clear is announced");
        assert!(cleared[0].is_empty(), "with the empty set the core now holds");
    }

    /// The walks these updates announced, in the order they went out.
    fn announced_walks(
        updates: &mut mpsc::UnboundedReceiver<SessionUpdate>,
    ) -> Vec<forge_agent::env::processes::ProcessSnapshot> {
        let mut announced = Vec::new();
        while let Ok(update) = updates.try_recv() {
            if let SessionUpdate::ProcessesChanged { snapshot, .. } = update {
                announced.push(snapshot);
            }
        }
        announced
    }

    /// **A walk that outlives its process draws a tree that is not there.** The
    /// walk describes the subprocess's descendants, and nothing follows a dead
    /// session that would replace it - so the clear is announced on the death
    /// frame like the registry's, rather than left for the next reader to
    /// notice and the next session to overwrite.
    #[test]
    fn a_dead_sessions_walk_is_cleared_and_announced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (workspace, _rx) =
            crate::Workspace::testing_stub_with_config_dir(dir.path().to_path_buf());
        let slot = SessionSlot::lead("TestOrg", "forge");
        let (mut task, _agent_rx) = command_task_for(&workspace, &slot);
        let mut updates = task.update_tx.subscribe(SubscriberRole::Answering);

        let walk = || forge_agent::env::processes::ProcessSnapshot {
            processes: vec![forge_agent::env::processes::ProcessEntry {
                pid: 4242,
                parent_pid: 1,
                name: "claude".to_owned(),
                command: "claude".to_owned(),
                memory_bytes: 1,
            }],
            scanned_at: std::time::SystemTime::now(),
        };
        task.domain.lock().process_snapshot = Some(walk());

        // A frame that does not end the run leaves the walk exactly where the
        // seat's own loop put it.
        task.translate_event(sdk_message(background_tasks(one_live_task())));
        assert!(
            announced_walks(&mut updates).is_empty(),
            "a frame that did not end the run announces no walk",
        );
        assert!(task.domain.lock().process_snapshot.is_some(), "and clears none");

        task.translate_event(AgentEvent::ConnectionFailed {
            message: "reader died".to_owned(),
            kind: SpawnFailureKind::Unclassified,
        });

        let cleared = announced_walks(&mut updates);
        assert_eq!(cleared.len(), 1, "the death announces the walk's clear");
        assert!(cleared[0].processes.is_empty(), "with the empty walk the core now holds");
        assert!(
            task.domain.lock().process_snapshot.is_none(),
            "and the store is cleared, so a read answers null rather than a dead tree",
        );
    }

    /// A monitor still running when the connection dies can never be settled,
    /// and the session page reads this set directly through the view surface,
    /// so a dead one draws as live on both surfaces until the next connect
    /// replaces it.
    #[test]
    fn connection_failed_drops_the_domain_monitor_set() {
        let mut domain = empty_domain();
        apply_event_to_domain(&mut domain, &sdk_message(monitor_tool_use("tu-mon", "ci-watch")));
        apply_event_to_domain(&mut domain, &sdk_message(task_started("t-mon", Some("tu-mon"))));
        assert_eq!(domain.monitors.len(), 1, "the monitor is seeded before the failure");
        assert_eq!(
            domain.monitors[0].status,
            forge_primitives::MonitorStatus::Running,
            "and it is still running, which is what makes it unsettleable",
        );

        apply_event_to_domain(
            &mut domain,
            &AgentEvent::ConnectionFailed {
                message: "reader died".to_owned(),
                kind: SpawnFailureKind::Unclassified,
            },
        );

        assert!(
            domain.monitors.is_empty(),
            "the run's monitor set does not outlive the run that held it",
        );
    }

    /// A second `Connected` is a new occupant in the same slot, and it
    /// inherits nothing the previous one held: `background_tasks_changed`
    /// is not re-sent for a session that has not started one, so a
    /// registry left standing would have a row spin over a task nobody is
    /// running until a real snapshot arrives.
    #[test]
    fn a_second_connected_drops_background_work() {
        let mut domain = empty_domain();
        domain.background_work = true;

        apply_event_to_domain(&mut domain, &connected_event("new-uuid", "/proj"));

        assert!(
            !domain.background_work,
            "a replaced occupant's background work must not survive it",
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
        let update_tx = UpdateFanout::default();
        let mut update_rx = update_tx.subscribe(SubscriberRole::Answering);
        let domain = Arc::new(parking_lot::Mutex::new(empty_domain()));
        let (response_tx, mut response_rx) =
            oneshot::channel::<forge_primitives::PermissionOutcome>();
        domain.lock().pending_interactions.insert(
            "stale_tool_id".to_owned(),
            crate::workspace::testing::test_permission(response_tx),
        );
        let mut task = SessionTask {
            key: SessionSlot::from_str_for_test("old-uuid"),
            handle: Arc::new(handle),
            command_rx,
            domain: Arc::clone(&domain),
            update_tx,
            connected_once: true,
            workspace: std::sync::Weak::new(),
            conversation: None,
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
        // The drop is announced, or a view drawing a dock from the request
        // it folded keeps offering a prompt the core has let go, and every
        // click on it reaches nothing.
        let announced: Vec<String> = std::iter::from_fn(|| update_rx.try_recv().ok())
            .filter_map(|update| match update {
                SessionUpdate::PendingInteractionResolved { key, tool_id, .. } => {
                    assert_eq!(key, SessionSlot::from_str_for_test("old-uuid"));
                    Some(tool_id)
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            announced,
            vec!["stale_tool_id".to_owned()],
            "a dropped prompt is announced with its own tool id",
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
        use crate::mcp::peers::types::{MessageId, WrappedKind, WrappedPrompt};

        let (workspace, _update_rx) = crate::Workspace::testing_stub();
        let sender = SessionSlot::from_str_for_test("sender-proj");

        // Park three Messages for the task's slot in known order.
        let session_key = test_slot();
        let domain =
            Arc::new(parking_lot::Mutex::new(DomainSession::new(session_key.clone(), None)));
        let bodies = ["first", "second", "third"];
        for body in bodies {
            workspace.park_peer_prompt(
                &session_key,
                &sender,
                WrappedPrompt {
                    id: MessageId::mint(),
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
            conversation: None,
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
                crate::protocol::Command::Prompt { text, .. }
                | crate::protocol::Command::PromptUnder { text, .. } => Some(text),
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

    /// A cron entry carrying what a park reads: the id, the prompt and a
    /// description.
    fn test_cron(id: &str, prompt: &str) -> forge_primitives::cron::CronEntry {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};
        CronEntry {
            id: CronId::from(id),
            project_name: "cron-drain".to_owned(),
            kind: CronKind::Recurring("0 9 * * *".to_owned()),
            prompt: prompt.to_owned(),
            created_at: std::time::SystemTime::UNIX_EPOCH,
            description: Some(format!("{id} summary")),
            last_fire: None,
            next_fire: std::time::SystemTime::UNIX_EPOCH,
            team_role: None,
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
        workspace.park_cron(&session_key, &test_cron("c1", "morning reminder"), false);

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
            conversation: None,
        };

        workspace.enable_test_dispatch_intercept();
        task.translate_event(connected_event(&session_key.display(), "/tmp/cron-drain"));

        // The buffered cron prompt is dispatched as a plain user turn.
        let buffered = workspace.drain_test_dispatch_buffer();
        assert!(
            buffered.iter().any(|c| matches!(
                c, crate::protocol::Command::Prompt { text, .. }
                    | crate::protocol::Command::PromptUnder { text, .. }
                    if text == "morning reminder"
            )),
            "the buffered cron prompt is dispatched on first-Connected",
        );

        // AND an echo lands so the drained prompt shows a cron block, naming
        // the entry the parked fire came from - the bucket is the only place
        // that identity survives an asleep fire.
        let mut echoed = None;
        while let Ok(u) = update_rx.try_recv() {
            if let SessionUpdate::CronPromptAppended { key, text, cron_id, description, .. } = u
                && key == session_key
                && text == "morning reminder"
            {
                echoed = Some((cron_id, description));
            }
        }
        let (cron_id, description) =
            echoed.expect("an asleep-fired cron echoes a CronPromptAppended on drain");
        assert_eq!(cron_id, "c1", "the drained echo names the entry that fired");
        assert_eq!(description.as_deref(), Some("c1 summary"), "and its description");

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
        workspace.park_cron(&worker_slot, &test_cron("c-worker", "worker work"), true);
        workspace.park_cron(&lead_slot, &test_cron("c-lead", "lead work"), false);

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
            conversation: None,
        };

        workspace.enable_test_dispatch_intercept();
        task.translate_event(connected_event(&worker_slot.display(), "/tmp/wdp"));

        let dispatched = workspace.drain_test_dispatch_buffer();
        assert!(
            dispatched.iter().any(|c| matches!(
                c, crate::protocol::Command::Prompt { key, text, .. }
                    | crate::protocol::Command::PromptUnder { key, text, .. }
                    if *key == session_key && text == "[missed cron] worker work"
            )),
            "the worker drains its own missed cron with the marker applied",
        );
        // The lead's bucket is untouched by the worker's drain.
        let lead_bucket = workspace.take_parked_for_slot(&lead_slot).cron;
        assert_eq!(lead_bucket.len(), 1, "the lead's cron stays buffered");
        assert_eq!(lead_bucket[0].text, "lead work");
    }

    /// Every echo in a multi-fire bucket names its OWN entry: the bucket
    /// holds one entry per due cron, so an identity read once for the
    /// bucket - the shape the loop's surrounding arguments invite - would
    /// print the first schedule on every row. Sorted rather than
    /// positional, so this does not pin drain order.
    #[tokio::test]
    async fn a_drain_names_each_entry_in_a_multi_fire_bucket() {
        let (workspace, mut update_rx) = crate::Workspace::testing_stub();
        workspace.seed_test_project("cron-drain", "/tmp/cron-drain");
        let session_key = SessionSlot::lead("TestOrg", "cron-drain");
        workspace.park_cron(&session_key, &test_cron("c1", "morning reminder"), false);
        workspace.park_cron(&session_key, &test_cron("c2", "worker digest"), true);
        // A once-off registered without a description: the reply's own
        // contract says the key is null then, and the row falls back to the
        // prompt's first line.
        let mut bare = test_cron("c3", "check the queue");
        bare.description = None;
        workspace.park_cron(&session_key, &bare, false);

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
            conversation: None,
        };

        workspace.enable_test_dispatch_intercept();
        task.translate_event(connected_event(&session_key.display(), "/tmp/cron-drain"));

        let mut heard: Vec<(String, String, Option<String>)> = Vec::new();
        while let Ok(u) = update_rx.try_recv() {
            if let SessionUpdate::CronPromptAppended { key, text, cron_id, description, .. } = u
                && key == session_key
            {
                heard.push((text, cron_id, description));
            }
        }
        heard.sort();
        assert_eq!(
            heard,
            vec![
                (
                    "[missed cron] worker digest".to_owned(),
                    "c2".to_owned(),
                    Some("c2 summary".to_owned())
                ),
                ("check the queue".to_owned(), "c3".to_owned(), None),
                ("morning reminder".to_owned(), "c1".to_owned(), Some("c1 summary".to_owned())),
            ],
            "each echo carries its own entry's identity, and a missing description stays null",
        );
    }

    /// **A session that runs long does not carry its whole run.** The copy a
    /// replay answers with is a window, for the reason the transport's held
    /// copy is: what its reader does with it is cut it to the newest turns,
    /// so a copy that grows with the transcript is a transcript-sized
    /// allocation per session, cloned again per replay.
    ///
    /// Both routes in are asserted apart - the history a connect hands over,
    /// and the frames the session emits after it - because each is its own
    /// call site and either could keep the whole run while the other does not.
    #[tokio::test]
    async fn a_session_keeps_its_conversation_in_the_same_window() {
        let (workspace, mut update_rx) = crate::Workspace::testing_stub();
        let session_key = SessionSlot::from_str_for_test("window-uuid");
        let domain =
            Arc::new(parking_lot::Mutex::new(DomainSession::new(session_key.clone(), None)));
        let (handle, _agent_cmd_rx) = Agent::testing_stub();
        let (_cmd_tx, command_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::protocol::Command>();
        let mut task = SessionTask {
            key: session_key.clone(),
            handle: Arc::new(handle),
            command_rx,
            domain,
            update_tx: workspace.update_sender(),
            connected_once: false,
            workspace: Arc::downgrade(&workspace),
            conversation: None,
        };
        let cap = crate::conversation_window::CONVERSATION_CAP;
        let slack = crate::conversation_window::CONVERSATION_SLACK;
        let frame = |at: usize| {
            serde_json::from_value::<forge_primitives::Message>(serde_json::json!({
                "type": "user",
                "message": {"role": "user", "content": format!("frame {at}")},
                "session_id": session_key.display(),
            }))
            .expect("a user frame")
        };
        // A resume, which hands over a transcript twice the cap - the shape
        // the clone this replaced was worst on.
        let mut event = connected_event(&session_key.display(), "/tmp/window");
        if let AgentEvent::Connected { history_updates, .. } = &mut event {
            *history_updates = Some((0..cap * 2).map(frame).collect());
        }
        task.translate_event(event);

        let (seeded, _) = task.conversation.as_ref().expect("the task keeps a conversation");
        assert_eq!(
            seeded.len(),
            cap,
            "a connect hands over a transcript and the copy keeps a window"
        );
        assert!(
            seeded.capacity() <= cap + slack,
            "held in a window-sized store rather than the transcript's: capacity for {} messages",
            seeded.capacity(),
        );

        // And the frames that arrive after it, which is where a long-running
        // session's copy grows.
        for at in 0..=(cap * 2 + slack) {
            task.translate_event(AgentEvent::SdkMessage {
                session_id: session_key.display(),
                msg: frame(at),
            });
        }
        while update_rx.try_recv().is_ok() {}

        task.execute_command(crate::protocol::Command::ReplayConversation {
            key: session_key.clone(),
        });
        let mut replayed = None;
        while let Ok(update) = update_rx.try_recv() {
            if let SessionUpdate::HistoryReplayed { history, .. } = update {
                replayed = Some(history);
            }
        }
        let history = replayed.expect("a replay is answered with the conversation");

        assert!(
            (cap..=cap + slack).contains(&history.len()),
            "however long the session has run, the replay answers with a window: {} messages",
            history.len(),
        );
        assert!(
            !history.iter().any(|message| {
                matches!(message, forge_primitives::Message::User { message, .. }
                    if message.content.iter().any(|block| matches!(block,
                        forge_primitives::ContentBlock::Text { text, .. }
                            if text == "frame 0")))
            }),
            "and the frames it dropped are the oldest ones",
        );

        // **The store is what the cap is for**, so the shape is pinned as well
        // as the length: a drop that drained would trim the copy and keep the
        // transcript's buffer under it.
        let (held, _) = task.conversation.as_ref().expect("the task keeps a conversation");
        assert!(
            held.capacity() <= cap + slack,
            "the copy is held in a window-sized store: capacity for {} messages",
            held.capacity(),
        );
        assert_eq!(held.len(), history.len(), "and holds the frames the replay answered with");
    }

    /// The on-open read of a call's own output: a view asks with the call's
    /// id, and the task answers from its own conversation - the frame that
    /// named the output file is there, and nothing else a read could reach
    /// holds it without walking the transcript.
    #[tokio::test]
    async fn a_calls_output_is_answered_from_the_tasks_own_conversation() {
        let (workspace, mut update_rx) = crate::Workspace::testing_stub();
        let session_key = SessionSlot::from_str_for_test("call-output-uuid");
        let domain =
            Arc::new(parking_lot::Mutex::new(DomainSession::new(session_key.clone(), None)));
        let (handle, _agent_cmd_rx) = Agent::testing_stub();
        let (_cmd_tx, command_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::protocol::Command>();
        let mut task = SessionTask {
            key: session_key.clone(),
            handle: Arc::new(handle),
            command_rx,
            domain,
            update_tx: workspace.update_sender(),
            connected_once: false,
            workspace: Arc::downgrade(&workspace),
            conversation: None,
        };
        task.translate_event(connected_event(&session_key.display(), "/tmp/call-output"));

        // The frame that ended a backgrounded call, in the shape the wire
        // carries it: the call's own id, its status, and the file the
        // command's output went to. More lines than the read answers with,
        // so the bound is pinned rather than incidental.
        let path =
            std::env::temp_dir().join(format!("forge-call-output-{}.log", std::process::id()));
        let written = (0..15).fold(String::new(), |mut all, at| {
            let _ = writeln!(all, "line-{at:02}");
            all
        });
        std::fs::write(&path, written).expect("write the output file");
        task.translate_event(AgentEvent::SdkMessage {
            session_id: session_key.display(),
            msg: serde_json::from_value(serde_json::json!({
                "type": "system",
                "subtype": "task_notification",
                "task_id": "t-1",
                "tool_use_id": "tu-1",
                "status": "completed",
                "output_file": path.display().to_string(),
                "summary": "Background command \"sleep 2\" completed",
                "session_id": session_key.display(),
                "uuid": "u-1",
            }))
            .expect("a task notification frame"),
        });
        while update_rx.try_recv().is_ok() {}

        task.execute_command(crate::protocol::Command::ReadCallOutput {
            key: session_key.clone(),
            call_id: "tu-1".to_owned(),
        });

        let mut answered = None;
        while let Ok(update) = update_rx.try_recv() {
            if let SessionUpdate::CallOutput { key, call_id, output } = update {
                answered = Some((key, call_id, output));
            }
        }
        let (key, call_id, output) = answered.expect("the read is answered by the task");
        assert_eq!(key, session_key, "the answer names the seat it routes on");
        assert_eq!(call_id, "tu-1", "the answer names the call it answers");
        let expected: Vec<String> = (3..15).map(|at| format!("line-{at:02}")).collect();
        assert_eq!(
            output,
            forge_primitives::CallOutput::Lines(expected),
            "and carries the command's own bytes, bounded to the newest twelve lines",
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The read never answers blank: a call the conversation does not name is
    /// the no-path reason, an empty path the frame carries is the same
    /// reason, and a file that is gone is its own.
    #[tokio::test]
    async fn a_calls_output_read_names_what_is_missing() {
        let (workspace, mut update_rx) = crate::Workspace::testing_stub();
        let session_key = SessionSlot::from_str_for_test("call-output-reasons");
        let domain =
            Arc::new(parking_lot::Mutex::new(DomainSession::new(session_key.clone(), None)));
        let (handle, _agent_cmd_rx) = Agent::testing_stub();
        let (_cmd_tx, command_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::protocol::Command>();
        let mut task = SessionTask {
            key: session_key.clone(),
            handle: Arc::new(handle),
            command_rx,
            domain,
            update_tx: workspace.update_sender(),
            connected_once: false,
            workspace: Arc::downgrade(&workspace),
            conversation: None,
        };
        task.translate_event(connected_event(&session_key.display(), "/tmp/call-output-reasons"));

        // A call the conversation names, whose file is not there.
        let gone =
            std::env::temp_dir().join(format!("forge-call-output-gone-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&gone);
        task.translate_event(AgentEvent::SdkMessage {
            session_id: session_key.display(),
            msg: serde_json::from_value(serde_json::json!({
                "type": "system",
                "subtype": "task_notification",
                "task_id": "t-gone",
                "tool_use_id": "tu-gone",
                "status": "completed",
                "output_file": gone.display().to_string(),
                "summary": "Background command \"true\" completed",
                "session_id": session_key.display(),
                "uuid": "u-gone",
            }))
            .expect("a task notification frame"),
        });
        // A call whose frame carries no path at all: the wire really sends an
        // empty string, and reading that as a path would answer "gone" about
        // a file that never existed.
        task.translate_event(AgentEvent::SdkMessage {
            session_id: session_key.display(),
            msg: serde_json::from_value(serde_json::json!({
                "type": "system",
                "subtype": "task_notification",
                "task_id": "t-empty",
                "tool_use_id": "tu-empty",
                "status": "completed",
                "output_file": "",
                "summary": "Background command \"true\" completed",
                "session_id": session_key.display(),
                "uuid": "u-empty",
            }))
            .expect("a task notification frame"),
        });
        while update_rx.try_recv().is_ok() {}

        for call_id in ["tu-unknown", "tu-empty", "tu-gone"] {
            task.execute_command(crate::protocol::Command::ReadCallOutput {
                key: session_key.clone(),
                call_id: call_id.to_owned(),
            });
        }

        let mut answers = Vec::new();
        while let Ok(update) = update_rx.try_recv() {
            if let SessionUpdate::CallOutput { key, call_id, output } = update {
                answers.push((key, call_id, output));
            }
        }
        assert_eq!(
            answers,
            vec![
                (
                    session_key.clone(),
                    "tu-unknown".to_owned(),
                    forge_primitives::CallOutput::NoPath
                ),
                (session_key.clone(), "tu-empty".to_owned(), forge_primitives::CallOutput::NoPath),
                (session_key.clone(), "tu-gone".to_owned(), forge_primitives::CallOutput::FileGone),
            ],
            "each answer names the seat it routes on and the call it answers; a call with no \
             frame and a call with an empty path are both no-path, and a call whose file is gone \
             is gone - none of them blanks",
        );
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
                conversation: None,
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

    /// A replay answers with the conversation the task has CARRIED, not with
    /// the history it was handed at connect.
    ///
    /// **This is the mechanism the transport's seed rests on and nothing else
    /// observed it**: with `retain` mutated to a no-op the whole workspace
    /// suite stays green, and a replay then hands back the connect alone -
    /// which is empty for a fresh session and stops at the connect for a
    /// resumed one, so a consumer joining late gets a seat that never spoke.
    ///
    /// The two halves are asserted apart so a change to either fails on its
    /// own message: the frames the session emitted after its connect, and the
    /// count the connect carried as the boundary moved it.
    #[tokio::test]
    async fn a_replay_answers_with_the_conversation_the_task_has_carried() {
        let (workspace, mut update_rx) = crate::Workspace::testing_stub();
        let session_key = SessionSlot::from_str_for_test("replay-carries-uuid");
        let domain =
            Arc::new(parking_lot::Mutex::new(DomainSession::new(session_key.clone(), None)));
        let (handle, _agent_cmd_rx) = Agent::testing_stub();
        let (_cmd_tx, command_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::protocol::Command>();
        let mut task = SessionTask {
            key: session_key.clone(),
            handle: Arc::new(handle),
            command_rx,
            domain,
            update_tx: workspace.update_sender(),
            connected_once: false,
            workspace: Arc::downgrade(&workspace),
            conversation: None,
        };

        // A FRESH session, which is the case that makes this load-bearing: the
        // CLI hands over an empty history, so a replay answering from the
        // connect alone would hand back nothing at all.
        // A resumed connect carries the count it has already compacted, so the
        // STORE has to keep it and not only the emit: with it zeroed here the
        // replay answers from a count that never saw those compactions.
        let mut event = connected_event(&session_key.display(), "/tmp/replay");
        if let AgentEvent::Connected { history_updates, compaction_count, .. } = &mut event {
            *history_updates = Some(Vec::new());
            *compaction_count = 4;
        }
        task.translate_event(event);

        // One frame the session emitted after it, and one compaction boundary.
        for msg in [
            serde_json::from_value::<forge_primitives::Message>(serde_json::json!({
                "type": "user",
                "message": {"role": "user", "content": "carried"},
                "session_id": session_key.display(),
            }))
            .expect("a user frame"),
            forge_primitives::Message::CompactBoundary {
                trigger: "auto".to_owned(),
                pre_tokens: 1,
                post_tokens: 1,
                uuid: "c1".to_owned(),
                session_id: session_key.display(),
                metadata_extras: serde_json::Map::new(),
                extras: serde_json::Map::new(),
            },
        ] {
            task.translate_event(AgentEvent::SdkMessage { session_id: session_key.display(), msg });
        }
        while update_rx.try_recv().is_ok() {}

        task.execute_command(crate::protocol::Command::ReplayConversation {
            key: session_key.clone(),
        });

        let mut replayed = None;
        while let Ok(update) = update_rx.try_recv() {
            if let SessionUpdate::HistoryReplayed { history, compaction_count, .. } = update {
                replayed = Some((history, compaction_count));
            }
        }
        let (history, compaction_count) =
            replayed.expect("a replay is answered with the conversation");

        assert_eq!(
            history.len(),
            2,
            "the frames the session emitted after its connect are in the replay, not only the \
             empty history the connect carried",
        );
        assert_eq!(
            compaction_count, 5,
            "the count the connect carried survives to the replay, moved by the boundary",
        );
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
            conversation: None,
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
            conversation: None,
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

    /// The login wait comes from the typed signals the CLI sends, and a
    /// turn that finished clears it.
    ///
    /// The `TurnError` envelope's text is a fallback rather than the
    /// signal: its producers are forge's own failed prompt write and
    /// failed cancel, so no CLI output reaches it, and the classifier it
    /// reads is the presentation-time one the TUI warns can fire on
    /// ordinary error words.
    #[test]
    fn a_login_wait_comes_from_the_typed_signals() {
        let held = |event: &AgentEvent| {
            let mut domain = empty_domain();
            apply_event_to_domain(&mut domain, event);
            domain.awaiting_login
        };

        assert!(
            held(&sdk_message(assistant_with("authentication_failed"))),
            "the CLI attributes the failure to the assistant message, and that is typed",
        );
        assert!(
            held(&sdk_message(api_retry("authentication_failed"))),
            "and names it on the retry frame as well",
        );
        assert!(
            !held(&sdk_message(assistant_with("server_error"))),
            "another class is not a login wait",
        );

        // A finished turn proves the credential works. A failed one does
        // not, and clearing on it would undo the mark the frames above
        // just set.
        let mut domain = empty_domain();
        apply_event_to_domain(&mut domain, &sdk_message(assistant_with("authentication_failed")));
        assert!(domain.awaiting_login, "precondition: held");
        apply_event_to_domain(
            &mut domain,
            &sdk_message(result_message("error_during_execution", true)),
        );
        assert!(
            domain.awaiting_login,
            "a turn that died on the missing credential does not clear the wait",
        );
        apply_event_to_domain(&mut domain, &sdk_message(result_message("success", false)));
        assert!(!domain.awaiting_login, "a turn that finished does");
    }

    fn sdk_message(msg: forge_primitives::Message) -> AgentEvent {
        AgentEvent::SdkMessage { session_id: "s".to_owned(), msg }
    }

    fn hook_observation(
        permission_mode: Option<&str>,
        effort: Option<&str>,
        tool_use_id: Option<&str>,
        agent_type: Option<&str>,
    ) -> AgentEvent {
        AgentEvent::HookObservation {
            session_id: "s".to_owned(),
            tool_use_id: tool_use_id.map(str::to_owned),
            permission_mode: permission_mode.map(str::to_owned),
            effort: effort.map(str::to_owned),
            agent_id: tool_use_id.map(|_| "agent-1".to_owned()),
            agent_type: agent_type.map(str::to_owned),
        }
    }

    fn init_frame_naming_the_model(model: &str) -> forge_primitives::Message {
        serde_json::from_value(serde_json::json!({
            "type": "system",
            "subtype": "init",
            "session_id": "s",
            "model": model,
        }))
        .expect("parse an init frame naming a model")
    }

    fn mcp_server(name: &str) -> forge_primitives::McpServerStatus {
        serde_json::from_value(serde_json::json!({ "name": name, "status": "connected" }))
            .expect("parse an MCP server status")
    }

    fn monitor_tool_use(tool_use_id: &str, description: &str) -> forge_primitives::Message {
        serde_json::from_value(serde_json::json!({
            "type": "assistant",
            "message": {
                "id": "msg-mon",
                "role": "assistant",
                "model": "claude-sonnet-5",
                "content": [{
                    "type": "tool_use",
                    "id": tool_use_id,
                    "name": "Monitor",
                    "input": {
                        "description": description,
                        "command": "gh run watch 1",
                        "persistent": true,
                    },
                }],
            },
            "session_id": "s",
        }))
        .expect("parse a Monitor tool_use")
    }

    fn task_started(task_id: &str, tool_use_id: Option<&str>) -> forge_primitives::Message {
        serde_json::from_value(serde_json::json!({
            "type": "system",
            "subtype": "task_started",
            "task_id": task_id,
            "description": "watch CI",
            "uuid": "u-task",
            "session_id": "s",
            "tool_use_id": tool_use_id,
        }))
        .expect("parse a task_started")
    }

    fn task_updated(task_id: &str, status: &str) -> forge_primitives::Message {
        serde_json::from_value(serde_json::json!({
            "type": "system",
            "subtype": "task_updated",
            "task_id": task_id,
            "patch": { "status": status },
            "uuid": "u-upd",
            "session_id": "s",
        }))
        .expect("parse a task_updated")
    }

    /// A terminal patch as the CLI sends one: the status and the instant
    /// it ended, in one frame.
    fn task_updated_at(task_id: &str, status: &str, end_ms: u64) -> forge_primitives::Message {
        serde_json::from_value(serde_json::json!({
            "type": "system",
            "subtype": "task_updated",
            "task_id": task_id,
            "patch": { "status": status, "end_time": end_ms },
            "uuid": "u-upd",
            "session_id": "s",
        }))
        .expect("parse a task_updated carrying an end time")
    }

    fn task_notification(task_id: &str) -> forge_primitives::Message {
        task_notification_with_status(task_id, "completed")
    }

    fn task_notification_with_status(task_id: &str, status: &str) -> forge_primitives::Message {
        serde_json::from_value(serde_json::json!({
            "type": "system",
            "subtype": "task_notification",
            "task_id": task_id,
            "status": status,
            "output_file": "/tmp/forge-test-monitor.out",
            "summary": "Monitor stream ended",
            "uuid": "u-note",
            "session_id": "s",
        }))
        .expect("parse a task_notification")
    }

    /// A `commands_changed` payload carrying entries that parse to none
    /// means the CLI's entry shape changed under us. Storing it would
    /// wipe the list, and `/help` with it, for every view reading
    /// through this, so the prior one stands.
    #[test]
    fn an_unparseable_commands_changed_payload_keeps_the_retained_commands() {
        let mut domain = empty_domain();
        apply_event_to_domain(&mut domain, &sdk_message(init_frame(&["/help"], &[])));

        apply_event_to_domain(
            &mut domain,
            &sdk_message(forge_primitives::Message::CommandsChanged {
                commands: vec![serde_json::json!({"no_name": "x"}), serde_json::json!(7)],
                uuid: "cmd-uuid".to_owned(),
                session_id: "s".to_owned(),
                extras: serde_json::Map::new(),
            }),
        );

        let commands: Vec<&str> =
            domain.available_commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(commands, vec!["/help"], "the prior list stands rather than being wiped");
    }

    /// An empty payload is a real answer - a plugin uninstall - and it
    /// clears.
    #[test]
    fn an_empty_commands_changed_payload_clears_the_retained_commands() {
        let mut domain = empty_domain();
        apply_event_to_domain(&mut domain, &sdk_message(init_frame(&["/help"], &[])));

        apply_event_to_domain(
            &mut domain,
            &sdk_message(forge_primitives::Message::CommandsChanged {
                commands: Vec::new(),
                uuid: "cmd-uuid".to_owned(),
                session_id: "s".to_owned(),
                extras: serde_json::Map::new(),
            }),
        );

        assert!(domain.available_commands.is_empty(), "an empty payload clears the list");
    }

    /// A `Connected` in the same seat is a NEW OCCUPANT, and it has
    /// advertised nothing yet: the CLI sends `system/init` at the head of
    /// a turn only, so the last one's catalogues would stand until the new
    /// occupant's first message. The TUI clears both on the same event,
    /// and after a login swap the stale list came from another account's
    /// config dir - another session's data, not a late one.
    ///
    /// The refill is half the property: a swap that ends no turn - a
    /// mid-turn `/new`, a reconnect after a login - never sees the
    /// `Result` that re-arms the agent read, so the new occupant's own
    /// init has to be the thing that fills the catalogue back in.
    #[test]
    fn a_new_occupant_advertises_no_catalogues_until_its_own_init() {
        let mut domain = empty_domain();
        apply_event_to_domain(&mut domain, &sdk_message(init_frame(&["/help"], &["reviewer"])));

        apply_event_to_domain(&mut domain, &connected_event("new-occupant", "/proj"));

        assert!(
            domain.available_commands.is_empty(),
            "the previous occupant's command list does not stand",
        );
        assert!(domain.available_agents.is_empty(), "nor the agent catalogue it advertised");

        apply_event_to_domain(&mut domain, &sdk_message(init_frame(&["/newhelp"], &["newagent"])));

        let commands: Vec<&str> =
            domain.available_commands.iter().map(|c| c.name.as_str()).collect();
        let agents: Vec<&str> = domain.available_agents.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(commands, vec!["/newhelp"], "the new occupant's own init refills the commands");
        assert_eq!(agents, vec!["newagent"], "and the catalogue, without waiting a turn");
    }

    /// A `system/init` frame, which is where the CLI advertises both
    /// catalogues: bare command names and bare agent names.
    fn init_frame(slash_commands: &[&str], agents: &[&str]) -> forge_primitives::Message {
        serde_json::from_value(serde_json::json!({
            "type": "system",
            "subtype": "init",
            "session_id": "s",
            "slash_commands": slash_commands,
            "agents": agents,
        }))
        .expect("an init frame")
    }

    /// Both lists are facts about the session, so the core keeps them: a
    /// view that arrives after the turn started reads them rather than
    /// waiting for the next init frame, which may be a whole turn away.
    #[test]
    fn the_catalogues_the_cli_advertises_are_retained_on_the_domain() {
        let mut domain = empty_domain();

        apply_event_to_domain(
            &mut domain,
            &sdk_message(init_frame(&["/help", "memory"], &["reviewer"])),
        );

        let commands: Vec<&str> =
            domain.available_commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(commands, vec!["/help", "memory"], "the command list is kept whole");
        let agents: Vec<&str> = domain.available_agents.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(agents, vec!["reviewer"], "and so is the agent catalogue");
    }

    /// A plugin reload re-sends the command list mid-session, and the
    /// retained copy has to follow it or the dropdown goes stale.
    #[test]
    fn a_commands_changed_frame_refreshes_the_retained_commands() {
        let mut domain = empty_domain();
        apply_event_to_domain(&mut domain, &sdk_message(init_frame(&["/help"], &[])));

        let refreshed = forge_primitives::Message::CommandsChanged {
            commands: vec![serde_json::json!({"name": "/reload", "description": "Reloaded"})],
            uuid: "cmd-uuid".to_owned(),
            session_id: "s".to_owned(),
            extras: serde_json::Map::new(),
        };
        apply_event_to_domain(&mut domain, &sdk_message(refreshed));

        let commands: Vec<&str> =
            domain.available_commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(commands, vec!["/reload"], "the reload's list replaces the init one");
    }

    /// The CLI re-fires `system/init` every turn. A frame that advertises
    /// no commands carries nothing about them, so it must not wipe what
    /// the previous turn established.
    #[test]
    fn an_init_frame_with_no_commands_keeps_the_retained_ones() {
        let mut domain = empty_domain();
        apply_event_to_domain(&mut domain, &sdk_message(init_frame(&["/help"], &["reviewer"])));

        apply_event_to_domain(&mut domain, &sdk_message(init_frame(&[], &["reviewer"])));

        assert_eq!(domain.available_commands.len(), 1, "the earlier list stands");
        assert_eq!(domain.available_agents.len(), 1, "and the catalogue with it");
    }

    /// The agent catalogue is read once per turn, which is the rule the
    /// TUI's own walker applies: a re-fire inside the same turn is the
    /// same list, and the turn boundary is what re-arms the read.
    #[test]
    fn the_agent_catalogue_is_read_once_per_turn() {
        let mut domain = empty_domain();
        apply_event_to_domain(&mut domain, &sdk_message(init_frame(&[], &["reviewer"])));

        apply_event_to_domain(&mut domain, &sdk_message(init_frame(&[], &["someone-else"])));
        assert_eq!(
            domain.available_agents.first().map(|a| a.name.as_str()),
            Some("reviewer"),
            "a re-fire inside the turn does not replace the catalogue",
        );

        apply_event_to_domain(&mut domain, &sdk_message(result_message("success", false)));
        apply_event_to_domain(&mut domain, &sdk_message(init_frame(&[], &["someone-else"])));
        assert_eq!(
            domain.available_agents.first().map(|a| a.name.as_str()),
            Some("someone-else"),
            "and the next turn's frame does",
        );
    }

    /// The hook observation carries three facts a view renders and the
    /// TUI alone holds today: the permission mode, the effort level and
    /// the tool_use -> agent-type attribution. All three land on the
    /// session so a view reads them instead of folding the same event
    /// again.
    #[test]
    fn a_hook_observation_is_held_on_the_domain() {
        let mut domain = empty_domain();

        apply_event_to_domain(
            &mut domain,
            &hook_observation(Some("acceptEdits"), Some("xhigh"), Some("tu-1"), Some("Explore")),
        );

        assert_eq!(
            domain.observed_permission_mode,
            Some(forge_primitives::PermissionMode::AcceptEdits),
            "the hook's permission mode is kept",
        );
        assert_eq!(
            domain.observed_effort,
            Some(forge_primitives::EffortLevel::Xhigh),
            "and its effort level",
        );
    }

    /// A level the CLI spells differently is a level forge cannot name,
    /// not a reason to forget the one it already has.
    #[test]
    fn an_unreadable_hook_effort_keeps_the_level_already_held() {
        let mut domain = empty_domain();
        domain.observed_effort = Some(forge_primitives::EffortLevel::High);

        apply_event_to_domain(&mut domain, &hook_observation(None, Some("turbo"), None, None));

        assert_eq!(
            domain.observed_effort,
            Some(forge_primitives::EffortLevel::High),
            "an unreadable level leaves the held one standing",
        );
    }

    /// The MCP set is per session, and the core is the one that asks for
    /// it, so the answer is kept rather than only passed through.
    #[test]
    fn the_mcp_snapshot_the_bridge_returns_is_held() {
        let mut domain = empty_domain();

        apply_event_to_domain(
            &mut domain,
            &AgentEvent::McpSnapshot {
                session_id: "s".to_owned(),
                servers: vec![mcp_server("ctx7")],
                error: None,
            },
        );

        let held = domain.mcp_servers.as_ref().expect("the snapshot is kept");
        assert_eq!(held.servers.len(), 1, "the server list is kept whole");
        assert_eq!(held.servers[0].name, "ctx7", "and names the server it carries");
        assert_eq!(held.error, None, "with no error standing beside it");
    }

    /// A later snapshot replaces the one before it, including when the
    /// bridge reports a failure: a stale `connected` row is worse than
    /// no row.
    #[test]
    fn a_failed_mcp_snapshot_replaces_the_one_before_it() {
        let mut domain = empty_domain();
        apply_event_to_domain(
            &mut domain,
            &AgentEvent::McpSnapshot {
                session_id: "s".to_owned(),
                servers: vec![mcp_server("ctx7")],
                error: None,
            },
        );

        apply_event_to_domain(
            &mut domain,
            &AgentEvent::McpSnapshot {
                session_id: "s".to_owned(),
                servers: Vec::new(),
                error: Some("the CLI refused".to_owned()),
            },
        );

        let held = domain.mcp_servers.as_ref().expect("the snapshot is kept");
        assert!(held.servers.is_empty(), "the failed read's empty set replaces the old one");
        assert_eq!(held.error.as_deref(), Some("the CLI refused"), "and carries why");
    }

    /// Context usage is the header's `ctx` reading, and it survives its
    /// own refresh: the poll that fills it lands on the session rather
    /// than only on the view that asked.
    #[test]
    fn the_context_usage_the_bridge_reports_is_held() {
        let mut domain = empty_domain();

        apply_event_to_domain(
            &mut domain,
            &AgentEvent::ContextUsage {
                session_id: "s".to_owned(),
                percentage: Some(62),
                max_tokens: Some(1_000_000),
            },
        );

        let held = domain.context_usage.expect("the usage is kept");
        assert_eq!(held.percent, Some(62), "the percentage is kept");
        assert_eq!(held.max_tokens, Some(1_000_000), "and the window it is a share of");
    }

    /// The model a session runs is stated at connect, and a view reading
    /// later must get the session's own rather than nothing.
    #[test]
    fn a_connected_session_holds_the_model_it_reported() {
        let mut domain = empty_domain();

        apply_event_to_domain(&mut domain, &connected_event("uuid-1", "/proj"));

        assert_eq!(
            domain.current_model.as_ref().map(|m| m.resolved_id.as_str()),
            Some("claude"),
            "the model the connect reported is kept",
        );
    }

    /// The CLI names the model each turn runs under at the head of the
    /// turn, and that frame is how a mid-session switch reaches a reader:
    /// after `/model` the terminal's own row moves, and a read that only
    /// listened at connect would go on naming the model the session left.
    #[test]
    fn the_model_follows_a_later_turns_init_frame() {
        let mut domain = empty_domain();
        apply_event_to_domain(&mut domain, &connected_event("uuid-1", "/proj"));
        apply_event_to_domain(
            &mut domain,
            &sdk_message(init_frame_naming_the_model("claude-opus-5-5")),
        );

        assert_eq!(
            domain.current_model.as_ref().map(|model| model.resolved_id.as_str()),
            Some("claude-opus-5-5"),
            "the model the turn runs under is the model the session is said to be on",
        );
    }

    /// The frame arrives at the head of every turn, including the turns
    /// that changed nothing, so a session that is not switching keeps the
    /// name it resolved at connect rather than being renamed to the CLI's
    /// own spelling of it.
    #[test]
    fn an_init_frame_naming_the_same_model_keeps_the_resolved_name() {
        let mut domain = empty_domain();
        apply_event_to_domain(&mut domain, &connected_event("uuid-1", "/proj"));
        domain.current_model = Some(forge_primitives::CurrentModel {
            requested_id: Some("Opus (1M context)".to_owned()),
            resolved_id: "claude".to_owned(),
            display_name_short: "Opus (1M context)".to_owned(),
            display_name_long: "Opus (1M context)".to_owned(),
            ..forge_primitives::CurrentModel::new("claude", "claude", "claude")
        });

        apply_event_to_domain(&mut domain, &sdk_message(init_frame_naming_the_model("claude")));

        assert_eq!(
            domain.current_model.as_ref().map(|model| model.display_name_long.as_str()),
            Some("Opus (1M context)"),
            "a turn that switched nothing does not rename the session",
        );
    }

    /// A replacement occupant inherits none of the last one's facts: the
    /// hook mirrors describe a session that is gone, and the process tree
    /// belonged to a subprocess that exited with it.
    #[test]
    fn a_replaced_occupant_clears_the_held_view_facts() {
        let mut domain = empty_domain();
        apply_event_to_domain(
            &mut domain,
            &hook_observation(Some("plan"), Some("max"), Some("tu-1"), Some("Explore")),
        );
        domain.mcp_servers = Some(crate::domain_session::McpServers::default());
        domain.context_usage =
            Some(crate::domain_session::ContextUsage { percent: Some(10), max_tokens: None });

        apply_event_to_domain(&mut domain, &connected_event("uuid-2", "/proj"));

        assert_eq!(domain.observed_permission_mode, None, "the dead run's mode does not stand");
        assert_eq!(domain.observed_effort, None, "nor its effort");
        assert_eq!(domain.mcp_servers, None, "nor the servers it had connected");
        assert_eq!(domain.context_usage, None, "nor the context it had filled");
    }

    /// A login wait and a dead connection both blank the session's
    /// runtime identity, and a view renders that: what the core holds has
    /// to go with the run it described, or the read answers with a mode
    /// and a model the session no longer has.
    #[test]
    fn a_login_wait_clears_the_held_view_facts() {
        let mut domain = seeded_view_facts();

        apply_event_to_domain(
            &mut domain,
            &AgentEvent::AuthRequired {
                method_name: "oauth".to_owned(),
                method_description: "sign in".to_owned(),
            },
        );

        assert_view_facts_cleared(&domain, "no credential");
    }

    /// The same, for the other event that ends a run.
    #[test]
    fn a_dead_connection_clears_the_held_view_facts() {
        let mut domain = seeded_view_facts();

        apply_event_to_domain(
            &mut domain,
            &AgentEvent::ConnectionFailed {
                message: "reader died".to_owned(),
                kind: SpawnFailureKind::Unclassified,
            },
        );

        assert_view_facts_cleared(&domain, "the subprocess died");
    }

    fn seeded_view_facts() -> DomainSession {
        let mut domain = empty_domain();
        apply_event_to_domain(
            &mut domain,
            &hook_observation(Some("plan"), Some("max"), Some("tu-1"), Some("Explore")),
        );
        domain.current_model =
            Some(forge_primitives::CurrentModel::new("claude-opus-5", "Opus", "Claude Opus 5"));
        domain.mcp_servers = Some(crate::domain_session::McpServers::default());
        domain.context_usage =
            Some(crate::domain_session::ContextUsage { percent: Some(10), max_tokens: None });
        domain
    }

    fn assert_view_facts_cleared(domain: &DomainSession, why: &str) {
        assert_eq!(domain.observed_permission_mode, None, "{why} leaves no mode standing");
        assert_eq!(domain.observed_effort, None, "{why} leaves no effort standing");
        assert_eq!(domain.current_model, None, "{why} leaves no model standing");
        assert_eq!(domain.context_usage, None, "{why} leaves no context reading standing");
        assert_eq!(domain.mcp_servers, None, "{why} leaves no server snapshot standing");
    }

    /// The walk describes a subprocess tree, so it goes with the process: left
    /// standing it paints rows for programs that are not there, and nothing
    /// following a dead session would ever replace it. The clear reaches the
    /// stream as an empty walk, which is what the monitors and the background
    /// registry already do on the same event.
    ///
    /// **This reverses an earlier call** that kept the walk readable after a
    /// failure, on the view going on painting it - the ageing note said the
    /// tree was old, never that it was gone.
    #[test]
    fn a_dead_connection_clears_the_process_walk() {
        let mut domain = empty_domain();
        domain.process_snapshot = Some(forge_agent::env::processes::ProcessSnapshot {
            processes: Vec::new(),
            scanned_at: std::time::SystemTime::UNIX_EPOCH,
        });

        apply_event_to_domain(
            &mut domain,
            &AgentEvent::ConnectionFailed {
                message: "reader died".to_owned(),
                kind: SpawnFailureKind::Unclassified,
            },
        );

        assert!(
            domain.process_snapshot.is_none(),
            "the tree went with the subprocess, so the read has nothing to serve",
        );
    }

    /// A Monitor tool call is live work, and the core holds it so a view
    /// that is not the TUI can draw it.
    #[test]
    fn a_monitor_tool_call_enters_the_held_set() {
        let mut domain = empty_domain();

        apply_event_to_domain(&mut domain, &sdk_message(monitor_tool_use("tu-mon", "ci-watch")));

        assert_eq!(domain.monitors.len(), 1, "the call enters the set");
        let held = &domain.monitors[0];
        assert_eq!(held.tool_use_id, "tu-mon", "keyed by the tool_use that started it");
        assert_eq!(held.description, "ci-watch", "carrying its headline");
        assert_eq!(held.command, "gh run watch 1", "and the command it watches");
        assert_eq!(
            held.status,
            forge_primitives::MonitorStatus::Running,
            "and it starts running, which is the only state the tool call proves",
        );
        assert_eq!(held.task_id, None, "with no task id the wire has not named yet");
    }

    /// The monitor's terminal transition is keyed by the CLI's task id,
    /// so the `task_started` stamp is what lets a later `task_updated`
    /// find the record at all.
    #[test]
    fn a_monitors_task_transition_settles_its_held_record() {
        let mut domain = empty_domain();
        apply_event_to_domain(&mut domain, &sdk_message(monitor_tool_use("tu-mon", "ci-watch")));

        apply_event_to_domain(&mut domain, &sdk_message(task_started("t-1", Some("tu-mon"))));
        assert_eq!(
            domain.monitors[0].task_id.as_deref(),
            Some("t-1"),
            "the task id the CLI assigned is stamped on the record",
        );

        apply_event_to_domain(&mut domain, &sdk_message(task_updated("t-1", "completed")));

        assert_eq!(
            domain.monitors[0].status,
            forge_primitives::MonitorStatus::Completed,
            "a clean exit settles the monitor",
        );
    }

    /// The instant a monitor ended rides the frame that settles it, and a
    /// view draws the age from it. Without it the record says how a
    /// monitor ended and not when, which is the whole difference between
    /// `completed` and `completed 12m`.
    #[test]
    fn a_monitors_terminal_transition_stamps_the_instant_it_ended() {
        let mut domain = empty_domain();
        apply_event_to_domain(&mut domain, &sdk_message(monitor_tool_use("tu-mon", "ci-watch")));
        apply_event_to_domain(&mut domain, &sdk_message(task_started("t-1", Some("tu-mon"))));

        apply_event_to_domain(
            &mut domain,
            &sdk_message(task_updated_at("t-1", "completed", 1_700_000_000_123)),
        );

        assert_eq!(
            domain.monitors[0].ended_at,
            Some(
                std::time::SystemTime::UNIX_EPOCH
                    + std::time::Duration::from_millis(1_700_000_000_123)
            ),
            "the frame that ends the monitor is the frame that says when",
        );
    }

    /// A running monitor has no end instant, and a read that carried one
    /// would draw an age on work that is still going.
    #[test]
    fn a_running_monitor_carries_no_end_instant() {
        let mut domain = empty_domain();
        apply_event_to_domain(&mut domain, &sdk_message(monitor_tool_use("tu-mon", "ci-watch")));
        apply_event_to_domain(&mut domain, &sdk_message(task_started("t-1", Some("tu-mon"))));
        apply_event_to_domain(&mut domain, &sdk_message(task_updated("t-1", "running")));

        assert_eq!(
            domain.monitors[0].ended_at, None,
            "a monitor still watching has not ended, so it has nothing to age",
        );
    }

    /// A monitor that was already running before this process started is
    /// in the transcript the connect carries, and the transcript carries
    /// no lifecycle frame at all: the replay synthesizer emits user and
    /// assistant messages and nothing else. So a seeded entry is settled
    /// on arrival - one seeded running could never be settled, because
    /// every settlement is keyed on the task id the transcript cannot
    /// carry.
    #[test]
    fn a_monitor_folded_from_a_transcript_is_seeded_settled() {
        let mut domain = empty_domain();

        fold_monitor(
            &mut domain,
            &monitor_tool_use("tu-mon", "ci-watch"),
            MonitorOrigin::Transcript,
        );

        assert_eq!(domain.monitors.len(), 1, "the transcript's monitor is seeded");
        assert_eq!(
            domain.monitors[0].status,
            forge_primitives::MonitorStatus::Completed,
            "settled rather than left falsely running forever",
        );
        assert_eq!(
            domain.monitors[0].task_id, None,
            "with no task id, which is why nothing could ever settle it",
        );
    }

    /// A live tool call is not settled: its task is running and the
    /// command frames that settle it are still to come.
    #[test]
    fn a_monitor_folded_from_the_wire_is_seeded_running() {
        let mut domain = empty_domain();

        fold_monitor(&mut domain, &monitor_tool_use("tu-mon", "ci-watch"), MonitorOrigin::Wire);

        assert_eq!(
            domain.monitors[0].status,
            forge_primitives::MonitorStatus::Running,
            "a call on the live wire is running until the command says otherwise",
        );
    }

    /// The monitor set drains once every entry is terminal, which is what
    /// the terminal does: a session that ran a monitor an hour ago shows
    /// no section, not a section with nothing to say.
    #[test]
    fn the_monitor_set_drains_once_every_entry_is_terminal() {
        let mut domain = empty_domain();
        apply_event_to_domain(&mut domain, &sdk_message(monitor_tool_use("tu-a", "ci-watch")));
        apply_event_to_domain(&mut domain, &sdk_message(monitor_tool_use("tu-b", "deploy-gate")));
        apply_event_to_domain(&mut domain, &sdk_message(task_started("t-a", Some("tu-a"))));
        apply_event_to_domain(&mut domain, &sdk_message(task_started("t-b", Some("tu-b"))));

        apply_event_to_domain(&mut domain, &sdk_message(task_updated("t-a", "completed")));
        assert_eq!(
            domain.monitors.len(),
            2,
            "one settled monitor keeps the section, holding its tail"
        );

        apply_event_to_domain(&mut domain, &sdk_message(task_notification("t-b")));

        assert!(
            domain.monitors.is_empty(),
            "every entry terminal drains the set rather than leaving an empty section",
        );
    }

    /// A notification whose status forge cannot name is still a
    /// notification, and it is the last frame a monitor sends: the
    /// terminal drains on every one of them, so a status the CLI adds
    /// later must not leave the core's set standing forever.
    #[test]
    fn an_unnameable_task_status_still_drains_the_set() {
        let mut domain = empty_domain();
        apply_event_to_domain(&mut domain, &sdk_message(monitor_tool_use("tu-a", "ci-watch")));
        apply_event_to_domain(&mut domain, &sdk_message(task_started("t-a", Some("tu-a"))));
        // Settled by the command frame, which deliberately does not drain:
        // the notification that follows is what carries the tail.
        apply_event_to_domain(&mut domain, &sdk_message(task_updated("t-a", "killed")));

        apply_event_to_domain(
            &mut domain,
            &sdk_message(task_notification_with_status("t-a", "reticulating")),
        );

        assert!(
            domain.monitors.is_empty(),
            "a notification drains the set whatever its status says",
        );
    }

    /// The same drain runs over a transcript's seed, so a resumed session
    /// whose monitors were all over shows none of them.
    #[test]
    fn a_resumed_sessions_history_leaves_no_live_monitor() {
        let mut domain = empty_domain();
        let mut connected = connected_event("uuid-1", "/proj");
        if let AgentEvent::Connected { history_updates, .. } = &mut connected {
            *history_updates = Some(vec![
                monitor_tool_use("tu-mon", "ci-watch"),
                monitor_tool_use("tu-other", "deploy-gate"),
            ]);
        }

        apply_event_to_domain(&mut domain, &connected);

        assert!(
            domain.monitors.is_empty(),
            "a transcript cannot say a monitor is still running, so none of them are drawn",
        );
    }

    fn assistant_with(error: &str) -> forge_primitives::Message {
        serde_json::from_value(serde_json::json!({
            "type": "assistant",
            "message": {
                "id": "msg-1",
                "role": "assistant",
                "model": "claude-sonnet-5",
                "content": [],
            },
            "session_id": "s",
            "error": error,
        }))
        .expect("parse an assistant message")
    }

    fn api_retry(error: &str) -> forge_primitives::Message {
        serde_json::from_value(serde_json::json!({
            "type": "system",
            "subtype": "api_retry",
            "session_id": "s",
            "attempt": 1,
            "max_retries": 3,
            "retry_delay_ms": 100,
            "error": error,
        }))
        .expect("parse a retry frame")
    }

    /// A failed connection is recorded where a view can read it, and a
    /// `Connected` clears it.
    ///
    /// Both halves go through `translate_event`, the dispatcher the run
    /// loop uses, and that is what this test is for: the record is in the
    /// failure arm and the clear is in the `Connected` arm rather than in
    /// `register_domain_session`, which production never calls - the
    /// spawn entries hoist a domain straight into `domain_handles` and
    /// insert it themselves.
    ///
    /// Both `Connected` branches are driven, because narrowing the clear
    /// into the replacement branch is the plausible mistake and it leaves
    /// the common recovery reading failed for the life of the process: a
    /// fresh task's FIRST connect after a retry.
    #[tokio::test]
    async fn a_connection_failure_is_recorded_and_a_connected_clears_it() {
        let (workspace, _update_rx) = crate::Workspace::testing_stub();

        for (key, connected_once) in [
            (SessionSlot::from_str_for_test("record-failure-replaced"), true),
            (SessionSlot::from_str_for_test("record-failure-fresh"), false),
        ] {
            let (handle, _agent_cmds) = Agent::testing_stub();
            let arc = Arc::new(handle);
            let domain = workspace.register_domain_session(key.clone(), Some(Arc::clone(&arc)));
            let (_cmd_tx, command_rx) =
                tokio::sync::mpsc::unbounded_channel::<crate::protocol::Command>();
            let mut task = SessionTask {
                key: key.clone(),
                handle: arc,
                command_rx,
                domain,
                update_tx: workspace.update_sender(),
                connected_once,
                workspace: Arc::downgrade(&workspace),
                conversation: None,
            };

            task.translate_event(AgentEvent::ConnectionFailed {
                message: "the bucket died".to_owned(),
                kind: SpawnFailureKind::Unclassified,
            });
            assert_eq!(
                workspace.spawn_failure(&key).as_deref(),
                Some("the bucket died"),
                "a failed connection is recorded where a view can read it ({connected_once})",
            );

            task.translate_event(connected_event("fresh-uuid", "/proj"));

            assert_eq!(
                workspace.spawn_failure(&key),
                None,
                "and a session that comes up clears it rather than reading failed for the \
                 process ({connected_once})",
            );
        }
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
            conversation: None,
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
                None,
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
            conversation: None,
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

    /// A prompt is recorded on the seat as waiting, announced with its words,
    /// and sent under the SAME id - which is the whole mechanism: the CLI's
    /// lifecycle frames carry only the id and the state, so the row and the
    /// frames are one thing by id or not at all.
    #[tokio::test]
    async fn executing_a_prompt_records_and_announces_it() {
        let (workspace, mut updates) = crate::Workspace::testing_stub();
        workspace.seed_test_project("qp", "/tmp/qp");
        let key = SessionSlot::from_str_for_test("qp-lead");
        let domain = Arc::new(parking_lot::Mutex::new(DomainSession::new(key.clone(), None)));
        // A connected seat: the id is what the prompt is addressed to, and a
        // task that never connected drops the send rather than recording it.
        domain.lock().session_id = Some(forge_primitives::SessionId::new("qp-session"));
        let (handle, mut agent_rx) = Agent::testing_stub();
        let (_cmd_tx, command_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::protocol::Command>();
        let task = SessionTask {
            key: key.clone(),
            handle: Arc::new(handle),
            command_rx,
            domain: Arc::clone(&domain),
            update_tx: workspace.update_sender(),
            connected_once: false,
            workspace: Arc::downgrade(&workspace),
            conversation: None,
        };

        task.execute_command(Command::Prompt {
            key: key.clone(),
            text: "the queued words".to_owned(),
            attachments: Vec::new(),
        });

        let rows = domain.lock().prompt_queue.clone();
        assert_eq!(rows.len(), 1, "the prompt is on the seat's queue");
        assert_eq!(rows[0].text, "the queued words");
        assert_eq!(rows[0].source, PromptSource::You);

        let sent = agent_rx.try_recv().expect("the prompt reaches the agent");
        let forge_primitives::AgentCommand::PromptWithImages { uuid, .. } = sent else {
            panic!("expected a prompt, got {sent:?}");
        };
        assert_eq!(
            uuid, rows[0].uuid,
            "the recorded row and the frame share one id, which is what the lifecycle frames resolve against",
        );

        let announced: Vec<crate::protocol::SessionUpdate> =
            std::iter::from_fn(|| updates.try_recv().ok()).collect();
        assert!(
            announced.iter().any(|update| matches!(
                update,
                crate::protocol::SessionUpdate::PromptQueued { uuid: announced, text, .. }
                    if announced == &rows[0].uuid && text == "the queued words"
            )),
            "the row's words ride the announcement, because no lifecycle frame carries them: {announced:?}",
        );
    }

    /// A session whose CLI says it does not carry the lifecycle frames records
    /// NO queued rows: nothing could ever settle one, and a card that can never
    /// drain is worse than no card - that session draws its prompt the way it
    /// did before the pile existed.
    #[tokio::test]
    async fn a_cli_without_lifecycle_frames_records_nothing() {
        let (workspace, mut updates) = crate::Workspace::testing_stub();
        workspace.seed_test_project("nf", "/tmp/nf");
        let key = SessionSlot::from_str_for_test("nf-lead");
        let domain = Arc::new(parking_lot::Mutex::new(DomainSession::new(key.clone(), None)));
        domain.lock().session_id = Some(forge_primitives::SessionId::new("nf-session"));
        let (handle, mut agent_rx) = Agent::testing_stub();
        let (_cmd_tx, command_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::protocol::Command>();
        let mut task = SessionTask {
            key: key.clone(),
            handle: Arc::new(handle),
            command_rx,
            domain: Arc::clone(&domain),
            update_tx: workspace.update_sender(),
            connected_once: false,
            workspace: Arc::downgrade(&workspace),
            conversation: None,
        };

        // An init frame from a CLI that does not advertise the capability - the
        // one thing that turns the pile off for this seat.
        let init: forge_primitives::Message = serde_json::from_value(serde_json::json!({
            "type": "system",
            "subtype": "init",
            "session_id": "nf-session",
            "capabilities": ["interrupt_receipt_v1"],
        }))
        .expect("parse an init frame");
        task.translate_event(forge_agent::AgentEvent::SdkMessage {
            session_id: "nf-session".to_owned(),
            msg: init,
        });

        task.execute_command(Command::Prompt {
            key: key.clone(),
            text: "this one is not recorded".to_owned(),
            attachments: Vec::new(),
        });

        assert!(
            domain.lock().prompt_queue.is_empty(),
            "a prompt is recorded only where the CLI can settle it",
        );
        assert!(
            agent_rx.try_recv().is_ok(),
            "the prompt itself still reaches the agent: the pile is a view, not the send",
        );
        let announced: Vec<crate::protocol::SessionUpdate> =
            std::iter::from_fn(|| updates.try_recv().ok()).collect();
        assert!(
            !announced.iter().any(|update| matches!(
                update,
                crate::protocol::SessionUpdate::PromptQueued { .. }
            )),
            "nothing is announced for a row that is not kept: {announced:?}",
        );
    }

    /// The capability latch is per-occupant and re-reads.
    ///
    /// The flag and the pile go with the occupant that set them - the same
    /// drop the `Connected` arm runs - so a `/resume` onto a CLI that DOES
    /// advertise the frames gets its pile back, and one that does not cannot
    /// inherit a latched true beside rows nothing can settle. A latch that
    /// only ever wrote once would keep the pile silently off for the rest of
    /// the slot's life.
    #[tokio::test]
    async fn the_capability_latch_re_reads_on_a_new_occupant() {
        let (workspace, mut updates) = crate::Workspace::testing_stub();
        workspace.seed_test_project("latch", "/tmp/latch");
        let key = SessionSlot::from_str_for_test("latch-lead");
        let domain = Arc::new(parking_lot::Mutex::new(DomainSession::new(key.clone(), None)));
        domain.lock().session_id = Some(forge_primitives::SessionId::new("latch-session"));
        let (handle, _agent_rx) = Agent::testing_stub();
        let (_cmd_tx, command_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::protocol::Command>();
        let mut task = SessionTask {
            key: key.clone(),
            handle: Arc::new(handle),
            command_rx,
            domain: Arc::clone(&domain),
            update_tx: workspace.update_sender(),
            connected_once: false,
            workspace: Arc::downgrade(&workspace),
            conversation: None,
        };

        let init = |capabilities: serde_json::Value| -> forge_primitives::Message {
            serde_json::from_value(serde_json::json!({
                "type": "system",
                "subtype": "init",
                "session_id": "latch-session",
                "capabilities": capabilities,
            }))
            .expect("parse an init frame")
        };

        // The first occupant does not advertise: the pile is off for it.
        task.translate_event(forge_agent::AgentEvent::SdkMessage {
            session_id: "latch-session".to_owned(),
            msg: init(serde_json::json!(["interrupt_receipt_v1"])),
        });
        assert_eq!(domain.lock().lifecycle_frames, Some(false), "the first init is read");

        // A new occupant: the per-occupant drop `Connected` runs.
        domain.lock().drop_background_tasks();
        assert_eq!(domain.lock().lifecycle_frames, None, "nothing has been advertised yet");
        assert!(domain.lock().prompt_queue.is_empty(), "nor is anything waiting");

        // The new occupant advertises, so its init is the first sight again -
        // and the pile works for it.
        task.translate_event(forge_agent::AgentEvent::SdkMessage {
            session_id: "latch-session".to_owned(),
            msg: init(serde_json::json!(["msg_lifecycle_v1"])),
        });
        assert_eq!(
            domain.lock().lifecycle_frames,
            Some(true),
            "the re-read is what gives the new occupant its pile",
        );

        task.execute_command(Command::Prompt {
            key: key.clone(),
            text: "recorded now".to_owned(),
            attachments: Vec::new(),
        });
        assert_eq!(
            domain.lock().prompt_queue.len(),
            1,
            "and a row is kept for a CLI that can settle it",
        );
        let announced: Vec<crate::protocol::SessionUpdate> =
            std::iter::from_fn(|| updates.try_recv().ok()).collect();
        assert!(
            announced.iter().any(|update| matches!(
                update,
                crate::protocol::SessionUpdate::PromptQueued { text, .. } if text == "recorded now"
            )),
            "and announced: {announced:?}",
        );
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

    fn background_tasks(tasks: Vec<serde_json::Value>) -> forge_primitives::Message {
        forge_primitives::Message::BackgroundTasksChanged {
            tasks,
            uuid: "u1".to_owned(),
            session_id: "worker".to_owned(),
            extras: serde_json::Map::new(),
        }
    }

    fn one_live_task() -> Vec<serde_json::Value> {
        vec![serde_json::json!({
            "task_id": "t1",
            "task_type": "local_bash",
            "description": "gh run watch",
        })]
    }

    /// The registry a PROCESSES view leads its rows with, with the command
    /// each task's own tool call carried: the bool alone told a view that
    /// something was running and nothing about what.
    ///
    /// The order is the CLI's own, from the capture: the card that names the
    /// command, then the registry that names the task, then `task_started`
    /// that links the two.
    #[test]
    fn the_registry_carries_each_tasks_own_command() {
        let (workspace, _rx) = crate::Workspace::testing_stub();
        let key = SessionSlot::from_str_for_test("w-bg");
        let (mut task, _update_rx) = review_task_for(&workspace, &key);
        let send = |task: &mut SessionTask, msg: forge_primitives::Message| {
            task.translate_event(AgentEvent::SdkMessage { session_id: "worker".to_owned(), msg });
        };

        send(&mut task, a_backgrounded_bash("toolu_1", "gh run watch 123 --exit-status"));
        send(&mut task, background_tasks(one_live_task()));
        send(&mut task, a_task_started("t1", "toolu_1"));

        let held = task.domain.lock();
        let tasks = held.background_tasks.clone();
        assert_eq!(tasks.len(), 1, "the CLI's own entry is held: {tasks:?}");
        assert_eq!(tasks[0].task_id, "t1", "under the id the CLI gave it");
        assert_eq!(tasks[0].task_type, "local_bash", "with the kind a row routes on");
        assert_eq!(tasks[0].description, "gh run watch", "and the line the row leads with");
        assert_eq!(
            tasks[0].command.as_deref(),
            Some("gh run watch 123 --exit-status"),
            "and the command its own card carried, which the scan adopts on",
        );
        drop(held);

        send(&mut task, background_tasks(Vec::new()));
        assert!(
            task.domain.lock().background_tasks.is_empty(),
            "the whole set arrives each change, so an empty snapshot clears",
        );
    }

    /// The same link carries the CALL, the id a view jumps by: a registry row
    /// names the tool_use_id `task_started` held for its task, whichever order
    /// the two halves land in.
    #[test]
    fn the_registry_carries_the_tool_call_its_link_names() {
        let (workspace, _rx) = crate::Workspace::testing_stub();
        let key = SessionSlot::from_str_for_test("w-bg");
        let (mut task, _update_rx) = review_task_for(&workspace, &key);
        let send = |task: &mut SessionTask, msg: forge_primitives::Message| {
            task.translate_event(AgentEvent::SdkMessage { session_id: "worker".to_owned(), msg });
        };

        // Registry first, then the link - the CLI's own order.
        send(&mut task, background_tasks(one_live_task()));
        assert_eq!(
            task.domain.lock().background_tasks[0].tool_use_id,
            None,
            "before the link lands the row carries no call rather than a guess",
        );
        send(&mut task, a_task_started("t1", "toolu_1"));
        assert_eq!(
            task.domain.lock().background_tasks[0].tool_use_id.as_deref(),
            Some("toolu_1"),
            "the link fills the call once the registry is held",
        );

        // The reverse order on a fresh seat: the link first, then the snapshot.
        let key = SessionSlot::from_str_for_test("w-bg2");
        let (mut task, _update_rx) = review_task_for(&workspace, &key);
        send(&mut task, a_task_started("t1", "toolu_1"));
        send(&mut task, background_tasks(one_live_task()));
        assert_eq!(
            task.domain.lock().background_tasks[0].tool_use_id.as_deref(),
            Some("toolu_1"),
            "and the snapshot fills from a link that arrived first",
        );
    }

    /// The command is held for ANY card that carries one, not only a card
    /// whose input asked to run in the background: the CLI backgrounds a bash
    /// three ways and two of them cannot show on the card, so gating on the
    /// flag left those tasks with no command - and a view that needs the
    /// command draws no row for them at all.
    #[test]
    fn a_task_backgrounded_after_its_card_still_gets_its_command() {
        let (workspace, _rx) = crate::Workspace::testing_stub();
        let key = SessionSlot::from_str_for_test("w-bg");
        let (mut task, _update_rx) = review_task_for(&workspace, &key);
        let send = |task: &mut SessionTask, msg: forge_primitives::Message| {
            task.translate_event(AgentEvent::SdkMessage { session_id: "worker".to_owned(), msg });
        };

        // The card as a foreground call leaves it: no `run_in_background`.
        send(&mut task, a_bash_card("toolu_fg", "sleep 30 && echo later", None));
        send(&mut task, background_tasks(one_live_task()));
        send(&mut task, a_task_started("t1", "toolu_fg"));

        let held = task.domain.lock().background_tasks.clone();
        assert_eq!(
            held[0].command.as_deref(),
            Some("sleep 30 && echo later"),
            "the command the task's own card carried crosses even though the card did not ask \
             for the background: the registry and the link are what scope it",
        );
    }

    /// The other half of that rule: the command a card carried is read only
    /// through a task that names it. A rostered task whose link points at a
    /// different card keeps no command, and a card no task names resolves
    /// nothing.
    #[test]
    fn a_rostered_task_reads_only_the_command_its_own_link_names() {
        let (workspace, _rx) = crate::Workspace::testing_stub();
        let key = SessionSlot::from_str_for_test("w-bg");
        let (mut task, _update_rx) = review_task_for(&workspace, &key);
        let send = |task: &mut SessionTask, msg: forge_primitives::Message| {
            task.translate_event(AgentEvent::SdkMessage { session_id: "worker".to_owned(), msg });
        };

        send(&mut task, a_bash_card("toolu_other", "echo unrelated", None));
        send(&mut task, a_bash_card("toolu_ours", "gh run watch 9", None));
        send(&mut task, background_tasks(one_live_task()));
        send(&mut task, a_task_started("t1", "toolu_ours"));

        let held = task.domain.lock().background_tasks.clone();
        assert_eq!(held.len(), 1, "one rostered task");
        assert_eq!(
            held[0].command.as_deref(),
            Some("gh run watch 9"),
            "the command the task's own link names, not the other card's",
        );
    }

    /// A task whose card forge never saw keeps its row and has no command -
    /// the terminal's own rule, where a rostered task with no recorded card
    /// still draws from its roster entry.
    #[test]
    fn a_task_with_no_card_still_carries_its_registry_row() {
        let (workspace, _rx) = crate::Workspace::testing_stub();
        let key = SessionSlot::from_str_for_test("w-bg");
        let (mut task, _update_rx) = review_task_for(&workspace, &key);

        task.translate_event(AgentEvent::SdkMessage {
            session_id: "worker".to_owned(),
            msg: background_tasks(one_live_task()),
        });

        let held = task.domain.lock();
        let tasks = held.background_tasks.clone();
        assert_eq!(tasks.len(), 1, "the roster entry is still drawn: {tasks:?}");
        assert_eq!(tasks[0].command, None, "and its command is absent rather than invented");
    }

    fn a_backgrounded_bash(id: &str, command: &str) -> forge_primitives::Message {
        a_bash_card(id, command, Some(true))
    }

    /// A Bash card, with the `run_in_background` flag only when the caller
    /// says the card carried one.
    fn a_bash_card(
        id: &str,
        command: &str,
        run_in_background: Option<bool>,
    ) -> forge_primitives::Message {
        let mut input = serde_json::json!({ "command": command });
        if let Some(flag) = run_in_background {
            input["run_in_background"] = serde_json::Value::Bool(flag);
        }
        serde_json::from_value(serde_json::json!({
            "type": "assistant",
            "uuid": "a1",
            "session_id": "worker",
            "message": {
                "id": "m1",
                "role": "assistant",
                "model": "claude-opus-5",
                "content": [{
                    "type": "tool_use",
                    "id": id,
                    "name": "Bash",
                    "input": input,
                }],
            },
        }))
        .expect("parse an assistant message")
    }

    fn a_task_started(task_id: &str, tool_use_id: &str) -> forge_primitives::Message {
        serde_json::from_value(serde_json::json!({
            "type": "system",
            "subtype": "task_started",
            "task_id": task_id,
            "tool_use_id": tool_use_id,
            "description": "gh run watch",
            "task_type": "local_bash",
            "uuid": "u2",
            "session_id": "worker",
        }))
        .expect("parse a task_started message")
    }

    /// The session's own registry follows the CLI's snapshot, which
    /// carries the whole set every change - so an empty one clears rather
    /// than adding to what was there.
    #[test]
    fn background_work_follows_the_cli_snapshot() {
        let (workspace, _rx) = crate::Workspace::testing_stub();
        let key = SessionSlot::from_str_for_test("w-bg");
        let (mut task, _update_rx) = review_task_for(&workspace, &key);

        task.translate_event(AgentEvent::SdkMessage {
            session_id: "worker".to_owned(),
            msg: background_tasks(one_live_task()),
        });
        assert!(
            task.domain.lock().background_work,
            "a snapshot naming a live task is background work",
        );

        task.translate_event(AgentEvent::SdkMessage {
            session_id: "worker".to_owned(),
            msg: background_tasks(Vec::new()),
        });
        assert!(
            !task.domain.lock().background_work,
            "the whole set arrives each change, so an empty snapshot clears it",
        );
    }

    /// The CLI never sends a terminal `background_tasks_changed` for a
    /// session that died, so without this the registry stays true behind a
    /// subprocess that is gone and the row spins forever.
    #[test]
    fn a_connection_failure_drops_background_work() {
        let (workspace, _rx) = crate::Workspace::testing_stub();
        let key = SessionSlot::from_str_for_test("w-bg-fail");
        let (mut task, _update_rx) = review_task_for(&workspace, &key);
        task.translate_event(AgentEvent::SdkMessage {
            session_id: "worker".to_owned(),
            msg: background_tasks(one_live_task()),
        });
        assert!(task.domain.lock().background_work, "precondition: the snapshot armed it");

        task.translate_event(AgentEvent::ConnectionFailed {
            message: "the subprocess exited".to_owned(),
            kind: SpawnFailureKind::Unclassified,
        });

        assert!(
            !task.domain.lock().background_work,
            "a dead subprocess has no live background work, whatever the last snapshot said",
        );
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
                None,
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
                None,
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
        let prompts: Vec<&Command> = dispatched
            .iter()
            .filter(|c| matches!(c, Command::Prompt { .. } | Command::PromptUnder { .. }))
            .collect();
        assert_eq!(prompts.len(), 1, "inline-kick worker gets exactly one kick");
        if let Command::Prompt { key, text, .. } | Command::PromptUnder { key, text, .. } =
            prompts[0]
        {
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
        let prompts: Vec<&Command> = dispatched
            .iter()
            .filter(|c| matches!(c, Command::Prompt { .. } | Command::PromptUnder { .. }))
            .collect();
        assert_eq!(prompts.len(), 1, "an underscore-labelled worker gets its kick");
        if let Command::Prompt { text, .. } | Command::PromptUnder { text, .. } = prompts[0] {
            assert_eq!(text, "Begin: review the open diff.", "the kick arrives verbatim");
        }
    }

    /// A live worker whose entry carries no kick gets none - it idles
    /// until the lead sends an agents__send_message.
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
        let prompts: Vec<&Command> = dispatched
            .iter()
            .filter(|c| matches!(c, Command::Prompt { .. } | Command::PromptUnder { .. }))
            .collect();
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
            dispatched
                .iter()
                .all(|c| !matches!(c, Command::Prompt { .. } | Command::PromptUnder { .. })),
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
                c, Command::Prompt { text, .. } | Command::PromptUnder { text, .. }
                    if text == "go"
            )),
            "an underscore in the label must not cost the kick: {dispatched:?}",
        );
    }
}
