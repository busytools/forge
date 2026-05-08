//! Connection task: spin up a [`ForgeSdkBridge`], wire its event
//! receiver into the App's [`ClientEvent`] channel, and own the
//! permission/question response forwarders. The `AgentEvent` →
//! `ClientEvent` translation lives here as private helpers.

use crate::agent::client::AgentEvent;
use crate::agent::events::ClientEvent;
use crate::agent::model;
use crate::error::AppError;
use forge_primitives as types;
use std::rc::Rc;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{Instrument as _, info_span};

use super::type_converters::{
    convert_current_model, convert_mode_state, map_available_models, map_permission_request,
    map_question_request,
};
use super::{ConnectionSlot, StartConnectionParams};

pub(super) async fn run_connection_task(
    params: StartConnectionParams,
    conn_slot_writer: Rc<std::cell::RefCell<Option<ConnectionSlot>>>,
) {
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

        // Destructure so `workspace` can drop the moment we're done with
        // it. The spawned event loop only needs `event_tx`, so dropping
        // `workspace` here lets `Rc::try_unwrap` succeed at clean exit.
        let StartConnectionParams { event_tx, workspace, session_launch_settings, project } =
            params;

        let target = match project {
            Some(name) => forge_workspace::SessionTarget::Named(name),
            None => forge_workspace::SessionTarget::Default,
        };

        let mut connected_once = false;
        let agent: Arc<forge_agent::AgentHandle> =
            match workspace.get_agent_handle(target, session_launch_settings).await {
                Ok(handle) => handle,
                Err(err) => {
                    emit_connection_failed(
                        &event_tx,
                        format!("workspace.get_agent_handle failed: {err}"),
                        AppError::ConnectionFailed,
                    );
                    return;
                }
            };
        drop(workspace);

        let Some(mut event_rx) = agent.take_events() else {
            // The Agent the workspace just spawned for us seeds a fresh
            // receiver, so this is only reachable if a future refactor
            // double-taps `take_events`. Surface as a connection failure
            // rather than panic.
            emit_connection_failed(
                &event_tx,
                "forge-workspace yielded no event receiver".to_owned(),
                AppError::ConnectionFailed,
            );
            return;
        };

        *conn_slot_writer.borrow_mut() = Some(ConnectionSlot { conn: Arc::clone(&agent) });

        forge_sdk_event_loop(&event_tx, &mut event_rx, &agent, &mut connected_once).await;
    }
    .instrument(connection_span)
    .await;
}

/// Drain [`AgentEvent`]s emitted by the bridge and translate each
/// into the corresponding [`ClientEvent`]. Permission and question
/// requests are forwarded with a oneshot reply channel and a
/// dedicated forwarder task drains the reply.
async fn forge_sdk_event_loop(
    event_tx: &mpsc::UnboundedSender<ClientEvent>,
    event_rx: &mut mpsc::UnboundedReceiver<AgentEvent>,
    agent: &Arc<forge_agent::AgentHandle>,
    connected_once: &mut bool,
) {
    while let Some(event) = event_rx.recv().await {
        handle_agent_event(event_tx, agent, connected_once, event);
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
    connected_once: &mut bool,
    event: AgentEvent,
) {
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
                event_tx,
                connected_once,
                session_id,
                cwd,
                current_model,
                available_models,
                mode,
                history_updates,
            );
        }
        AgentEvent::AuthRequired { method_name, method_description } => {
            let _ = event_tx.send(ClientEvent::AuthRequired { method_name, method_description });
        }
        AgentEvent::ConnectionFailed { message } => {
            emit_connection_failed(event_tx, message, AppError::ConnectionFailed);
        }
        AgentEvent::PermissionRequest { session_id, request } => {
            handle_permission_request_event(event_tx, agent, session_id, request);
        }
        AgentEvent::QuestionRequest { session_id, request } => {
            handle_question_request_event(event_tx, agent, session_id, request);
        }
        AgentEvent::ElicitationRequest { session_id, request } => {
            if event_tx.send(ClientEvent::McpElicitationRequest { request }).is_err() {
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
            let _ =
                event_tx.send(ClientEvent::McpElicitationCompleted { elicitation_id, server_name });
        }
        AgentEvent::McpAuthRedirect { redirect, .. } => {
            let _ = event_tx.send(ClientEvent::McpAuthRedirect { redirect });
        }
        AgentEvent::McpOperationError { error, .. } => {
            let _ = event_tx.send(ClientEvent::McpOperationError { error });
        }
        AgentEvent::SlashError { message, .. } => {
            let _ = event_tx.send(ClientEvent::SlashCommandError(message));
        }
        AgentEvent::RuntimeReloadCompleted { session_id } => {
            let _ = event_tx.send(ClientEvent::RuntimeReloadCompleted { session_id });
        }
        AgentEvent::RuntimeReloadFailed { session_id, message } => {
            let _ = event_tx.send(ClientEvent::RuntimeReloadFailed { session_id, message });
        }
        AgentEvent::SessionReplaced {
            session_id,
            cwd,
            current_model,
            available_models,
            mode,
            history_updates,
        } => {
            let history_updates = history_updates.unwrap_or_default();
            let _ = event_tx.send(ClientEvent::SessionReplaced {
                session_id: model::SessionId::new(session_id),
                cwd,
                current_model: convert_current_model(current_model),
                available_models: map_available_models(available_models),
                mode: mode.map(convert_mode_state),
                history_updates,
            });
        }
        AgentEvent::SessionsListed { sessions } => {
            let _ = event_tx.send(ClientEvent::SessionsListed { sessions });
        }
        AgentEvent::StatusSnapshot { session_id, account } => {
            let _ = event_tx.send(ClientEvent::StatusSnapshotReceived { session_id, account });
        }
        AgentEvent::OauthCredentialsSnapshot { session_id, credentials } => {
            let _ = event_tx
                .send(ClientEvent::OauthCredentialsSnapshotReceived { session_id, credentials });
        }
        AgentEvent::GitContextSnapshot { session_id, context } => {
            let _ = event_tx.send(ClientEvent::GitContextSnapshotReceived { session_id, context });
        }
        AgentEvent::ContextUsage { session_id, percentage } => {
            let _ = event_tx.send(ClientEvent::ContextUsageReceived { session_id, percentage });
        }
        AgentEvent::McpSnapshot { session_id, servers, error } => {
            let _ = event_tx.send(ClientEvent::McpSnapshotReceived { session_id, servers, error });
        }
        AgentEvent::SdkMessage { session_id, msg } => {
            let _ = event_tx.send(ClientEvent::SdkMessageReceived { session_id, msg });
        }
        AgentEvent::HookObservation {
            session_id,
            tool_use_id,
            permission_mode,
            effort,
            agent_id,
            agent_type,
        } => {
            let _ = event_tx.send(ClientEvent::HookObservation {
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
    event_tx: &mpsc::UnboundedSender<ClientEvent>,
    connected_once: &mut bool,
    session_id: String,
    cwd: String,
    current_model: types::CurrentModel,
    available_models: Vec<types::AvailableModel>,
    mode: Option<types::ModeState>,
    history_updates: Option<Vec<types::Message>>,
) {
    let mode = mode.map(convert_mode_state);
    let history_updates = history_updates.unwrap_or_default();
    if *connected_once {
        let _ = event_tx.send(ClientEvent::SessionReplaced {
            session_id: model::SessionId::new(session_id),
            cwd,
            current_model: convert_current_model(current_model),
            available_models: map_available_models(available_models),
            mode,
            history_updates,
        });
    } else {
        *connected_once = true;
        let _ = event_tx.send(ClientEvent::Connected {
            session_id: model::SessionId::new(session_id),
            cwd,
            current_model: convert_current_model(current_model),
            available_models: map_available_models(available_models),
            mode,
            history_updates,
        });
    }
}

fn handle_permission_request_event(
    event_tx: &mpsc::UnboundedSender<ClientEvent>,
    agent: &Arc<forge_agent::AgentHandle>,
    session_id: String,
    request: types::PermissionRequest,
) {
    let (request, tool_call_id) = map_permission_request(&session_id, request);
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    if event_tx.send(ClientEvent::PermissionRequest { request, response_tx }).is_ok() {
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
    event_tx: &mpsc::UnboundedSender<ClientEvent>,
    agent: &Arc<forge_agent::AgentHandle>,
    session_id: String,
    request: types::QuestionRequest,
) {
    let (request, tool_call_id) = map_question_request(&session_id, request);
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    if event_tx.send(ClientEvent::QuestionRequest { request, response_tx }).is_ok() {
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
    response_rx: tokio::sync::oneshot::Receiver<model::RequestPermissionResponse>,
    session_id: String,
    tool_call_id: String,
) {
    tokio::task::spawn_local(async move {
        let Ok(response) = response_rx.await else {
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
        let outcome = match response.outcome {
            model::RequestPermissionOutcome::Selected(selected) => {
                types::PermissionOutcome::Selected { option_id: selected.option_id.clone() }
            }
            model::RequestPermissionOutcome::Cancelled => types::PermissionOutcome::Cancelled,
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
    response_rx: tokio::sync::oneshot::Receiver<model::RequestQuestionResponse>,
    session_id: String,
    tool_call_id: String,
) {
    tokio::task::spawn_local(async move {
        let Ok(response) = response_rx.await else {
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
        let outcome = match response.outcome {
            model::RequestQuestionOutcome::Answered(answered) => types::QuestionOutcome::Answered {
                selected_option_ids: answered.selected_option_ids,
                annotation: answered.annotation.map(|annotation| types::QuestionAnnotation {
                    preview: annotation.preview,
                    notes: annotation.notes,
                }),
            },
            model::RequestQuestionOutcome::Cancelled => types::QuestionOutcome::Cancelled,
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

pub(super) fn emit_connection_failed(
    event_tx: &mpsc::UnboundedSender<ClientEvent>,
    message: String,
    app_error: AppError,
) {
    let _ = event_tx.send(ClientEvent::ConnectionFailed(message));
    let _ = event_tx.send(ClientEvent::FatalError(app_error));
}
