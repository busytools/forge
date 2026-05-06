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
use tokio::sync::mpsc;
use tracing::{Instrument as _, info_span};

use super::type_converters::{
    convert_current_model, convert_mode_state, map_available_models, map_permission_request,
    map_question_request, map_session_update,
};
use super::{ConnectionSlot, StartConnectionParams};

pub(super) async fn run_connection_task(
    params: StartConnectionParams,
    conn_slot_writer: Rc<std::cell::RefCell<Option<ConnectionSlot>>>,
) {
    let request_kind = if params.resume_id.is_some() { "resume" } else { "create" };
    let session_id = params.resume_id.clone().unwrap_or_default();
    let connection_span = info_span!(
        target: crate::logging::targets::BRIDGE_LIFECYCLE,
        "bridge_connection",
        request_kind,
        resume_requested = params.resume_requested,
        session_id = %session_id,
        cwd = %params.cwd_raw,
    );

    async move {
        tracing::debug!(
            target: crate::logging::targets::BRIDGE_LIFECYCLE,
            event_name = "bridge_connection_task_started",
            message = "bridge connection task started",
            outcome = "start",
            request_kind,
            resume_requested = params.resume_requested,
            session_id = %session_id,
        );

        let mut connected_once = false;
        let agent_handle = forge_agent::Agent::spawn();
        let Some(mut event_rx) = agent_handle.take_events() else {
            // `Agent::spawn()` seeds a fresh receiver, so this is only
            // reachable if a future refactor double-taps `take_events`.
            // Surface as a connection failure rather than panic.
            emit_connection_failed(
                &params.event_tx,
                "forge-agent yielded no event receiver".to_owned(),
                AppError::ConnectionFailed,
            );
            return;
        };

        let agent: Rc<forge_agent::AgentHandle> = Rc::new(agent_handle);
        *conn_slot_writer.borrow_mut() = Some(ConnectionSlot { conn: Rc::clone(&agent) });

        let send_result = if let Some(resume_id) = params.resume_id.clone() {
            agent.resume_session(resume_id, params.session_launch_settings.clone())
        } else {
            agent.new_session(params.cwd_raw.clone(), params.session_launch_settings.clone())
        };
        if let Err(err) = send_result {
            emit_connection_failed(
                &params.event_tx,
                format!("Failed to start forge-sdk session: {err}"),
                AppError::ConnectionFailed,
            );
            return;
        }

        forge_sdk_event_loop(&params, &mut event_rx, &agent, &mut connected_once).await;
    }
    .instrument(connection_span)
    .await;
}

/// Drain [`AgentEvent`]s emitted by the bridge and translate each
/// into the corresponding [`ClientEvent`]. Permission and question
/// requests are forwarded with a oneshot reply channel and a
/// dedicated forwarder task drains the reply.
async fn forge_sdk_event_loop(
    params: &StartConnectionParams,
    event_rx: &mut mpsc::UnboundedReceiver<AgentEvent>,
    agent: &Rc<forge_agent::AgentHandle>,
    connected_once: &mut bool,
) {
    while let Some(event) = event_rx.recv().await {
        handle_agent_event(&params.event_tx, agent, connected_once, params.resume_requested, event);
    }
    tracing::info!(
        target: crate::logging::targets::BRIDGE_LIFECYCLE,
        event_name = "forge_sdk_event_loop_exited",
        message = "forge-sdk worker channel closed; connection task exiting",
        outcome = "success",
    );
}

#[allow(clippy::too_many_lines)]
fn handle_agent_event(
    event_tx: &mpsc::UnboundedSender<ClientEvent>,
    agent: &Rc<forge_agent::AgentHandle>,
    connected_once: &mut bool,
    resume_requested: bool,
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
            if resume_requested
                && !*connected_once
                && message.to_ascii_lowercase().contains("unknown session")
            {
                let _ = event_tx.send(ClientEvent::FatalError(AppError::SessionNotFound));
                return;
            }
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
            let history_updates = history_updates
                .unwrap_or_default()
                .into_iter()
                .filter_map(map_session_update)
                .collect();
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
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_connected_event(
    event_tx: &mpsc::UnboundedSender<ClientEvent>,
    connected_once: &mut bool,
    session_id: String,
    cwd: String,
    current_model: types::CurrentModel,
    available_models: Vec<types::AvailableModel>,
    mode: Option<types::ModeState>,
    history_updates: Option<Vec<types::SessionUpdate>>,
) {
    let mode = mode.map(convert_mode_state);
    let history_updates =
        history_updates.unwrap_or_default().into_iter().filter_map(map_session_update).collect();
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
    agent: &Rc<forge_agent::AgentHandle>,
    session_id: String,
    request: types::PermissionRequest,
) {
    let (request, tool_call_id) = map_permission_request(&session_id, request);
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    if event_tx.send(ClientEvent::PermissionRequest { request, response_tx }).is_ok() {
        spawn_permission_response_forwarder(
            Rc::clone(agent),
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
    agent: &Rc<forge_agent::AgentHandle>,
    session_id: String,
    request: types::QuestionRequest,
) {
    let (request, tool_call_id) = map_question_request(&session_id, request);
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    if event_tx.send(ClientEvent::QuestionRequest { request, response_tx }).is_ok() {
        spawn_question_response_forwarder(Rc::clone(agent), response_rx, session_id, tool_call_id);
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
    agent: Rc<forge_agent::AgentHandle>,
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
    agent: Rc<forge_agent::AgentHandle>,
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
