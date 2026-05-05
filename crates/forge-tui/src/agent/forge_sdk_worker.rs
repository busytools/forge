//! Session-launcher + reader-pump helpers for [`super::forge_sdk_bridge`].
//!
//! Plays the role of the upstream Node `bridge.ts`'s spawn dance and
//! reader subtask: build `forge_sdk::Options` from the TUI's launch
//! settings (with the `can_use_tool` callback wired in), spawn the
//! `Client`, emit a synthetic `Connected` event, and pump
//! `Client::next_event()` into [`AgentEvent::SdkMessage`]. The bridge
//! owns the resulting `Client`; this module exposes the helpers it
//! calls.

use std::path::PathBuf;
use std::sync::Arc;

use forge_sdk::{
    Client, Options, OptionsBuilder, PermissionDecision, PermissionMode, ToolPermissionContext,
};
use tokio::sync::{mpsc, oneshot};

use crate::agent::client::AgentEvent;
use crate::agent::forge_sdk_bridge::{ForgeSdkBridge, PendingQuestions, PendingResponses};
use crate::agent::{
    commands as bridge_commands, session_lifecycle, state as bridge_state,
    user_interaction as bridge_user_interaction,
};

/// Spawn a fresh `Client` for `bridge` and start the reader subtask.
/// Builds `Options` from `launch_settings` (mode, model, effort,
/// `can_use_tool` callback). When `resume_id` is `Some`, passes the
/// `--resume` flag and backfills past turns into the `Connected`
/// event's `history_updates`.
///
/// On success the bridge's client slot is populated and a
/// [`AgentEvent::Connected`] is emitted. On failure the caller is
/// expected to surface a [`AgentEvent::ConnectionFailed`] from the
/// returned error.
pub(crate) async fn spawn_session(
    bridge: &ForgeSdkBridge,
    cwd: &str,
    resume_id: Option<&str>,
    launch_settings: &crate::agent::client::SessionLaunchSettings,
) -> anyhow::Result<()> {
    // If we already have a client, drop it so the existing subprocess
    // shuts down cleanly before the replacement spawns.
    if let Some(prev) = bridge.clear_client() {
        let _ = prev.disconnect().await;
    }

    let options = build_options_with_callback(
        cwd,
        resume_id,
        launch_settings,
        bridge.event_tx().clone(),
        Arc::clone(bridge.inner_pending()),
        Arc::clone(bridge.inner_pending_questions()),
        Arc::clone(bridge.session_id_slot_arc()),
    );
    let (client, events) = Client::spawn(options).await?;
    // For resume sessions the CLI flag carried the real session id —
    // prefer that over `Client::session_id()`, which is empty until
    // `system/init` lands on the wire (per `spawn_inner` docs, after
    // both the initialize control_response AND a user message). For
    // new sessions we also fall back to whatever Client captured
    // during its init loop (typically empty), and the App-side handler
    // adopts the first non-empty session id seen on the wire.
    let session_id = match resume_id {
        Some(id) if !id.is_empty() => id.to_owned(),
        _ => client.session_id(),
    };
    if let Ok(mut slot) = bridge.session_id_slot_arc().lock() {
        slot.clone_from(&session_id);
    }

    let cwd_owned = std::env::current_dir()
        .ok()
        .and_then(|p| p.into_os_string().into_string().ok())
        .unwrap_or_default();

    // Emit Connected BEFORE spawning the reader subtask so the App
    // sees Connected first on its mpsc — otherwise the reader can
    // race and push an SdkMessage before Connected, leaving
    // `app.session_id` = None when the SdkMessage arrives.
    emit_connected(bridge.event_tx(), &client, &session_id, &cwd_owned, launch_settings, resume_id);

    // Reader subtask — owns the events receiver. Client is the writer-side
    // handle (Arc-backed, Clone) and stays on the bridge.
    let reader_event_tx = bridge.event_tx().clone();
    let reader_session_id = session_id.clone();
    tokio::spawn(reader_loop(events, reader_event_tx, reader_session_id));

    bridge.set_client(client);
    Ok(())
}

/// Build the typed `Connected` envelope from the SDK's cached init data
/// + the initialize `control_response`, and emit it onto `event_tx`.
fn emit_connected(
    event_tx: &mpsc::UnboundedSender<AgentEvent>,
    client: &Client,
    session_id: &str,
    cwd: &str,
    launch_settings: &crate::agent::client::SessionLaunchSettings,
    resume_id: Option<&str>,
) {
    let server_info = client.get_server_info().cloned();
    let init_data = client.initial_session_data().cloned();

    let available_models =
        session_lifecycle::map_available_models(server_info.as_ref().and_then(|v| v.get("models")));
    let init_record = init_data.as_ref().and_then(serde_json::Value::as_object);

    // The CLI doesn't emit `system/init` until both the initialize
    // control_response AND a user message have landed. Fall back to
    // launch_settings (settings.json) for the initial Connected.
    let launch_settings_record =
        launch_settings.settings.as_ref().and_then(serde_json::Value::as_object);
    let init_model_id = init_record
        .and_then(|r| r.get("model"))
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            launch_settings_record.and_then(|r| r.get("model")).and_then(serde_json::Value::as_str)
        })
        .unwrap_or("")
        .to_owned();
    let raw_permission_mode = init_record
        .and_then(|r| r.get("permissionMode"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            launch_settings_record
                .and_then(|r| r.get("permissions"))
                .and_then(serde_json::Value::as_object)
                .and_then(|p| p.get("defaultMode"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        });
    let init_permission_mode = raw_permission_mode
        .as_deref()
        .and_then(bridge_state::PermissionMode::from_wire)
        .or(Some(bridge_state::PermissionMode::Default));
    let supports_bypass = init_record
        .and_then(|r| r.get("supportsBypassPermissionsMode"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    let current_model = session_lifecycle::resolve_current_model_from_inputs(
        &init_model_id,
        None,
        None,
        &available_models,
    );
    let mode = init_permission_mode.map(|m| {
        let supports_auto_mode = current_model.supports_auto_mode == Some(true);
        let supported = bridge_commands::supported_mode_ids_filtered(
            supports_auto_mode,
            supports_bypass,
            Some(m),
            &[],
        );
        bridge_commands::build_mode_state_from_supported(m, &supported)
    });

    let history_updates = resume_id.and_then(|prev_session_id| {
        let updates = load_history_updates(prev_session_id, cwd);
        if updates.is_empty() { None } else { Some(updates) }
    });

    let _ = event_tx.send(AgentEvent::Connected {
        session_id: session_id.to_owned(),
        cwd: cwd.to_owned(),
        current_model,
        available_models,
        mode,
        history_updates,
    });

    if let Some(account) = client.account_info() {
        let _ = event_tx
            .send(AgentEvent::StatusSnapshot { session_id: session_id.to_owned(), account });
    }

    let _ = event_tx.send(AgentEvent::SessionsListed { sessions: list_recent_sessions(cwd) });
}

fn load_history_updates(
    prev_session_id: &str,
    cwd: &str,
) -> Vec<crate::agent::types::SessionUpdate> {
    let dir = if cwd.is_empty() { None } else { Some(cwd.to_owned()) };
    let messages = forge_sdk::session::scan::get_session_messages(prev_session_id, dir);
    let raw: Vec<serde_json::Value> = messages
        .into_iter()
        .map(|m| {
            let kind = match m.kind {
                forge_sdk::SessionMessageKind::User => "user",
                forge_sdk::SessionMessageKind::Assistant => "assistant",
            };
            serde_json::json!({
                "type": kind,
                "message": m.message,
                "parent_tool_use_id": m.parent_tool_use_id,
            })
        })
        .collect();
    crate::agent::history::map_session_messages_to_updates(&raw)
}

fn list_recent_sessions(cwd: &str) -> Vec<crate::agent::types::SessionListEntry> {
    use crate::agent::types::SessionListEntry;
    const MAX_RECENT: usize = 50;
    let dir = if cwd.is_empty() { None } else { Some(cwd.to_owned()) };
    forge_sdk::session::scan::list_sessions(dir, Some(MAX_RECENT), 0)
        .into_iter()
        .map(|info| SessionListEntry {
            session_id: info.session_id,
            summary: info.summary,
            last_modified_ms: info.last_modified,
            file_size_bytes: info.file_size.unwrap_or(0),
            cwd: info.cwd,
            git_branch: info.git_branch,
            custom_title: info.custom_title,
            first_prompt: info.first_prompt,
        })
        .collect()
}

fn msg_session_id(msg: &forge_sdk::Message) -> Option<String> {
    use forge_sdk::Message;
    match msg {
        Message::Assistant { session_id, .. }
        | Message::User { session_id, .. }
        | Message::Result { session_id, .. } => Some(session_id.clone()),
        Message::System { session_id, .. } => session_id.clone(),
        _ => None,
    }
}

async fn reader_loop(
    mut events: forge_sdk::ClientEvents,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    session_id: String,
) {
    while let Some(item) = events.recv().await {
        match item {
            Ok(msg) => {
                let session_id_for_sdk_msg =
                    msg_session_id(&msg).unwrap_or_else(|| session_id.clone());
                if event_tx
                    .send(AgentEvent::SdkMessage { session_id: session_id_for_sdk_msg, msg })
                    .is_err()
                {
                    return;
                }
            }
            Err(err) => {
                tracing::error!(
                    target: crate::logging::targets::BRIDGE_LIFECYCLE,
                    error = %err,
                    "forge_sdk reader: events stream errored",
                );
                return;
            }
        }
    }
    tracing::info!(
        target: crate::logging::targets::BRIDGE_LIFECYCLE,
        "forge_sdk reader: events stream closed",
    );
}

pub(crate) async fn send_prompt(
    client: &Client,
    chunks: Vec<crate::agent::types::PromptChunk>,
) -> anyhow::Result<()> {
    if chunks.iter().all(|c| c.kind == "text") {
        let prompt: String =
            chunks.iter().filter_map(|c| c.value.as_str()).collect::<Vec<_>>().join("\n");
        client.send_user_message(&prompt).await?;
    } else {
        let content: Vec<serde_json::Value> = chunks
            .into_iter()
            .map(|c| match c.kind.as_str() {
                "text" => serde_json::json!({
                    "type": "text",
                    "text": c.value.as_str().unwrap_or(""),
                }),
                "image" => serde_json::json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": c.value.get("mime_type").and_then(|v| v.as_str()).unwrap_or("image/png"),
                        "data": c.value.get("data").and_then(|v| v.as_str()).unwrap_or(""),
                    },
                }),
                _ => c.value,
            })
            .collect();
        client.send_user_message_with_content(&content).await?;
    }
    Ok(())
}

pub(crate) fn parse_permission_mode(mode: &str) -> anyhow::Result<PermissionMode> {
    match mode {
        "default" | "ask" => Ok(PermissionMode::Ask),
        "acceptEdits" | "accept_edits" => Ok(PermissionMode::AcceptEdits),
        "plan" => Ok(PermissionMode::Plan),
        "bypassPermissions" | "bypass_permissions" => Ok(PermissionMode::BypassPermissions),
        "auto" => Ok(PermissionMode::Auto),
        "dontAsk" | "dont_ask" | "deny" => Ok(PermissionMode::DenyPermissions),
        other => Err(anyhow::anyhow!("forge_sdk: unknown permission mode {other:?}")),
    }
}

// ----------------------------------------------------------------------------
// Permission / question round-trip
// ----------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn build_options_with_callback(
    cwd: &str,
    resume: Option<&str>,
    launch_settings: &crate::agent::client::SessionLaunchSettings,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    pending: PendingResponses,
    pending_questions: PendingQuestions,
    session_id_slot: Arc<std::sync::Mutex<String>>,
) -> Options {
    let callback = move |ctx: ToolPermissionContext| {
        let event_tx = event_tx.clone();
        let pending = Arc::clone(&pending);
        let pending_questions = Arc::clone(&pending_questions);
        let session_id = session_id_slot.lock().map(|s| s.clone()).unwrap_or_default();
        async move {
            if ctx.tool_name == bridge_user_interaction::ASK_USER_QUESTION_TOOL_NAME {
                run_ask_user_question(ctx, session_id, &event_tx, &pending_questions).await
            } else {
                run_permission_request(ctx, session_id, &event_tx, &pending).await
            }
        }
    };

    let mut b = OptionsBuilder::new().can_use_tool(callback).permission_prompt_tool_name("stdio");
    if !cwd.is_empty() {
        b = b.cwd(PathBuf::from(cwd));
    }
    if let Some(id) = resume {
        b = b.resume(id);
    }

    let mut applied_mode: Option<&'static str> = None;
    let mut applied_model: Option<String> = None;
    let mut applied_effort: Option<String> = None;
    let settings_present = launch_settings.settings.is_some();
    if let Some(settings_value) = launch_settings.settings.as_ref()
        && let Some(settings_record) = settings_value.as_object()
    {
        if let Some(perms) =
            settings_record.get("permissions").and_then(serde_json::Value::as_object)
            && let Some(default_mode_str) =
                perms.get("defaultMode").and_then(serde_json::Value::as_str)
            && let Ok(mode) = parse_permission_mode(default_mode_str)
        {
            b = b.permission_mode(mode);
            applied_mode = Some(mode.as_cli_arg());
        }
        if let Some(model) = settings_record.get("model").and_then(serde_json::Value::as_str)
            && !model.trim().is_empty()
        {
            b = b.model(model);
            applied_model = Some(model.to_owned());
        }
        if let Some(effort) = settings_record.get("effortLevel").and_then(serde_json::Value::as_str)
            && !effort.trim().is_empty()
        {
            applied_effort = Some(effort.to_owned());
            b = b.extra_arg("effort", Some(effort.to_owned()));
        }
    }
    tracing::info!(
        target: crate::logging::targets::BRIDGE_LIFECYCLE,
        event_name = "forge_sdk_options_built",
        message = "launch_settings → forge-sdk OptionsBuilder",
        outcome = "info",
        settings_present,
        applied_permission_mode = applied_mode.unwrap_or("(none)"),
        applied_model = applied_model.as_deref().unwrap_or("(none)"),
        applied_effort = applied_effort.as_deref().unwrap_or("(none)"),
        cwd_present = !cwd.is_empty(),
        resume_present = resume.is_some(),
    );
    b.build()
}

async fn run_permission_request(
    ctx: ToolPermissionContext,
    session_id: String,
    event_tx: &mpsc::UnboundedSender<AgentEvent>,
    pending: &PendingResponses,
) -> PermissionDecision {
    let (tx, rx) = oneshot::channel();
    if let Ok(mut map) = pending.lock() {
        map.insert(ctx.tool_use_id.clone(), tx);
    }
    let event = synth_permission_request(&session_id, &ctx);
    if event_tx.send(event).is_err() {
        return PermissionDecision::deny("event channel closed");
    }
    match rx.await {
        Ok(decision) => decision,
        Err(_) => PermissionDecision::deny("response channel closed"),
    }
}

async fn run_ask_user_question(
    ctx: ToolPermissionContext,
    session_id: String,
    event_tx: &mpsc::UnboundedSender<AgentEvent>,
    pending_questions: &PendingQuestions,
) -> PermissionDecision {
    use crate::agent::types::QuestionOutcome;

    let prompts = bridge_user_interaction::parse_ask_user_question_prompts(&ctx.tool_input);
    if prompts.is_empty() {
        return PermissionDecision::allow();
    }

    let total = prompts.len() as u64;
    let base_tool_call = synth_question_base_tool_call(&ctx);
    let mut answers = serde_json::Map::new();
    let mut annotations = serde_json::Map::new();

    for (index, prompt) in prompts.iter().enumerate() {
        let request = bridge_user_interaction::build_question_request(
            &base_tool_call,
            prompt,
            index as u64,
            total,
        );
        let (tx, rx) = oneshot::channel();
        if let Ok(mut map) = pending_questions.lock() {
            map.insert(ctx.tool_use_id.clone(), tx);
        }
        if event_tx
            .send(AgentEvent::QuestionRequest {
                session_id: session_id.clone(),
                request: request.clone(),
            })
            .is_err()
        {
            return PermissionDecision::deny("event channel closed");
        }
        let Ok(outcome) = rx.await else {
            return PermissionDecision::deny("response channel closed");
        };
        match outcome {
            QuestionOutcome::Answered { selected_option_ids, annotation } => {
                let selected: Vec<crate::agent::types::QuestionOption> = request
                    .prompt
                    .options
                    .iter()
                    .filter(|opt| selected_option_ids.iter().any(|id| id == &opt.option_id))
                    .cloned()
                    .collect();
                if selected.is_empty() || (!prompt.multi_select && selected.len() != 1) {
                    return PermissionDecision::deny("Question answer was invalid");
                }
                let answer =
                    selected.iter().map(|o| o.label.as_str()).collect::<Vec<_>>().join(", ");
                answers.insert(prompt.question.clone(), serde_json::Value::String(answer));
                if let Some(annotation) =
                    bridge_user_interaction::derive_annotation(&selected, annotation.as_ref())
                    && let Ok(v) = serde_json::to_value(&annotation)
                {
                    annotations.insert(prompt.question.clone(), v);
                }
            }
            QuestionOutcome::Cancelled => {
                return PermissionDecision::deny("Question cancelled");
            }
        }
    }

    let updated_input =
        bridge_user_interaction::build_updated_input(&ctx.tool_input, answers, annotations);
    PermissionDecision::allow_with_input(updated_input)
}

fn synth_question_base_tool_call(ctx: &ToolPermissionContext) -> crate::agent::types::ToolCall {
    use crate::agent::types::ToolCall;
    ToolCall {
        tool_call_id: ctx.tool_use_id.clone(),
        title: bridge_user_interaction::ASK_USER_QUESTION_TOOL_NAME.to_owned(),
        kind: "ask".to_owned(),
        status: "pending".to_owned(),
        content: Vec::new(),
        raw_input: Some(ctx.tool_input.clone()),
        raw_output: None,
        output_metadata: None,
        task_metadata: None,
        locations: Vec::new(),
        meta: None,
    }
}

pub(crate) fn deliver_permission_response(
    pending: &PendingResponses,
    tool_call_id: &str,
    outcome: crate::agent::types::PermissionOutcome,
) {
    let Some(tx) = take_pending(pending, tool_call_id) else {
        tracing::warn!(
            target: crate::logging::targets::APP_PERMISSION,
            tool_call_id,
            "forge_sdk: PermissionResponse for unknown tool_call_id (already drained?)",
        );
        return;
    };
    let decision = match outcome {
        crate::agent::types::PermissionOutcome::Selected { option_id } => {
            if option_id.eq_ignore_ascii_case("deny") || option_id.eq_ignore_ascii_case("reject") {
                PermissionDecision::deny(format!("user denied: {option_id}"))
            } else {
                PermissionDecision::allow()
            }
        }
        crate::agent::types::PermissionOutcome::Cancelled => {
            PermissionDecision::deny("user cancelled")
        }
    };
    let _ = tx.send(decision);
}

pub(crate) fn deliver_question_response(
    pending: &PendingQuestions,
    tool_call_id: &str,
    outcome: crate::agent::types::QuestionOutcome,
) {
    let Some(tx) = pending.lock().ok().and_then(|mut m| m.remove(tool_call_id)) else {
        tracing::warn!(
            target: crate::logging::targets::APP_PERMISSION,
            tool_call_id,
            "forge_sdk: QuestionResponse for unknown tool_call_id",
        );
        return;
    };
    let _ = tx.send(outcome);
}

fn take_pending(
    pending: &PendingResponses,
    tool_call_id: &str,
) -> Option<oneshot::Sender<PermissionDecision>> {
    pending.lock().ok()?.remove(tool_call_id)
}

fn synth_permission_request(session_id: &str, ctx: &ToolPermissionContext) -> AgentEvent {
    use crate::agent::types::{PermissionDisplay, PermissionRequest, ToolCall};
    let tool_call = ToolCall {
        tool_call_id: ctx.tool_use_id.clone(),
        title: ctx.tool_name.clone(),
        kind: "execute".to_owned(),
        status: "pending".to_owned(),
        content: Vec::new(),
        raw_input: Some(ctx.tool_input.clone()),
        raw_output: None,
        output_metadata: None,
        task_metadata: None,
        locations: Vec::new(),
        meta: None,
    };
    let display = PermissionDisplay {
        title: ctx.title.clone(),
        display_name: ctx.display_name.clone(),
        description: ctx.description.clone(),
    };
    AgentEvent::PermissionRequest {
        session_id: session_id.to_owned(),
        request: PermissionRequest {
            tool_call,
            options: default_permission_options(),
            display: Some(display),
        },
    }
}

fn default_permission_options() -> Vec<crate::agent::types::PermissionOption> {
    vec![
        crate::agent::types::PermissionOption {
            option_id: "allow_once".to_owned(),
            name: "Allow once".to_owned(),
            description: None,
            kind: "allow_once".to_owned(),
        },
        crate::agent::types::PermissionOption {
            option_id: "allow_always".to_owned(),
            name: "Allow always".to_owned(),
            description: None,
            kind: "allow_always".to_owned(),
        },
        crate::agent::types::PermissionOption {
            option_id: "deny".to_owned(),
            name: "Deny".to_owned(),
            description: None,
            kind: "reject_once".to_owned(),
        },
    ]
}

pub(crate) fn clamp_percentage_to_u8(p: f64) -> u8 {
    if p.is_nan() {
        return 0;
    }
    let clamped = p.clamp(0.0, 100.0).round();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let n = clamped as u8;
    n
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::{
        PendingQuestions, PendingResponses, deliver_permission_response, deliver_question_response,
        synth_permission_request, take_pending,
    };
    use crate::agent::client::AgentEvent;
    use crate::agent::types::{ElicitationAction, PermissionOutcome, QuestionOutcome};
    use forge_sdk::ToolPermissionContext;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tokio::sync::oneshot;

    fn fresh_pending() -> PendingResponses {
        Arc::new(Mutex::new(HashMap::new()))
    }

    fn ctx(tool_name: &str, tool_use_id: &str, input: serde_json::Value) -> ToolPermissionContext {
        ToolPermissionContext::new(tool_name, input, tool_use_id, None)
    }

    fn park(
        pending: &PendingResponses,
        id: &str,
    ) -> oneshot::Receiver<forge_sdk::PermissionDecision> {
        let (tx, rx) = oneshot::channel();
        pending.lock().unwrap().insert(id.to_owned(), tx);
        rx
    }

    #[test]
    fn permission_response_allow_drains_oneshot_with_allow() {
        let pending = fresh_pending();
        let rx = park(&pending, "tu_1");
        deliver_permission_response(
            &pending,
            "tu_1",
            PermissionOutcome::Selected { option_id: "allow_once".to_owned() },
        );
        let decision = rx.blocking_recv().expect("oneshot resolved");
        assert!(decision.is_allow());
    }

    #[test]
    fn permission_response_deny_keyword_drains_with_deny() {
        let pending = fresh_pending();
        let rx = park(&pending, "tu_2");
        deliver_permission_response(
            &pending,
            "tu_2",
            PermissionOutcome::Selected { option_id: "deny".to_owned() },
        );
        let decision = rx.blocking_recv().expect("oneshot resolved");
        assert!(!decision.is_allow());
    }

    #[test]
    fn permission_response_cancel_drains_with_deny() {
        let pending = fresh_pending();
        let rx = park(&pending, "tu_3");
        deliver_permission_response(&pending, "tu_3", PermissionOutcome::Cancelled);
        let decision = rx.blocking_recv().expect("oneshot resolved");
        assert!(!decision.is_allow());
    }

    #[test]
    fn permission_response_unknown_id_is_silent_no_op() {
        let pending = fresh_pending();
        deliver_permission_response(
            &pending,
            "missing",
            PermissionOutcome::Selected { option_id: "allow_once".to_owned() },
        );
        assert!(pending.lock().unwrap().is_empty());
    }

    fn fresh_pending_questions() -> PendingQuestions {
        Arc::new(Mutex::new(HashMap::new()))
    }

    fn park_question(
        pending: &PendingQuestions,
        id: &str,
    ) -> oneshot::Receiver<crate::agent::types::QuestionOutcome> {
        let (tx, rx) = oneshot::channel();
        pending.lock().unwrap().insert(id.to_owned(), tx);
        rx
    }

    #[test]
    fn question_response_forwards_typed_outcome() {
        let pending = fresh_pending_questions();
        let rx = park_question(&pending, "tu_q1");
        deliver_question_response(
            &pending,
            "tu_q1",
            QuestionOutcome::Answered {
                selected_option_ids: vec!["question_0".to_owned(), "question_1".to_owned()],
                annotation: None,
            },
        );
        let outcome = rx.blocking_recv().expect("oneshot resolved");
        match outcome {
            QuestionOutcome::Answered { selected_option_ids, .. } => {
                assert_eq!(selected_option_ids, vec!["question_0", "question_1"]);
            }
            QuestionOutcome::Cancelled => panic!("expected answered outcome"),
        }
    }

    #[test]
    fn question_response_cancelled_forwards_typed_outcome() {
        let pending = fresh_pending_questions();
        let rx = park_question(&pending, "tu_q2");
        deliver_question_response(&pending, "tu_q2", QuestionOutcome::Cancelled);
        let outcome = rx.blocking_recv().expect("oneshot resolved");
        assert!(matches!(outcome, QuestionOutcome::Cancelled));
    }

    #[test]
    fn question_response_unknown_id_is_silent_no_op() {
        let pending = fresh_pending_questions();
        deliver_question_response(&pending, "missing", QuestionOutcome::Cancelled);
        assert!(pending.lock().unwrap().is_empty());
    }

    #[test]
    fn synth_permission_request_carries_tool_input_and_display_fields() {
        let c = ctx("Bash", "tu_p1", json!({ "command": "ls" })).with_display(
            None,
            None,
            Some("Run shell command".to_owned()),
            Some("Bash".to_owned()),
            Some("Lists directory entries".to_owned()),
        );
        let event = synth_permission_request("sess_1", &c);
        let AgentEvent::PermissionRequest { session_id, request } = event else {
            panic!("expected PermissionRequest");
        };
        assert_eq!(session_id, "sess_1");
        assert_eq!(request.tool_call.tool_call_id, "tu_p1");
        assert_eq!(request.tool_call.title, "Bash");
        assert_eq!(request.tool_call.raw_input, Some(json!({ "command": "ls" })));
        assert_eq!(request.options.len(), 3);
        assert!(request.options.iter().any(|o| o.option_id == "deny"));
        let display = request.display.expect("display populated");
        assert_eq!(display.title.as_deref(), Some("Run shell command"));
        assert_eq!(display.display_name.as_deref(), Some("Bash"));
        assert_eq!(display.description.as_deref(), Some("Lists directory entries"));
    }

    #[test]
    fn take_pending_removes_entry() {
        let pending = fresh_pending();
        let _rx = park(&pending, "tu_x");
        assert!(take_pending(&pending, "tu_x").is_some());
        assert!(take_pending(&pending, "tu_x").is_none());
    }

    #[test]
    fn elicitation_action_variants_match_expected_wire_strings() {
        let cases = [
            (ElicitationAction::Accept, "accept"),
            (ElicitationAction::Decline, "decline"),
            (ElicitationAction::Cancel, "cancel"),
        ];
        for (action, expected) in cases {
            let actual = match action {
                ElicitationAction::Accept => "accept",
                ElicitationAction::Decline => "decline",
                ElicitationAction::Cancel => "cancel",
            };
            assert_eq!(actual, expected);
        }
    }
}
