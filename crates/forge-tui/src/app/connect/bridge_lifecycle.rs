//! Connection task: spin up a [`ForgeSdkBridge`], wire its event
//! receiver into the App's [`ClientEvent`] channel, and own the
//! permission/question response forwarders. The `AgentEvent` →
//! `ClientEvent` translation lives here as private helpers.
//!
//! Each connection task is the per-session forwarder: it consumes
//! [`AgentEvent`]s emitted by one [`forge_agent::AgentHandle`] and
//! tags each translated [`ClientEvent`] with the [`SessionKey`] the
//! App's multiplexer uses to route to the right
//! [`crate::app::session::Session`] bucket.

use crate::agent::client::AgentEvent;
use crate::agent::events::ClientEvent;
use crate::app::App;
use crate::error::AppError;
use forge_primitives as types;
use forge_workspace::SessionKey;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{Instrument as _, info_span};

use super::StartConnectionParams;
use super::type_converters::{map_permission_request, map_question_request};

/// Resolve the [`SessionKey`] used to tag a translated
/// [`ClientEvent`]. AgentEvents that already carry a `session_id`
/// derive directly from it (this is the post-Connected steady
/// state). The few AgentEvents that lack a session id
/// (`AuthRequired`, `ConnectionFailed`, `SessionsListed`) fall back
/// to the pre-Connected synthetic key — the App's bucket map keeps
/// an entry under that key until the first `Connected` event
/// migrates the bucket onto the real session UUID.
fn session_key_for(event: &AgentEvent) -> SessionKey {
    match event.session_id() {
        Some(id) => SessionKey::from_session_id(id.to_owned()),
        None => SessionKey::from_session_id(App::PRE_CONNECT_KEY),
    }
}

pub(super) async fn run_connection_task(params: StartConnectionParams) {
    let connection_span = info_span!(
        target: crate::logging::targets::BRIDGE_LIFECYCLE,
        "bridge_connection",
        request_kind = "create",
    );

    async move {
        tracing::debug!(
            target: crate::logging::targets::BRIDGE_LIFECYCLE,
            event_name = "bridge_connection_task_started",
            message = "bridge connection task started",
            outcome = "start",
            request_kind = "create",
        );

        // Phase 1: workspace stays alive through the event loop so
        // bridge_lifecycle can store interaction oneshots via
        // `Workspace::store_pending_interaction` for each
        // permission/question/elicitation request. Phase 4 retires
        // this when SessionTask owns the AgentHandle event drain.
        let StartConnectionParams {
            event_tx,
            workspace,
            session_launch_settings,
            target,
            pre_connect_key,
            is_fatal_on_failure,
        } = params;

        let mut connected_once = false;
        // Workspace update sender cloned up-front so failure paths
        // below can dual-emit alongside the legacy ClientEvent stream.
        let pre_loop_update_tx = workspace.update_sender();
        let agent: Arc<forge_agent::AgentHandle> =
            match workspace.get_agent_handle(target, session_launch_settings).await {
                Ok(handle) => handle,
                Err(err) => {
                    emit_connection_failed(
                        &pre_loop_update_tx,
                        &pre_connect_key,
                        format!("workspace.get_agent_handle failed: {err}"),
                        AppError::ConnectionFailed,
                        is_fatal_on_failure,
                    );
                    return;
                }
            };

        // Forge-side display_name is known the instant the
        // workspace picks an account — much earlier than the CLI-
        // side status snapshot (which has to wait for the claude
        // subprocess to boot, init, and emit `system/init`). Emit
        // it now so the welcome message renders the right label
        // from the first frame after spawn.
        if let Some(display_name) = agent.display_name() {
            // Phase 3b: stamp the workspace-side DomainSession before
            // emitting the SessionUpdate so the projection stays in
            // sync. The `record_forge_account_identity_for_domain`
            // call is a no-op when no domain handle is registered for
            // `pre_connect_key` (still the synthetic pre-connect key
            // here — the real domain handle gets created on the first
            // Connected migration), which matches the existing
            // `record_event_for_domain` semantics for pre-connect
            // events.
            workspace
                .record_forge_account_identity_for_domain(&pre_connect_key, display_name.clone());
            // Phase 3b: ClientEvent emit removed. Workspace's
            // `SessionUpdate::ForgeAccountIdentity` drives the TUI
            // reducer via the `WorkspaceUpdate` dispatcher.
            let _ = pre_loop_update_tx.send(forge_workspace::SessionUpdate::ForgeAccountIdentity {
                key: pre_connect_key.clone(),
                display_name,
            });
        }

        let Some(mut event_rx) = agent.take_events() else {
            // The Agent the workspace just spawned for us seeds a fresh
            // receiver, so this is only reachable if a future refactor
            // double-taps `take_events`. Surface as a connection failure
            // rather than panic.
            emit_connection_failed(
                &pre_loop_update_tx,
                &pre_connect_key,
                "forge-workspace yielded no event receiver".to_owned(),
                AppError::ConnectionFailed,
                is_fatal_on_failure,
            );
            return;
        };

        forge_sdk_event_loop(
            &event_tx,
            &mut event_rx,
            &agent,
            &workspace,
            &mut connected_once,
            &pre_connect_key,
        )
        .await;
        // Drop workspace here so `Rc::try_unwrap` can succeed at
        // clean exit (no parallel SessionUpdate forwarder task to
        // keep it alive past the event loop).
        drop(workspace);
    }
    .instrument(connection_span)
    .await;
}

/// Drain [`AgentEvent`]s emitted by the bridge and translate each
/// into the corresponding [`ClientEvent`]. Permission and question
/// requests park a single oneshot on
/// `DomainSession.pending_interactions` via
/// [`forge_workspace::Workspace::store_pending_interaction`] and
/// emit a `tool_id` keyed envelope; the forwarder task drains the
/// reply when [`forge_workspace::SessionTask`] handles the
/// corresponding `Command::RespondPermission` /
/// `Command::RespondQuestion`.
async fn forge_sdk_event_loop(
    event_tx: &mpsc::UnboundedSender<ClientEvent>,
    event_rx: &mut mpsc::UnboundedReceiver<AgentEvent>,
    agent: &Arc<forge_agent::AgentHandle>,
    workspace: &std::rc::Rc<forge_workspace::Workspace>,
    connected_once: &mut bool,
    pre_connect_key: &SessionKey,
) {
    while let Some(event) = event_rx.recv().await {
        handle_agent_event(event_tx, agent, workspace, connected_once, pre_connect_key, event);
    }
    tracing::info!(
        target: crate::logging::targets::BRIDGE_LIFECYCLE,
        event_name = "forge_sdk_event_loop_exited",
        message = "forge-sdk worker channel closed; connection task exiting",
        outcome = "success",
    );
}

fn handle_agent_event(
    event_tx: &mpsc::UnboundedSender<ClientEvent>,
    agent: &Arc<forge_agent::AgentHandle>,
    workspace: &std::rc::Rc<forge_workspace::Workspace>,
    connected_once: &mut bool,
    pre_connect_key: &SessionKey,
    event: AgentEvent,
) {
    let session_key = session_key_for(&event);
    // Phase 2: update the workspace-side DomainSession before the
    // dual-emit. Synchronous, cheap (parking_lot::Mutex lock); no-op
    // for keys without a registered SessionTask (e.g. pre-connect
    // events keyed under the synthetic placeholder).
    workspace.record_event_for_domain(&session_key, &event);
    // Phase 1-3: dual-emit every translated agent event onto the
    // workspace channel as well as the legacy `ClientEvent` channel.
    // Phase 4 retires the ClientEvent path; `SessionTask` will own
    // the AgentHandle event drain and emit `SessionUpdate` directly.
    let update_tx = workspace.update_sender();
    match event {
        AgentEvent::Connected {
            session_id,
            cwd,
            current_model,
            available_models,
            mode,
            history_updates,
        } => {
            handle_connected_event(
                &update_tx,
                agent,
                connected_once,
                pre_connect_key,
                session_id,
                cwd,
                current_model,
                available_models,
                mode,
                history_updates,
            );
        }
        AgentEvent::AuthRequired { method_name, method_description } => {
            // Phase 3a: ClientEvent emit removed. Workspace's
            // `SessionUpdate::AuthRequired` drives the TUI reducer
            // via the `WorkspaceUpdate` dispatcher.
            let _ = update_tx.send(forge_workspace::SessionUpdate::AuthRequired {
                key: session_key,
                method_name,
                method_description,
            });
        }
        AgentEvent::ConnectionFailed { message } => {
            // After the first Connected event, an in-flight session
            // failure must not also kill the app even on the startup
            // path — the user has working state to preserve. Setting
            // `is_fatal_on_failure: false` here is correct: this code
            // path only runs once `forge_sdk_event_loop` is alive,
            // which means startup succeeded.
            emit_connection_failed(
                &update_tx,
                &session_key,
                message,
                AppError::ConnectionFailed,
                false,
            );
        }
        AgentEvent::PermissionRequest { session_id, request } => {
            handle_permission_request_event(&update_tx, agent, workspace, session_id, request);
        }
        AgentEvent::QuestionRequest { session_id, request } => {
            handle_question_request_event(&update_tx, agent, workspace, session_id, request);
        }
        AgentEvent::ElicitationRequest { session_id, request } => {
            let elicitation_id = request.elicitation_id.clone().unwrap_or_default();
            // Phase 3c: ClientEvent emit removed. The TUI's
            // `apply_session_update_mcp_elicitation_request` consumes
            // the SessionUpdate envelope below.
            if update_tx
                .send(forge_workspace::SessionUpdate::McpElicitationRequest {
                    key: session_key,
                    elicitation_id,
                    request,
                })
                .is_err()
            {
                tracing::error!(
                    target: crate::logging::targets::APP_PERMISSION,
                    event_name = "elicitation_request_dispatch_failed",
                    message = "failed to dispatch elicitation request to app event loop",
                    outcome = "failure",
                    session_id = %session_id,
                );
            }
        }
        AgentEvent::ElicitationComplete { elicitation_id, server_name, .. } => {
            let _ = event_tx.send(ClientEvent::McpElicitationCompleted {
                session_key: session_key.clone(),
                elicitation_id: elicitation_id.clone(),
                server_name: server_name.clone(),
            });
            let _ = update_tx.send(forge_workspace::SessionUpdate::McpElicitationCompleted {
                key: session_key,
                elicitation_id,
                server_name,
            });
        }
        AgentEvent::McpAuthRedirect { redirect, .. } => {
            let _ = event_tx.send(ClientEvent::McpAuthRedirect {
                session_key: session_key.clone(),
                redirect: redirect.clone(),
            });
            let _ = update_tx.send(forge_workspace::SessionUpdate::McpAuthRedirect {
                key: session_key,
                redirect,
            });
        }
        AgentEvent::McpOperationError { error, .. } => {
            let _ = event_tx.send(ClientEvent::McpOperationError {
                session_key: session_key.clone(),
                error: error.clone(),
            });
            let _ = update_tx.send(forge_workspace::SessionUpdate::McpOperationError {
                key: session_key,
                error,
            });
        }
        AgentEvent::SlashError { message, .. } => {
            // Phase 3a: ClientEvent emit removed. Workspace's
            // `SessionUpdate::SlashCommandError` drives the TUI
            // reducer via the `WorkspaceUpdate` dispatcher.
            let _ = update_tx.send(forge_workspace::SessionUpdate::SlashCommandError {
                key: session_key,
                message,
            });
        }
        AgentEvent::RuntimeReloadCompleted { session_id } => {
            // Phase 3b: ClientEvent emit removed. Workspace's
            // `SessionUpdate::RuntimeReloadCompleted` drives the TUI
            // reducer via the `WorkspaceUpdate` dispatcher.
            let _ = update_tx
                .send(forge_workspace::SessionUpdate::RuntimeReloadCompleted { session_id });
        }
        AgentEvent::RuntimeReloadFailed { session_id, message } => {
            // Phase 3b: ClientEvent emit removed. Workspace's
            // `SessionUpdate::RuntimeReloadFailed` drives the TUI
            // reducer via the `WorkspaceUpdate` dispatcher.
            let _ = update_tx
                .send(forge_workspace::SessionUpdate::RuntimeReloadFailed { session_id, message });
        }
        AgentEvent::SessionReplaced {
            session_id,
            cwd,
            current_model,
            available_models,
            mode,
            history_updates,
        } => {
            // Phase 3a: ClientEvent emit removed. Workspace's
            // `SessionUpdate::SessionReplaced` drives the TUI reducer
            // via the `WorkspaceUpdate` dispatcher.
            let history_updates = history_updates.unwrap_or_default();
            let _ = update_tx.send(forge_workspace::SessionUpdate::SessionReplaced {
                key: session_key,
                session_id: forge_primitives::SessionId::new(session_id),
                cwd,
                current_model,
                available_models,
                mode,
                history: history_updates,
                conn: Arc::clone(agent),
            });
        }
        AgentEvent::SessionsListed { sessions } => {
            // Phase 3a: ClientEvent emit removed; SessionUpdate path
            // drives `apply_session_update_sessions_listed`.
            let _ = update_tx.send(forge_workspace::SessionUpdate::SessionsListed { sessions });
        }
        AgentEvent::StatusSnapshot { session_id, account, forge_account } => {
            // Phase 3b: ClientEvent emit removed. Workspace's
            // `SessionUpdate::StatusSnapshot` drives the TUI reducer
            // via the `WorkspaceUpdate` dispatcher.
            let _ = update_tx.send(forge_workspace::SessionUpdate::StatusSnapshot {
                session_id,
                account,
                forge_account,
            });
        }
        AgentEvent::OauthCredentialsSnapshot { session_id, credentials } => {
            // Phase 3b: ClientEvent emit removed. Workspace's
            // `SessionUpdate::OauthCredentialsSnapshot` drives the
            // TUI reducer via the `WorkspaceUpdate` dispatcher.
            let _ = update_tx.send(forge_workspace::SessionUpdate::OauthCredentialsSnapshot {
                session_id,
                credentials,
            });
        }
        AgentEvent::GitContextSnapshot { session_id, context } => {
            // Phase 3b: ClientEvent emit removed. Workspace's
            // `SessionUpdate::GitContextSnapshot` drives the TUI
            // reducer via the `WorkspaceUpdate` dispatcher.
            let _ = update_tx
                .send(forge_workspace::SessionUpdate::GitContextSnapshot { session_id, context });
        }
        AgentEvent::ContextUsage { session_id, percentage } => {
            // Phase 3b: ClientEvent emit removed. Workspace's
            // `SessionUpdate::ContextUsageSnapshot` drives the TUI
            // reducer via the `WorkspaceUpdate` dispatcher.
            let _ = update_tx.send(forge_workspace::SessionUpdate::ContextUsageSnapshot {
                session_id,
                percentage,
            });
        }
        AgentEvent::McpSnapshot { session_id, servers, error } => {
            // Phase 3b: ClientEvent emit removed. Workspace's
            // `SessionUpdate::McpSnapshot` drives the TUI reducer
            // via the `WorkspaceUpdate` dispatcher.
            let _ = update_tx.send(forge_workspace::SessionUpdate::McpSnapshot {
                session_id,
                servers,
                error,
            });
        }
        AgentEvent::SdkMessage { session_id, msg } => {
            // Phase 3b: ClientEvent emit removed. Workspace's
            // `SessionUpdate::ChatAppended` drives the TUI reducer
            // via the `WorkspaceUpdate` dispatcher. The active-bucket
            // temp-swap inside the reducer is retained for Phase 4.
            let _ =
                update_tx.send(forge_workspace::SessionUpdate::ChatAppended { session_id, msg });
        }
        AgentEvent::HookObservation {
            session_id,
            tool_use_id,
            permission_mode,
            effort,
            agent_id,
            agent_type,
        } => {
            // Phase 3b: ClientEvent emit removed. Workspace's
            // `SessionUpdate::HookObservation` drives the TUI reducer
            // via the `WorkspaceUpdate` dispatcher.
            let _ = update_tx.send(forge_workspace::SessionUpdate::HookObservation {
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

// Connected-event handler — destructured fields from the `AgentEvent::Connected` variant. Packing into a struct just to forward to the App is busywork.
#[allow(clippy::too_many_arguments)]
fn handle_connected_event(
    update_tx: &mpsc::UnboundedSender<forge_workspace::SessionUpdate>,
    agent: &Arc<forge_agent::AgentHandle>,
    connected_once: &mut bool,
    pre_connect_key: &SessionKey,
    session_id: String,
    cwd: String,
    current_model: types::CurrentModel,
    available_models: Vec<types::AvailableModel>,
    mode: Option<types::ModeState>,
    history_updates: Option<Vec<types::Message>>,
) {
    // Phase 3a: ClientEvent emit removed for both first-Connected
    // and replacement-Connected paths. Workspace's
    // `SessionUpdate::Connected` / `SessionUpdate::SessionReplaced`
    // drive the TUI reducers via the `WorkspaceUpdate` dispatcher.
    //
    // On the first-Connected path we also emit
    // `SessionUpdate::KeyRenamed` so the TUI dispatcher can migrate
    // the synthetic spawn bucket (`__spawn_<project>__` /
    // `__conn_pending__`) onto the real claude session UUID BEFORE
    // the matching `SessionUpdate::Connected` reducer runs. Without
    // the explicit rename, rapid clicks across sleeping projects
    // could let one Connected's reducer race the active-key
    // fallback and pick up the wrong synthetic bucket.
    let history_updates = history_updates.unwrap_or_default();
    let session_key = SessionKey::from_session_id(session_id.clone());
    if *connected_once {
        let _ = update_tx.send(forge_workspace::SessionUpdate::SessionReplaced {
            key: session_key,
            session_id: forge_primitives::SessionId::new(session_id),
            cwd,
            current_model,
            available_models,
            mode,
            history: history_updates,
            conn: Arc::clone(agent),
        });
    } else {
        *connected_once = true;
        // First-Connected: emit `KeyRenamed` from the synthetic
        // bucket to the real session key so the TUI side sees the
        // bucket under `session_key` by the time `Connected` is
        // dispatched. No-op for the TUI dispatcher when the
        // synthetic bucket doesn't exist (test paths, race-resolved
        // cases) — that's fine.
        if pre_connect_key.as_str() != session_key.as_str() {
            let _ = update_tx.send(forge_workspace::SessionUpdate::KeyRenamed {
                from: pre_connect_key.clone(),
                to: session_key.clone(),
            });
        }
        let _ = update_tx.send(forge_workspace::SessionUpdate::Connected {
            key: session_key,
            session_id: forge_primitives::SessionId::new(session_id),
            cwd,
            current_model,
            available_models,
            mode,
            history: history_updates,
            conn: Arc::clone(agent),
        });
    }
}

fn handle_permission_request_event(
    update_tx: &mpsc::UnboundedSender<forge_workspace::SessionUpdate>,
    agent: &Arc<forge_agent::AgentHandle>,
    workspace: &std::rc::Rc<forge_workspace::Workspace>,
    session_id: String,
    request: types::PermissionRequest,
) {
    let wire_request = request.clone();
    let (_legacy_request, tool_call_id) = map_permission_request(&session_id, request);
    let session_key = SessionKey::from_session_id(session_id.clone());
    // Single workspace-owned oneshot. The bridge keeps the receiver
    // and forwards the outcome to the agent's pending response when
    // `SessionTask` handles `Command::RespondPermission`.
    let (response_tx, response_rx) = tokio::sync::oneshot::channel::<types::PermissionOutcome>();
    workspace.store_pending_interaction(
        &session_key,
        tool_call_id.clone(),
        forge_workspace::PendingInteractionSlot::Permission(response_tx),
    );
    // Phase 3c: ClientEvent emit removed. The TUI's
    // `apply_session_update_permission_request` consumes the
    // SessionUpdate envelope and re-runs the
    // `map_permission_request` conversion against the wire-side
    // payload below.
    if update_tx
        .send(forge_workspace::SessionUpdate::PermissionRequest {
            key: session_key,
            tool_id: tool_call_id.clone(),
            request: wire_request,
        })
        .is_ok()
    {
        spawn_permission_response_forwarder(
            Arc::clone(agent),
            response_rx,
            session_id,
            tool_call_id,
        );
    } else {
        tracing::error!(
            target: crate::logging::targets::APP_PERMISSION,
            event_name = "permission_request_dispatch_failed",
            message = "failed to dispatch permission request to app event loop",
            outcome = "failure",
            session_id = %session_id,
            tool_call_id = %tool_call_id,
        );
    }
}

fn handle_question_request_event(
    update_tx: &mpsc::UnboundedSender<forge_workspace::SessionUpdate>,
    agent: &Arc<forge_agent::AgentHandle>,
    workspace: &std::rc::Rc<forge_workspace::Workspace>,
    session_id: String,
    request: types::QuestionRequest,
) {
    let wire_request = request.clone();
    let (_legacy_request, tool_call_id) = map_question_request(&session_id, request);
    let session_key = SessionKey::from_session_id(session_id.clone());
    let (response_tx, response_rx) = tokio::sync::oneshot::channel::<types::QuestionOutcome>();
    workspace.store_pending_interaction(
        &session_key,
        tool_call_id.clone(),
        forge_workspace::PendingInteractionSlot::Question(response_tx),
    );
    // Phase 3c: ClientEvent emit removed. See `handle_permission_request_event` above.
    if update_tx
        .send(forge_workspace::SessionUpdate::QuestionRequest {
            key: session_key,
            tool_id: tool_call_id.clone(),
            request: wire_request,
        })
        .is_ok()
    {
        spawn_question_response_forwarder(Arc::clone(agent), response_rx, session_id, tool_call_id);
    } else {
        tracing::error!(
            target: crate::logging::targets::APP_PERMISSION,
            event_name = "question_request_dispatch_failed",
            message = "failed to dispatch question request to app event loop",
            outcome = "failure",
            session_id = %session_id,
            tool_call_id = %tool_call_id,
        );
    }
}

fn spawn_permission_response_forwarder(
    agent: Arc<forge_agent::AgentHandle>,
    response_rx: tokio::sync::oneshot::Receiver<types::PermissionOutcome>,
    session_id: String,
    tool_call_id: String,
) {
    tokio::task::spawn_local(async move {
        let Ok(outcome) = response_rx.await else {
            tracing::warn!(
                target: crate::logging::targets::APP_PERMISSION,
                event_name = "permission_response_abandoned",
                message = "permission response channel closed before bridge forwarding",
                outcome = "dropped",
                session_id = %session_id,
                tool_call_id = %tool_call_id,
            );
            return;
        };
        let selected_option = match &outcome {
            types::PermissionOutcome::Selected { option_id } => option_id.clone(),
            types::PermissionOutcome::Cancelled => "cancelled".to_owned(),
        };
        let session_id_for_log = session_id.clone();
        let tool_call_id_for_log = tool_call_id.clone();
        match agent.permission_response(session_id, tool_call_id, outcome) {
            Ok(()) => {
                tracing::info!(
                    target: crate::logging::targets::APP_PERMISSION,
                    event_name = "permission_response_forwarded",
                    message = "permission response forwarded to bridge",
                    outcome = "success",
                    session_id = %session_id_for_log,
                    tool_call_id = %tool_call_id_for_log,
                    selected_option = %selected_option,
                );
            }
            Err(err) => {
                tracing::error!(
                    target: crate::logging::targets::APP_PERMISSION,
                    event_name = "permission_response_forward_failed",
                    message = "failed to forward permission response to bridge",
                    outcome = "failure",
                    session_id = %session_id_for_log,
                    tool_call_id = %tool_call_id_for_log,
                    selected_option = %selected_option,
                    error = %err,
                );
            }
        }
    });
}

fn spawn_question_response_forwarder(
    agent: Arc<forge_agent::AgentHandle>,
    response_rx: tokio::sync::oneshot::Receiver<types::QuestionOutcome>,
    session_id: String,
    tool_call_id: String,
) {
    tokio::task::spawn_local(async move {
        let Ok(outcome) = response_rx.await else {
            tracing::warn!(
                target: crate::logging::targets::APP_PERMISSION,
                event_name = "question_response_abandoned",
                message = "question response channel closed before bridge forwarding",
                outcome = "dropped",
                session_id = %session_id,
                tool_call_id = %tool_call_id,
            );
            return;
        };
        let selected_option_count = match &outcome {
            types::QuestionOutcome::Answered { selected_option_ids, .. } => {
                selected_option_ids.len()
            }
            types::QuestionOutcome::Cancelled => 0,
        };
        let session_id_for_log = session_id.clone();
        let tool_call_id_for_log = tool_call_id.clone();
        match agent.question_response(session_id, tool_call_id, outcome) {
            Ok(()) => {
                tracing::info!(
                    target: crate::logging::targets::APP_PERMISSION,
                    event_name = "question_response_forwarded",
                    message = "question response forwarded to bridge",
                    outcome = "success",
                    session_id = %session_id_for_log,
                    tool_call_id = %tool_call_id_for_log,
                    selected_option_count,
                );
            }
            Err(err) => {
                tracing::error!(
                    target: crate::logging::targets::APP_PERMISSION,
                    event_name = "question_response_forward_failed",
                    message = "failed to forward question response to bridge",
                    outcome = "failure",
                    session_id = %session_id_for_log,
                    tool_call_id = %tool_call_id_for_log,
                    selected_option_count,
                    error = %err,
                );
            }
        }
    });
}

/// Emit a connection failure for the bucket addressed by `session_key`.
///
/// `is_fatal` controls whether forge-tui should also exit. The
/// startup connection task sets this `true` — if the very first
/// bridge fails before any session exists, there's nothing to
/// render and the app should terminate. The sleeping-project spawn
/// flow sets this `false` — the user has an active session whose
/// state must survive a fresh spawn's failure; the failure surfaces
/// inline in the spawn bucket.
pub(super) fn emit_connection_failed(
    update_tx: &mpsc::UnboundedSender<forge_workspace::SessionUpdate>,
    session_key: &SessionKey,
    message: String,
    app_error: AppError,
    is_fatal: bool,
) {
    // Phase 3a: ClientEvent emits removed for ConnectionFailed +
    // FatalError. Workspace's matching SessionUpdate variants drive
    // the TUI reducers via the `WorkspaceUpdate` dispatcher.
    let _ = update_tx.send(forge_workspace::SessionUpdate::ConnectionFailed {
        key: session_key.clone(),
        message,
        fatal: is_fatal,
    });
    if is_fatal {
        let _ = update_tx.send(forge_workspace::SessionUpdate::FatalError(app_error));
    }
}
