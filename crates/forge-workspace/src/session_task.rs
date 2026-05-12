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
            other => {
                let sid = self.session_id_string();
                let _ = execute_command_via_handle(&self.handle, &self.key, sid.as_deref(), other);
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
) -> anyhow::Result<()> {
    match cmd {
        Command::RespondElicitation { key: _, elicitation_id, action, content } => {
            // MCP elicitation responses bypass `pending_interactions`
            // entirely — the bridge emits `AgentEvent::ElicitationRequest`
            // without registering a workspace-side oneshot, and the TUI
            // replies out of band with optional `content`. Forward
            // straight to the agent.
            let Some(sid) = session_id else {
                warn_no_session(key, "RespondElicitation");
                return Ok(());
            };
            handle.respond_to_elicitation(sid.to_owned(), elicitation_id, action, content)
        }
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
        Command::ResumeOrNewSession { key: _, session_id, cwd, launch_settings } => {
            handle.resume_or_new_session(session_id, cwd, launch_settings)
        }
        Command::GenerateSessionTitle { key: _, description } => {
            let Some(sid) = session_id else {
                warn_no_session(key, "GenerateSessionTitle");
                return Ok(());
            };
            handle.generate_session_title(sid.to_owned(), description)
        }
        Command::RenameSession { key: _, title } => {
            let Some(sid) = session_id else {
                warn_no_session(key, "RenameSession");
                return Ok(());
            };
            handle.rename_session(sid.to_owned(), title)
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
        Command::SetMcpServers { key: _, servers } => {
            let Some(sid) = session_id else {
                warn_no_session(key, "SetMcpServers");
                return Ok(());
            };
            handle.set_mcp_servers(sid.to_owned(), servers)
        }
        Command::AuthenticateMcpServer { key: _, server_name } => {
            let Some(sid) = session_id else {
                warn_no_session(key, "AuthenticateMcpServer");
                return Ok(());
            };
            handle.authenticate_mcp_server(sid.to_owned(), server_name)
        }
        Command::ClearMcpAuth { key: _, server_name } => {
            let Some(sid) = session_id else {
                warn_no_session(key, "ClearMcpAuth");
                return Ok(());
            };
            handle.clear_mcp_auth(sid.to_owned(), server_name)
        }
        Command::SubmitMcpOauthCallbackUrl { key: _, server_name, callback_url } => {
            let Some(sid) = session_id else {
                warn_no_session(key, "SubmitMcpOauthCallbackUrl");
                return Ok(());
            };
            handle.submit_mcp_oauth_callback_url(sid.to_owned(), server_name, callback_url)
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
        Command::StartGitWatch { key: _, session_id, cwd } => {
            handle.start_git_context_watch(session_id, cwd)
        }
        Command::StopGitWatch { key: _, session_id } => handle.stop_git_context_watch(session_id),
        // The remaining variants (CloseSession, the four Request*
        // refresh commands, RuntimeReload, UpdatePermissions)
        // aren't wired through `dispatch` yet — they predate the
        // Phase 6 strict-wiring pass and remain stubs. Refresh
        // commands flow via `Workspace::refresh_*` direct methods
        // instead.
        stub @ (Command::CloseSession { .. }
        | Command::RequestStatusSnapshot { .. }
        | Command::RequestMcpSnapshot { .. }
        | Command::RequestContextUsage { .. }
        | Command::RequestOauthCredentials { .. }
        | Command::RuntimeReload { .. }
        | Command::UpdatePermissions { .. }) => {
            tracing::trace!(
                target: "forge_workspace::session_task",
                key = %key.as_str(),
                command = ?stub,
                "command received but not yet wired to AgentHandle"
            );
            Ok(())
        }
        // App-level commands are caught in Workspace::dispatch's
        // app-level branch; they never reach this helper.
        misrouted @ (Command::SpawnProject { .. }
        | Command::SpawnSession { .. }
        | Command::StartDefault { .. }) => {
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
    match event {
        AgentEvent::Connected { session_id, .. } if domain.session_id.is_none() => {
            domain.session_id = Some(SessionId::new(session_id.clone()));
        }
        AgentEvent::SessionReplaced { session_id, .. } => {
            domain.session_id = Some(SessionId::new(session_id.clone()));
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

    fn empty_domain() -> DomainSession {
        let (handle, _rx) = Agent::testing_stub();
        DomainSession::new(SessionKey::from_str_for_test("test"), Some(Arc::new(handle)))
    }

    /// `apply_event_to_domain` on `AgentEvent::Connected` stamps
    /// `session_id` so the workspace can route subsequent
    /// `AgentHandle` calls. The first-Connect path leaves any
    /// existing session_id alone — the bridge can re-fire this on
    /// snapshot rebuilds.
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

    /// `SessionReplaced` (e.g., `/new`, login, logout) overwrites the
    /// session_id mirror; the new session_id is what subsequent
    /// `AgentHandle` calls dispatch with.
    #[test]
    fn translate_session_replaced_replaces_session_id() {
        let mut domain = empty_domain();
        domain.session_id = Some(SessionId::new("old-uuid"));

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
    }

    /// Common harness for the `execute_command_via_handle` tests
    /// below: build a fresh stub handle + drain channel, return both.
    fn stub_handle_with_rx()
    -> (Arc<AgentHandle>, tokio::sync::mpsc::UnboundedReceiver<forge_primitives::Command>) {
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
            forge_primitives::Command::PromptWithImages { session_id, .. }
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
            forge_primitives::Command::Cancel { session_id } if session_id == "sess-1"
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
            forge_primitives::Command::SetMode { session_id, mode } => {
                assert_eq!(session_id.as_str(), "sess-1");
                assert_eq!(mode, "plan");
            }
            other => panic!("expected SetMode, got {other:?}"),
        }
    }

    /// `Command::GenerateSessionTitle` forwards through the new path.
    #[test]
    fn execute_generate_session_title_forwards_to_handle() {
        let (handle, mut rx) = stub_handle_with_rx();
        let key = SessionKey::from_str_for_test("sess");
        execute_command_via_handle(
            &handle,
            &key,
            Some("sess-1"),
            Command::GenerateSessionTitle { key: key.clone(), description: "first turn".into() },
        )
        .expect("dispatch succeeds");
        let cmd = rx.try_recv().expect("command queued");
        match cmd {
            forge_primitives::Command::GenerateSessionTitle { session_id, description } => {
                assert_eq!(session_id.as_str(), "sess-1");
                assert_eq!(description, "first turn");
            }
            other => panic!("expected GenerateSessionTitle, got {other:?}"),
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
            forge_primitives::Command::ReconnectMcpServer { server_name, .. } => {
                assert_eq!(server_name, "fs");
            }
            other => panic!("expected ReconnectMcpServer, got {other:?}"),
        }
    }

    /// `Command::SubmitMcpOauthCallbackUrl` carries the captured URL
    /// through.
    #[test]
    fn execute_submit_mcp_oauth_callback_url_forwards() {
        let (handle, mut rx) = stub_handle_with_rx();
        let key = SessionKey::from_str_for_test("sess");
        execute_command_via_handle(
            &handle,
            &key,
            Some("sess-1"),
            Command::SubmitMcpOauthCallbackUrl {
                key: key.clone(),
                server_name: "github".into(),
                callback_url: "https://example.com/cb?code=abc".into(),
            },
        )
        .expect("dispatch succeeds");
        let cmd = rx.try_recv().expect("command queued");
        match cmd {
            forge_primitives::Command::SubmitMcpOauthCallbackUrl { callback_url, .. } => {
                assert_eq!(callback_url, "https://example.com/cb?code=abc");
            }
            other => panic!("expected SubmitMcpOauthCallbackUrl, got {other:?}"),
        }
    }

    /// `Command::RespondElicitation` forwards directly (no
    /// `pending_interactions` lookup needed — MCP elicitations
    /// bypass the workspace's oneshot path).
    #[test]
    fn execute_respond_elicitation_forwards_with_content() {
        let (handle, mut rx) = stub_handle_with_rx();
        let key = SessionKey::from_str_for_test("sess");
        execute_command_via_handle(
            &handle,
            &key,
            Some("sess-1"),
            Command::RespondElicitation {
                key: key.clone(),
                elicitation_id: "elic-1".into(),
                action: forge_primitives::ElicitationAction::Accept,
                content: Some(serde_json::json!({"answer": "yes"})),
            },
        )
        .expect("dispatch succeeds");
        let cmd = rx.try_recv().expect("command queued");
        assert!(matches!(
            cmd,
            forge_primitives::Command::RespondToElicitation { content: Some(_), .. }
        ));
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
