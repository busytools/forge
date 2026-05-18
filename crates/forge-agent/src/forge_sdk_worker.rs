//! Session-launcher + reader-pump helpers for the bridge layer.
//!
//! Builds `forge_sdk::Options` from the TUI's launch settings (with
//! the `can_use_tool` callback wired in), spawns the `Client`, emits a
//! synthetic `Connected`, and pumps the event stream from
//! `forge_sdk::Client::spawn` into `AgentEvent::SdkMessage`. The
//! bridge owns the resulting `Client`; this module exposes the
//! helpers it calls.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use forge_primitives::{PermissionDecision, ToolPermissionContext};
use forge_sdk::{
    Client, HookContext, HookDecision, HooksBuilder, Options, OptionsBuilder, PermissionMode,
    PreToolUseInput, UserPromptSubmitInput,
};
use tokio::sync::{mpsc, oneshot};
use tracing::Instrument;

use crate::client::AgentEvent;
use crate::forge_sdk_bridge::{ForgeSdkBridge, PendingQuestions, PendingResponses};
use crate::{
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
    launch_settings: &crate::client::SessionLaunchSettings,
) -> anyhow::Result<()> {
    // If we already have a client, drop it so the existing subprocess
    // shuts down cleanly before the replacement spawns. Disconnect
    // failures are best-effort — log a breadcrumb so a stuck zombie
    // subprocess is observable in postmortems.
    if let Some(prev) = bridge.clear_client()
        && let Err(err) = prev.disconnect().await
    {
        tracing::debug!(
            target: crate::logging::targets::BRIDGE_LIFECYCLE,
            error = %err,
            "previous client disconnect failed during session swap",
        );
    }

    let config_dir = bridge.config_dir();
    let display_name = bridge.display_name();
    let options = build_options_with_callback(
        cwd,
        resume_id,
        launch_settings,
        bridge.event_tx().clone(),
        Arc::clone(bridge.inner_pending()),
        Arc::clone(bridge.inner_pending_questions()),
        Arc::clone(bridge.session_id_slot_arc()),
        &config_dir,
    );
    let (client, events) = Client::spawn(options).await?;
    // For resume sessions the CLI flag carried the real session id —
    // prefer that over `Client::session_id()`, which is empty until
    // `system/init` lands on the wire (per `Client::spawn` docs, after
    // both the initialize control_response AND a user message). For
    // new sessions we also fall back to whatever Client captured
    // during its init loop (typically empty), and the App-side handler
    // adopts the first non-empty session id seen on the wire.
    let session_id = match resume_id {
        Some(id) if !id.is_empty() => id.to_owned(),
        _ => client.session_id(),
    };
    bridge.session_id_slot_arc().lock().clone_from(&session_id);

    // The caller owns the session's cwd source — workspace flow
    // passes the forge.toml-derived project path; in-session
    // /resume sources the cwd from the session's transcript. An
    // empty cwd is a caller-side bug; log it and pass through. No
    // `current_dir()` fallback.
    if cwd.is_empty() {
        tracing::warn!(
            target: crate::logging::targets::BRIDGE_LIFECYCLE,
            "spawn_session received empty cwd; session will lack working-directory scope (caller should pass forge.toml-derived path or session-recorded cwd)",
        );
    }
    let cwd_owned = cwd.to_owned();

    // Install the client into the bridge BEFORE emitting Connected.
    // The TUI's Connected handler immediately fires command-channel
    // requests (status snapshot, oauth credentials, context usage, mcp
    // snapshot) which the dispatcher routes through `bridge.dispatch`.
    // Dispatch reads `bridge.client()` — which must already be Some,
    // otherwise the dispatch returns an error and the request is
    // dropped (chip stays empty / snapshot never lands).
    bridge.set_client(client.clone());

    // Emit Connected BEFORE spawning the reader subtask so the App
    // sees Connected first on its mpsc — otherwise the reader can
    // race and push an SdkMessage before Connected, leaving
    // `app.session_id` = None when the SdkMessage arrives.
    emit_connected(
        bridge.event_tx(),
        &client,
        &session_id,
        &cwd_owned,
        launch_settings,
        resume_id,
        &config_dir,
        display_name.as_deref(),
    )
    .await;

    // Reader subtask — owns the events receiver. Client is the writer-side
    // handle (Arc-backed, Clone) and stays on the bridge.
    let reader_event_tx = bridge.event_tx().clone();
    let reader_session_id = session_id.clone();
    let span = tracing::info_span!("sdk_reader", session_id = %reader_session_id);
    tokio::spawn(reader_loop(events, reader_event_tx, reader_session_id).instrument(span));
    Ok(())
}

/// Build the typed `Connected` envelope from the SDK's cached init data
/// + the initialize `control_response`, and emit it onto `event_tx`.
#[allow(clippy::too_many_arguments)]
async fn emit_connected(
    event_tx: &mpsc::UnboundedSender<AgentEvent>,
    client: &Client,
    session_id: &str,
    cwd: &str,
    launch_settings: &crate::client::SessionLaunchSettings,
    resume_id: Option<&str>,
    config_dir: &Path,
    display_name: Option<&str>,
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
        .or(Some(bridge_state::PermissionMode::Ask));
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
        let messages = load_history_messages(config_dir, prev_session_id, cwd, session_id);
        if messages.is_empty() { None } else { Some(messages) }
    });

    if event_tx
        .send(AgentEvent::Connected {
            session_id: session_id.to_owned(),
            cwd: cwd.to_owned(),
            current_model,
            available_models,
            mode,
            history_updates,
        })
        .is_err()
    {
        tracing::warn!(
            target: crate::logging::targets::BRIDGE_LIFECYCLE,
            session_id,
            "Connected event channel closed before emit; session stuck on Connecting"
        );
    }

    // account_info_from_shell shells out to `claude auth status` (~50ms
    // blocking per the docstring) — wrap in spawn_blocking so the
    // async worker doesn't park a tokio worker thread for the
    // duration. account_info_from_init is in-memory, no I/O.
    let account = if let Some(account) = client.account_info_from_init() {
        Some(account)
    } else {
        let config_dir_owned = config_dir.to_owned();
        match tokio::task::spawn_blocking(move || {
            crate::cloud::auth_status::account_info_from_shell(&config_dir_owned)
        })
        .await
        {
            Ok(opt) => opt,
            Err(join_err) => {
                tracing::warn!(
                    target: crate::logging::targets::BRIDGE_LIFECYCLE,
                    error = %join_err,
                    "account_info_from_shell spawn_blocking task panicked"
                );
                None
            }
        }
    };
    if let Some(account) = account {
        let forge_account =
            display_name.map(|d| forge_primitives::ForgeAccountIdentity::new(d.to_owned()));
        if event_tx
            .send(AgentEvent::StatusSnapshot {
                session_id: session_id.to_owned(),
                account,
                forge_account,
            })
            .is_err()
        {
            tracing::warn!(
                target: crate::logging::targets::BRIDGE_LIFECYCLE,
                session_id,
                "StatusSnapshot event channel closed before emit"
            );
        }
    }

    if event_tx
        .send(AgentEvent::SessionsListed { sessions: list_recent_sessions(config_dir, cwd).await })
        .is_err()
    {
        tracing::warn!(
            target: crate::logging::targets::BRIDGE_LIFECYCLE,
            session_id,
            "SessionsListed event channel closed before emit"
        );
    }
}

fn load_history_messages(
    config_dir: &Path,
    prev_session_id: &str,
    cwd: &str,
    session_id: &str,
) -> Vec<forge_primitives::Message> {
    let dir = if cwd.is_empty() { None } else { Some(cwd) };
    let messages =
        crate::userdata::catalog::scan::get_session_messages(config_dir, prev_session_id, dir);
    let raw: Vec<serde_json::Value> = messages
        .into_iter()
        .map(|m| {
            let kind = match m.kind {
                forge_primitives::SessionMessageKind::User => "user",
                forge_primitives::SessionMessageKind::Assistant => "assistant",
            };
            serde_json::json!({
                "type": kind,
                "message": m.message,
                "parent_tool_use_id": m.parent_tool_use_id,
            })
        })
        .collect();
    let mut synthesized = crate::replay::synthesize_replay_messages(&raw);
    // Stamp the resumed session_id on every synthesised Message — the
    // synthesizer leaves it empty so the caller picks the right value.
    for msg in &mut synthesized {
        match msg {
            forge_primitives::Message::Assistant { session_id: s, .. }
            | forge_primitives::Message::User { session_id: s, .. } => {
                session_id.clone_into(s);
            }
            _ => {}
        }
    }
    synthesized
}

async fn list_recent_sessions(
    config_dir: &Path,
    cwd: &str,
) -> Vec<forge_primitives::SessionListEntry> {
    use forge_primitives::SessionListEntry;
    const MAX_RECENT: usize = 50;
    let dir = if cwd.is_empty() { None } else { Some(cwd) };
    crate::userdata::catalog::scan::list_sessions(config_dir, dir, Some(MAX_RECENT), 0)
        .await
        .into_iter()
        .map(|info| SessionListEntry {
            session_id: info.session_id,
            summary: info.summary,
            last_modified_ms: info.last_modified,
            cwd: info.cwd,
            custom_title: info.custom_title,
            first_prompt: info.first_prompt,
        })
        .collect()
}

async fn reader_loop(
    mut events: forge_sdk::ClientEvents,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    session_id: String,
) {
    while let Some(item) = events.recv().await {
        match item {
            Ok(msg) => {
                let session_id_for_sdk_msg = match msg.session_id() {
                    Some(sid) => sid.to_owned(),
                    None => session_id.clone(),
                };
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
    chunks: Vec<forge_primitives::PromptChunk>,
) -> anyhow::Result<()> {
    debug_assert!(!chunks.is_empty(), "send_prompt called with empty chunks");
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
    forge_primitives::permission::PermissionMode::from_wire(mode)
        .ok_or_else(|| anyhow::anyhow!("forge_sdk: unknown permission mode {mode:?}"))
}

// ----------------------------------------------------------------------------
// Permission / question round-trip
// ----------------------------------------------------------------------------

// Options-builder bridge — args mirror `forge_sdk::OptionsBuilder` setters 1:1. Wrapping doesn't simplify — caller would just unpack again.
#[allow(clippy::too_many_arguments)]
fn build_options_with_callback(
    cwd: &str,
    resume: Option<&str>,
    launch_settings: &crate::client::SessionLaunchSettings,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    pending: PendingResponses,
    pending_questions: PendingQuestions,
    session_id_slot: Arc<parking_lot::Mutex<String>>,
    config_dir: &Path,
) -> Options {
    // Passthrough hooks emit `AgentEvent::HookObservation` for every
    // PreToolUse / UserPromptSubmit input without altering the dispatch
    // outcome. PreToolUse carries subagent attribution (`agent_id` +
    // `agent_type`) — see #84.
    let pre_tool_observe_tx = event_tx.clone();
    let pre_tool_observe_sid = Arc::clone(&session_id_slot);
    let user_prompt_observe_tx = event_tx.clone();
    let user_prompt_observe_sid = Arc::clone(&session_id_slot);

    let observation_hooks = HooksBuilder::new()
        .pre_tool_use("*", move |input: PreToolUseInput, _ctx: HookContext| {
            let tx = pre_tool_observe_tx.clone();
            let session_id = pre_tool_observe_sid.lock().clone();
            async move {
                let _ = tx.send(AgentEvent::HookObservation {
                    session_id,
                    tool_use_id: Some(input.tool_use_id.clone()),
                    permission_mode: input.base.permission_mode.clone(),
                    effort: input.base.effort.as_ref().map(|e| e.level.clone()),
                    agent_id: input.subagent.agent_id.clone(),
                    agent_type: input.subagent.agent_type.clone(),
                });
                HookDecision::passthrough()
            }
        })
        .user_prompt_submit(move |input: UserPromptSubmitInput, _ctx: HookContext| {
            let tx = user_prompt_observe_tx.clone();
            let session_id = user_prompt_observe_sid.lock().clone();
            async move {
                let _ = tx.send(AgentEvent::HookObservation {
                    session_id,
                    tool_use_id: None,
                    permission_mode: input.base.permission_mode.clone(),
                    effort: input.base.effort.as_ref().map(|e| e.level.clone()),
                    agent_id: None,
                    agent_type: None,
                });
                HookDecision::passthrough()
            }
        })
        .build();

    let callback = move |ctx: ToolPermissionContext| {
        let event_tx = event_tx.clone();
        let pending = Arc::clone(&pending);
        let pending_questions = Arc::clone(&pending_questions);
        let session_id = session_id_slot.lock().clone();
        async move {
            if ctx.tool_name == bridge_user_interaction::ASK_USER_QUESTION_TOOL_NAME {
                run_ask_user_question(ctx, session_id, &event_tx, &pending_questions).await
            } else {
                run_permission_request(ctx, session_id, &event_tx, &pending).await
            }
        }
    };

    let mut b = OptionsBuilder::new()
        .can_use_tool(callback)
        .hooks(observation_hooks)
        .permission_prompt_tool_name("stdio");
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

    // Effort resolution order:
    //   1. settings.json `effortLevel` (handled above).
    //   2. `CLAUDE_CODE_EFFORT_LEVEL` env var — leave it to env
    //      inheritance so a user override isn't shadowed by `--effort`.
    //   3. Default to `--effort max`.
    let effort_source: &'static str = if applied_effort.is_some() {
        "settings"
    } else {
        match std::env::var("CLAUDE_CODE_EFFORT_LEVEL") {
            Ok(env_value) if !env_value.trim().is_empty() => {
                applied_effort = Some(env_value);
                "env_var"
            }
            _ => {
                applied_effort = Some("max".to_owned());
                b = b.extra_arg("effort", Some("max".to_owned()));
                "default_max"
            }
        }
    };

    // Per-spawn `CLAUDE_CONFIG_DIR` — workspace-driven so each
    // `claude` subprocess reads/writes the bound account's
    // user-data tree (oauth tokens, projects history, settings).
    // Threaded through as a typed `Path` from the bridge; no
    // free-form HashMap of env vars at this layer.
    b = b.env("CLAUDE_CONFIG_DIR", config_dir.to_string_lossy().to_string());

    tracing::info!(
        target: crate::logging::targets::BRIDGE_LIFECYCLE,
        event_name = "forge_sdk_options_built",
        message = "launch_settings → forge-sdk OptionsBuilder",
        outcome = "info",
        settings_present,
        applied_permission_mode = applied_mode.unwrap_or("(none)"),
        applied_model = applied_model.as_deref().unwrap_or("(none)"),
        applied_effort = applied_effort.as_deref().unwrap_or("(none)"),
        effort_source,
        cwd_present = !cwd.is_empty(),
        resume_present = resume.is_some(),
        config_dir = %config_dir.display(),
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
    {
        let mut guard = pending.lock();
        // Guard against duplicate tool_use_id from upstream (CLI retry
        // bug, protocol drift). Without this, the second insert
        // silently drops the first oneshot and the original
        // `run_permission_request` hangs until the channel is dropped.
        if let Some(prev) = guard.insert(ctx.tool_use_id.clone(), tx) {
            tracing::warn!(
                target: crate::logging::targets::BRIDGE_LIFECYCLE,
                tool_use_id = %ctx.tool_use_id,
                "duplicate tool_use_id on permission request; cancelling prior oneshot"
            );
            // Best-effort resolve the displaced sender so the prior
            // run_permission_request future doesn't hang.
            let _ = prev.send(PermissionDecision::deny("displaced by duplicate tool_use_id"));
        }
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
    use forge_primitives::QuestionOutcome;

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
        {
            let mut guard = pending_questions.lock();
            if let Some(prev) = guard.insert(ctx.tool_use_id.clone(), tx) {
                tracing::warn!(
                    target: crate::logging::targets::BRIDGE_LIFECYCLE,
                    tool_use_id = %ctx.tool_use_id,
                    "duplicate tool_use_id on question request; cancelling prior oneshot"
                );
                let _ = prev.send(QuestionOutcome::Cancelled);
            }
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
                let selected: Vec<forge_primitives::QuestionOption> = request
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
                {
                    match serde_json::to_value(&annotation) {
                        Ok(v) => {
                            annotations.insert(prompt.question.clone(), v);
                        }
                        Err(err) => {
                            tracing::warn!(
                                target: "forge_agent::forge_sdk_worker",
                                question = %prompt.question,
                                error = %err,
                                "failed to serialise question annotation; dropping"
                            );
                        }
                    }
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

fn synth_question_base_tool_call(ctx: &ToolPermissionContext) -> forge_primitives::ToolCall {
    use forge_primitives::ToolCall;
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
    outcome: forge_primitives::PermissionOutcome,
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
        forge_primitives::PermissionOutcome::Selected {
            action, notes_text, edited_input, ..
        } => dispatch_permission_action(action, notes_text.as_deref().unwrap_or(""), edited_input),
        forge_primitives::PermissionOutcome::Cancelled => {
            PermissionDecision::deny("user cancelled")
        }
    };
    if tx.send(decision).is_err() {
        tracing::debug!(
            target: crate::logging::targets::APP_PERMISSION,
            tool_call_id,
            "PermissionResponse oneshot receiver dropped before delivery",
        );
    }
}

/// Build the `PermissionDecision` for a submitted option's `action`.
/// `notes_text` is the user's "tell Claude" feedback string (or
/// empty); consumed only when the action is `Deny`. `edited_input` is
/// the user's modified tool args; consumed only when the action is
/// `AllowWithInput`.
pub(crate) fn dispatch_permission_action(
    action: forge_primitives::permission_ui::PermissionAction,
    notes_text: &str,
    edited_input: Option<serde_json::Value>,
) -> PermissionDecision {
    use forge_primitives::permission_ui::PermissionAction;
    match action {
        PermissionAction::Allow => PermissionDecision::allow(),
        PermissionAction::AllowWithUpdates { updates } => {
            PermissionDecision::allow().with_updated_permissions(updates)
        }
        PermissionAction::AllowWithInput => {
            PermissionDecision::allow_with_input(edited_input.unwrap_or_default())
        }
        PermissionAction::Deny => {
            let reason = if notes_text.trim().is_empty() {
                "Denied by user".to_owned()
            } else {
                notes_text.trim().to_owned()
            };
            PermissionDecision::deny(reason)
        }
    }
}

pub(crate) fn deliver_question_response(
    pending: &PendingQuestions,
    tool_call_id: &str,
    outcome: forge_primitives::QuestionOutcome,
) {
    let Some(tx) = pending.lock().remove(tool_call_id) else {
        tracing::warn!(
            target: crate::logging::targets::APP_PERMISSION,
            tool_call_id,
            "forge_sdk: QuestionResponse for unknown tool_call_id",
        );
        return;
    };
    if tx.send(outcome).is_err() {
        tracing::debug!(
            target: crate::logging::targets::APP_PERMISSION,
            tool_call_id,
            "QuestionResponse oneshot receiver dropped before delivery",
        );
    }
}

fn take_pending(
    pending: &PendingResponses,
    tool_call_id: &str,
) -> Option<oneshot::Sender<PermissionDecision>> {
    pending.lock().remove(tool_call_id)
}

fn synth_permission_request(session_id: &str, ctx: &ToolPermissionContext) -> AgentEvent {
    use forge_primitives::{PermissionDisplay, PermissionRequest, ToolCall};
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
        decision_reason: ctx.decision_reason.clone(),
    };
    AgentEvent::PermissionRequest {
        session_id: session_id.to_owned(),
        request: PermissionRequest {
            tool_call,
            options: build_permission_options(ctx),
            display: Some(display),
        },
    }
}

/// Construct the contextual option list for a permission prompt.
/// Derives "Allow always for X" / "Allow always & add dirs" / "Allow
/// always & switch mode" entries from `ctx.suggestions`; adds the
/// synthesized "Allow with edits" for editable tools and the universal
/// "Tell Claude something else" escape hatch.
///
/// Wire reality (captured 2026-05-18; see baselines/sdk/2.1.117/):
/// - `Read` outside workspace -> `addRules` with `{toolName, ruleContent}`.
///   macOS sends BOTH `//tmp/**` AND `//private/tmp/**` -- these collapse
///   into ONE display option whose action carries BOTH rules.
/// - `Write` / `Edit` outside workspace -> `setMode -> acceptEdits` +
///   `addDirectories` (NOT addRules). Different variant mix.
/// - `ExitPlanMode` -> `suggestions: []` (empty). Plain Permission.
fn build_permission_options(
    ctx: &ToolPermissionContext,
) -> Vec<forge_primitives::PermissionOption> {
    use forge_primitives::PermissionOption;
    use forge_primitives::permission_ui::{PermissionAction, PermissionOptionKind};
    use forge_primitives::permissions::PermissionUpdate;

    let mut opts: Vec<PermissionOption> = Vec::new();

    // 1. Allow once is universal.
    opts.push(PermissionOption {
        option_id: "allow_once".to_owned(),
        name: "Allow once".to_owned(),
        kind: PermissionOptionKind::Allow,
        action: PermissionAction::Allow,
    });

    // 2. Derive "Allow always" options from ctx.suggestions, with macOS
    //    /tmp <-> /private/tmp dedupe for AddRules.
    let canonical = canonicalize_suggestions(&ctx.suggestions);
    for (i, update) in canonical.iter().enumerate() {
        let (name, action) = match update {
            PermissionUpdate::AddRules { rules, behavior, .. } => {
                if *behavior != forge_primitives::permissions::PermissionBehavior::Allow {
                    continue;
                }
                let summary = summarize_rules(rules);
                (
                    format!("Allow always for {} \u{b7} {}", ctx.tool_name, summary),
                    PermissionAction::AllowWithUpdates { updates: vec![update.clone()] },
                )
            }
            PermissionUpdate::AddDirectories { directories, .. } => {
                let summary = summarize_dirs(directories);
                (
                    format!("Allow always & add {summary} to allowed dirs"),
                    PermissionAction::AllowWithUpdates { updates: vec![update.clone()] },
                )
            }
            PermissionUpdate::SetMode { mode, .. } => (
                format!("Allow always & switch to {} mode", mode.display_name()),
                PermissionAction::AllowWithUpdates { updates: vec![update.clone()] },
            ),
            // RemoveRules / ReplaceRules / RemoveDirectories aren't
            // user-facing prompt options. Skip.
            PermissionUpdate::RemoveRules { .. }
            | PermissionUpdate::ReplaceRules { .. }
            | PermissionUpdate::RemoveDirectories { .. } => continue,
        };
        opts.push(PermissionOption {
            option_id: format!("allow_always_{i}"),
            name,
            kind: PermissionOptionKind::Allow,
            action,
        });
    }

    // 3. "Allow with edits" for editable tools.
    if is_editable_tool(&ctx.tool_name) {
        opts.push(PermissionOption {
            option_id: "allow_with_edits".to_owned(),
            name: "Allow with edits".to_owned(),
            kind: PermissionOptionKind::Edit,
            action: PermissionAction::AllowWithInput,
        });
    }

    // 4. Deny.
    opts.push(PermissionOption {
        option_id: "deny".to_owned(),
        name: "Deny".to_owned(),
        kind: PermissionOptionKind::Deny,
        action: PermissionAction::Deny,
    });

    // 5. Universal: Tell Claude something else.
    opts.push(PermissionOption {
        option_id: "tell_claude".to_owned(),
        name: "Tell Claude something else".to_owned(),
        kind: PermissionOptionKind::Notes,
        action: PermissionAction::Deny,
    });

    opts
}

/// Walk the suggestions list and collapse macOS `/tmp` <-> `/private/tmp`
/// AddRules mirrors into one merged entry. Two AddRules suggestions with
/// the same toolName + behavior + destination, whose rule_content differs
/// only by the `/tmp/` <-> `/private/tmp/` prefix, are merged into a
/// single AddRules whose `rules` vec contains BOTH entries so the CLI
/// installs both rules even though the UI shows one option.
fn canonicalize_suggestions(
    suggestions: &[forge_primitives::permissions::PermissionUpdate],
) -> Vec<forge_primitives::permissions::PermissionUpdate> {
    use forge_primitives::permissions::PermissionUpdate;

    let mut result: Vec<PermissionUpdate> = Vec::with_capacity(suggestions.len());
    let mut skip: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();

    for (i, update) in suggestions.iter().enumerate() {
        if skip.contains(&i) {
            continue;
        }
        let PermissionUpdate::AddRules { rules: rules_a, behavior: beh_a, destination: dst_a } =
            update
        else {
            result.push(update.clone());
            continue;
        };
        let mut merged_rules = rules_a.clone();
        for (j, other) in suggestions.iter().enumerate().skip(i + 1) {
            if skip.contains(&j) {
                continue;
            }
            let PermissionUpdate::AddRules { rules: rules_b, behavior: beh_b, destination: dst_b } =
                other
            else {
                continue;
            };
            if beh_a != beh_b || dst_a != dst_b {
                continue;
            }
            if is_tmp_mirror_pair(rules_a, rules_b) {
                merged_rules.extend(rules_b.iter().cloned());
                skip.insert(j);
            }
        }
        result.push(PermissionUpdate::AddRules {
            rules: merged_rules,
            behavior: *beh_a,
            destination: *dst_a,
        });
    }
    result
}

/// True if `rules_a` and `rules_b` differ only by the macOS
/// `/tmp/` <-> `/private/tmp/` prefix in their rule_content strings.
fn is_tmp_mirror_pair(
    rules_a: &[forge_primitives::permissions::PermissionRuleValue],
    rules_b: &[forge_primitives::permissions::PermissionRuleValue],
) -> bool {
    if rules_a.len() != rules_b.len() {
        return false;
    }
    rules_a.iter().zip(rules_b.iter()).all(|(a, b)| {
        if a.tool_name != b.tool_name {
            return false;
        }
        match (a.rule_content.as_deref(), b.rule_content.as_deref()) {
            (Some(ca), Some(cb)) => {
                let na = normalize_tmp_path(ca);
                let nb = normalize_tmp_path(cb);
                na == nb && ca != cb
            }
            // No content on either side: not a path-prefix mirror.
            _ => false,
        }
    })
}

/// Strip the `/private` prefix from `//private/tmp/...` rule_content so
/// the macOS mirror normalizes to the canonical `/tmp/...` form.
fn normalize_tmp_path(content: &str) -> String {
    content.replacen("//private/tmp/", "//tmp/", 1)
}

fn summarize_rules(rules: &[forge_primitives::permissions::PermissionRuleValue]) -> String {
    // Display: collapse the macOS /tmp + /private/tmp pair to "/tmp/**".
    let mut shown: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for r in rules {
        let label = r.rule_content.as_deref().map_or_else(
            || format!("any {} invocation", r.tool_name),
            |c| format!("paths matching {}", normalize_tmp_path(c)),
        );
        shown.insert(label);
    }
    shown.into_iter().collect::<Vec<_>>().join(", ")
}

fn summarize_dirs(dirs: &[String]) -> String {
    // Display: collapse the macOS /tmp + /private/tmp pair.
    let mut shown: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for d in dirs {
        shown.insert(d.replacen("/private/tmp", "/tmp", 1));
    }
    shown.into_iter().collect::<Vec<_>>().join(", ")
}

/// Tools whose `tool_input` is meaningful to edit before approving.
fn is_editable_tool(tool_name: &str) -> bool {
    matches!(tool_name, "Bash" | "Edit" | "Write" | "MultiEdit" | "NotebookEdit")
}

pub(crate) fn clamp_percentage_to_u8(p: f64) -> u8 {
    if p.is_nan() {
        return 0;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let n = p.clamp(0.0, 100.0).round() as u8;
    n
}

#[cfg(test)]
mod tests {
    use super::{
        PendingQuestions, PendingResponses, deliver_permission_response, deliver_question_response,
        synth_permission_request, take_pending,
    };
    use crate::client::AgentEvent;
    use forge_primitives::ToolPermissionContext;
    use forge_primitives::permission_ui::PermissionAction;
    use forge_primitives::{PermissionOutcome, QuestionOutcome};
    use parking_lot::Mutex;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Arc;
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
    ) -> oneshot::Receiver<forge_primitives::PermissionDecision> {
        let (tx, rx) = oneshot::channel();
        pending.lock().insert(id.to_owned(), tx);
        rx
    }

    #[test]
    fn permission_response_allow_drains_oneshot_with_allow() {
        let pending = fresh_pending();
        let rx = park(&pending, "tu_1");
        deliver_permission_response(
            &pending,
            "tu_1",
            PermissionOutcome::Selected {
                option_id: "allow_once".to_owned(),
                action: PermissionAction::Allow,
                notes_text: None,
                edited_input: None,
            },
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
            PermissionOutcome::Selected {
                option_id: "deny".to_owned(),
                action: PermissionAction::Deny,
                notes_text: None,
                edited_input: None,
            },
        );
        let decision = rx.blocking_recv().expect("oneshot resolved");
        assert!(!decision.is_allow());
    }

    #[test]
    fn permission_response_reject_once_drains_with_deny() {
        let pending = fresh_pending();
        let rx = park(&pending, "tu_r1");
        deliver_permission_response(
            &pending,
            "tu_r1",
            PermissionOutcome::Selected {
                option_id: "reject_once".to_owned(),
                action: PermissionAction::Deny,
                notes_text: None,
                edited_input: None,
            },
        );
        let decision = rx.blocking_recv().expect("oneshot resolved");
        assert!(!decision.is_allow());
    }

    #[test]
    fn permission_response_reject_always_drains_with_deny() {
        let pending = fresh_pending();
        let rx = park(&pending, "tu_r2");
        deliver_permission_response(
            &pending,
            "tu_r2",
            PermissionOutcome::Selected {
                option_id: "reject_always".to_owned(),
                action: PermissionAction::Deny,
                notes_text: None,
                edited_input: None,
            },
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
            PermissionOutcome::Selected {
                option_id: "allow_once".to_owned(),
                action: PermissionAction::Allow,
                notes_text: None,
                edited_input: None,
            },
        );
        assert!(pending.lock().is_empty());
    }

    fn fresh_pending_questions() -> PendingQuestions {
        Arc::new(Mutex::new(HashMap::new()))
    }

    fn park_question(
        pending: &PendingQuestions,
        id: &str,
    ) -> oneshot::Receiver<forge_primitives::QuestionOutcome> {
        let (tx, rx) = oneshot::channel();
        pending.lock().insert(id.to_owned(), tx);
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
        assert!(pending.lock().is_empty());
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
        // Empty suggestions + editable tool (Bash) -> [allow_once,
        // allow_with_edits, deny, tell_claude]. Shape assertion: first
        // is allow_once, last is tell_claude, deny present.
        let ids: Vec<&str> = request.options.iter().map(|o| o.option_id.as_str()).collect();
        assert_eq!(ids.first().copied(), Some("allow_once"));
        assert_eq!(ids.last().copied(), Some("tell_claude"));
        assert!(ids.contains(&"deny"));
        let display = request.display.expect("display populated");
        assert_eq!(display.title.as_deref(), Some("Run shell command"));
        assert_eq!(display.display_name.as_deref(), Some("Bash"));
        assert_eq!(display.description.as_deref(), Some("Lists directory entries"));
    }

    #[test]
    fn synth_permission_request_surfaces_decision_reason() {
        let c = ctx("Read", "tu_dr1", json!({ "file_path": "/tmp/x" })).with_display(
            None,
            Some("Path is outside allowed working directories".to_owned()),
            None,
            None,
            None,
        );
        let event = synth_permission_request("session-1", &c);
        let AgentEvent::PermissionRequest { request, .. } = event else {
            panic!("expected PermissionRequest");
        };
        let display = request.display.expect("display populated");
        assert_eq!(
            display.decision_reason.as_deref(),
            Some("Path is outside allowed working directories"),
        );
    }

    #[test]
    fn take_pending_removes_entry() {
        let pending = fresh_pending();
        let _rx = park(&pending, "tu_x");
        assert!(take_pending(&pending, "tu_x").is_some());
        assert!(take_pending(&pending, "tu_x").is_none());
    }
}

#[cfg(test)]
mod tests_permission_options {
    use super::{build_permission_options, dispatch_permission_action};
    use forge_primitives::options::PermissionMode;
    use forge_primitives::permission_ui::{PermissionAction, PermissionOptionKind};
    use forge_primitives::permissions::{
        PermissionBehavior, PermissionRuleValue, PermissionUpdate, PermissionUpdateDestination,
        ToolPermissionContext,
    };
    use serde_json::json;

    fn mk_ctx(tool_name: &str, suggestions: Vec<PermissionUpdate>) -> ToolPermissionContext {
        ToolPermissionContext::new(tool_name, json!({}), "tu-1", None).with_suggestions(suggestions)
    }

    #[test]
    fn empty_suggestions_yields_baseline_options() {
        // Non-editable tool (Read), no suggestions: Allow once, Deny, Tell Claude.
        let opts = build_permission_options(&mk_ctx("Read", vec![]));
        let ids: Vec<&str> = opts.iter().map(|o| o.option_id.as_str()).collect();
        assert_eq!(ids, vec!["allow_once", "deny", "tell_claude"]);
    }

    #[test]
    fn editable_tool_appends_allow_with_edits() {
        let opts = build_permission_options(&mk_ctx("Bash", vec![]));
        let ids: Vec<&str> = opts.iter().map(|o| o.option_id.as_str()).collect();
        assert_eq!(ids, vec!["allow_once", "allow_with_edits", "deny", "tell_claude"]);
        let edits = opts
            .iter()
            .find(|o| o.option_id == "allow_with_edits")
            .expect("allow_with_edits present for editable tool");
        assert_eq!(edits.kind, PermissionOptionKind::Edit);
        assert_eq!(edits.action, PermissionAction::AllowWithInput);
    }

    #[test]
    fn add_rules_suggestion_inserts_allow_always_option() {
        let suggestion = PermissionUpdate::AddRules {
            rules: vec![PermissionRuleValue {
                tool_name: "Read".into(),
                rule_content: Some("//tmp/**".into()),
            }],
            behavior: PermissionBehavior::Allow,
            destination: Some(PermissionUpdateDestination::Session),
        };
        let opts = build_permission_options(&mk_ctx("Read", vec![suggestion]));
        // Order: allow_once, allow_always_0 (rules), deny, tell_claude.
        let ids: Vec<&str> = opts.iter().map(|o| o.option_id.as_str()).collect();
        assert_eq!(ids, vec!["allow_once", "allow_always_0", "deny", "tell_claude"]);
        let allow_always =
            opts.iter().find(|o| o.option_id == "allow_always_0").expect("allow_always_0 present");
        assert_eq!(allow_always.kind, PermissionOptionKind::Allow);
        assert!(matches!(allow_always.action, PermissionAction::AllowWithUpdates { .. }));
    }

    #[test]
    fn macos_tmp_mirror_suggestions_collapse_for_display_but_keep_both_rules() {
        let suggestion_a = PermissionUpdate::AddRules {
            rules: vec![PermissionRuleValue {
                tool_name: "Read".into(),
                rule_content: Some("//tmp/**".into()),
            }],
            behavior: PermissionBehavior::Allow,
            destination: Some(PermissionUpdateDestination::Session),
        };
        let suggestion_b = PermissionUpdate::AddRules {
            rules: vec![PermissionRuleValue {
                tool_name: "Read".into(),
                rule_content: Some("//private/tmp/**".into()),
            }],
            behavior: PermissionBehavior::Allow,
            destination: Some(PermissionUpdateDestination::Session),
        };
        let opts = build_permission_options(&mk_ctx("Read", vec![suggestion_a, suggestion_b]));
        // ONE "Allow always" option whose action carries BOTH rules — not
        // two separate "Allow always" entries.
        let allow_always_count =
            opts.iter().filter(|o| o.option_id.starts_with("allow_always_")).count();
        assert_eq!(
            allow_always_count, 1,
            "macOS /tmp + /private/tmp should collapse into one option"
        );
        let merged = opts
            .iter()
            .find(|o| o.option_id.starts_with("allow_always_"))
            .expect("merged allow_always option present");
        let PermissionAction::AllowWithUpdates { updates } = &merged.action else {
            panic!("expected AllowWithUpdates");
        };
        // Both rules should be present in the merged update — count rule
        // entries across all updates.
        let total_rules: usize = updates
            .iter()
            .map(|u| match u {
                PermissionUpdate::AddRules { rules, .. } => rules.len(),
                _ => 0,
            })
            .sum();
        assert_eq!(total_rules, 2, "merged action should carry both /tmp and /private/tmp rules");
    }

    #[test]
    fn add_directories_suggestion_yields_allow_always_option() {
        let suggestion = PermissionUpdate::AddDirectories {
            directories: vec!["/tmp".into(), "/private/tmp".into()],
            destination: Some(PermissionUpdateDestination::Session),
        };
        let opts = build_permission_options(&mk_ctx("Write", vec![suggestion]));
        let ids: Vec<&str> = opts.iter().map(|o| o.option_id.as_str()).collect();
        // Write is editable so allow_with_edits also appears.
        assert!(ids.contains(&"allow_always_0"));
        assert!(ids.contains(&"allow_with_edits"));
    }

    #[test]
    fn set_mode_suggestion_yields_switch_mode_option() {
        let suggestion = PermissionUpdate::SetMode {
            mode: PermissionMode::AcceptEdits,
            destination: Some(PermissionUpdateDestination::Session),
        };
        let opts = build_permission_options(&mk_ctx("Write", vec![suggestion]));
        let switch_mode =
            opts.iter().find(|o| o.option_id == "allow_always_0").expect("allow_always_0 present");
        assert!(switch_mode.name.to_lowercase().contains("accept edits"));
    }

    #[test]
    fn dispatch_allow_action_yields_allow_decision() {
        let decision = dispatch_permission_action(PermissionAction::Allow, "", None);
        assert!(decision.is_allow());
    }

    #[test]
    fn dispatch_deny_with_notes_passes_notes_as_reason() {
        let decision =
            dispatch_permission_action(PermissionAction::Deny, "use --dry-run first", None);
        let reason = decision.reason().expect("deny carries reason");
        assert_eq!(reason, "use --dry-run first");
    }

    #[test]
    fn dispatch_deny_with_empty_notes_uses_default_reason() {
        let decision = dispatch_permission_action(PermissionAction::Deny, "", None);
        let reason = decision.reason().expect("deny carries reason");
        assert_eq!(reason, "Denied by user");
    }

    #[test]
    fn dispatch_allow_with_updates_attaches_them() {
        let updates =
            vec![PermissionUpdate::SetMode { mode: PermissionMode::Auto, destination: None }];
        let decision = dispatch_permission_action(
            PermissionAction::AllowWithUpdates { updates: updates.clone() },
            "",
            None,
        );
        assert_eq!(decision.updated_permissions(), updates.as_slice());
    }

    #[test]
    fn dispatch_allow_with_input_uses_edited_value() {
        let edited = json!({"command": "echo modified"});
        let decision =
            dispatch_permission_action(PermissionAction::AllowWithInput, "", Some(edited.clone()));
        let updated_input = decision.updated_input().expect("allow_with_input carries value");
        assert_eq!(updated_input, &edited);
    }
}
