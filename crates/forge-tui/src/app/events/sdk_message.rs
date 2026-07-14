//! Direct `forge_primitives::Message` consumer for the App.
//!
//! [`handle_sdk_message`] is the App-side dispatcher that receives
//! raw `forge_primitives::Message` envelopes from the bridge worker
//! and routes them to per-variant handlers below. Each handler
//! destructures the typed message variant directly and mutates App
//! state. `Message::System { data: Value, .. }` is the one variant
//! that still walks JSON - its subtype shapes aren't first-class on
//! the typed envelope, so per-subtype handlers branch on
//! narrow `.get()` lookups against `data`.
//!
//! # Clippy allows
//!
//! - `needless_pass_by_value`: handlers take `msg: Message` by value
//!   so they can destructure ownership-tracked fields without copying.
//! - `doc_markdown`: keeps prose like `forge_primitives::Message` in
//!   doc comments without backticks tripping the lint.
#![allow(clippy::needless_pass_by_value)]

use forge_primitives::Message;
use serde_json::Value;

use crate::app::App;
use forge_workspace::translate::state_parsing::{
    build_api_retry_update, build_rate_limit_update, normalize_settings_parse_errors,
    parse_runtime_session_state,
};

/// Top-level entry point. Called from `events::client` after the
/// session-id check on `SessionUpdate::ChatAppended`. Dispatches
/// to per-variant handlers below.
pub(super) fn handle_sdk_message(app: &mut App, msg: Message) {
    match msg {
        Message::Assistant { .. } => handle_assistant(app, msg),
        Message::User { .. } => handle_user(app, msg),
        Message::System { .. } => handle_system(app, msg),
        Message::TaskStarted { .. } => handle_task_started(app, msg),
        Message::TaskUpdated { .. } => handle_task_updated(app, msg),
        Message::TaskProgress { .. } => handle_task_progress(app, msg),
        Message::TaskNotification { .. } => handle_task_notification(app, msg),
        Message::RateLimitEvent { .. } => handle_rate_limit_event(app, msg),
        Message::Result { .. } => handle_result(app, msg),
        Message::BackgroundTasksChanged { .. } => handle_background_tasks_changed(app, msg),
        Message::CommandsChanged { .. } => handle_commands_changed(app, msg),
        // The CLI's last-gasp fatal transport error before teardown.
        // Route it through the turn-error path so the session leaves
        // the pinned-spinner state, in-flight tool calls finalize as
        // Failed, and the CLI's error string surfaces.
        Message::Error { error } => {
            if let Some(key) = app.active_session_key.clone() {
                super::turn::handle_turn_error_event(app, &key, &error, None, None);
            } else {
                tracing::warn!(
                    target: crate::logging::targets::APP_SESSION,
                    error = %error,
                    "fatal Message::Error with no active session to attribute it to",
                );
            }
        }
        // No-op arms:
        // - `StreamEvent` (partial-message streaming) + `Unknown`
        //   (forward-compat) - re-add a handler if a downstream
        //   consumer needs to react.
        // - `TurnDuration` (#279): decoder variant retained for
        //   wire-conformance (Hard Rule #9). CLI 2.1.156 never emits
        //   the event in forge's flow (30+ scenario baselines + 14
        //   fresh captures, all zero), so the prior banner chip +
        //   per-message stamp + per-session cache were dead code and
        //   got deleted.
        // - 2.1.204 `hook_started` / `hook_response`: typed for
        //   wire-conformance; no UI surface yet (hook-activity is a
        //   separate feature).
        Message::StreamEvent { .. }
        | Message::Unknown { .. }
        | Message::TurnDuration { .. }
        | Message::HookStarted { .. }
        | Message::HookResponse { .. } => {}
        // #273: typed wrappers around the CLI 2.1.156 system events.
        Message::ThinkingTokens { estimated_tokens, .. } => {
            handle_thinking_tokens(app, estimated_tokens);
        }
        Message::StopHookSummary { actions, hook_infos, .. } => {
            handle_stop_hook_summary(app, actions, hook_infos);
        }
    }
}

/// #273: Set the active session's latest thinking-token count.
/// The renderer reads it via `App::latest_thinking_tokens` to format
/// the spinner chip `⠋ thinking · N tok`. Repeated events overwrite;
/// the field is cleared on turn end (in `handle_result`).
fn handle_thinking_tokens(app: &mut App, estimated_tokens: u64) {
    app.set_latest_thinking_tokens(Some(estimated_tokens));
}

/// #273: Capture the stop-hook summary for the active assistant
/// message. The renderer surfaces a collapsed 1-liner
/// `↳ hook summary · N actions [▶ expand]` when `actions > 0`;
/// hidden entirely when `actions == 0`.
fn handle_stop_hook_summary(
    app: &mut App,
    actions: u32,
    hook_infos: Vec<forge_primitives::StopHookInfo>,
) {
    let Some(message_idx) = app.active_turn_assistant_idx() else {
        return;
    };
    let hooks: Vec<crate::app::state::types::StopHookEntry> = hook_infos
        .into_iter()
        .map(|h| crate::app::state::types::StopHookEntry {
            command: h.command,
            duration_ms: h.duration_ms,
        })
        .collect();
    app.set_last_stop_hook_summary(Some(crate::app::state::types::StopHookSummaryState {
        message_idx,
        actions,
        hooks,
    }));
}

/// Apply a typed `forge_primitives::FastModeState` to App state.
/// Idempotent - same state in re-applies as a no-op.
///
/// Converts the wire-side `forge_primitives::FastModeState` to the
/// App-side `model::FastModeState`; an unrecognised wire state
/// collapses to `Off` (no fast-mode badge).
fn apply_fast_mode_state(app: &mut App, wire_state: forge_primitives::FastModeState) {
    use crate::agent::model::FastModeState as Model;
    use forge_primitives::FastModeState as Wire;
    let model_state = match wire_state {
        Wire::Off | Wire::Unknown => Model::Off,
        Wire::Cooldown => Model::Cooldown,
        Wire::On => Model::On,
    };
    if app.fast_mode_state() == model_state {
        return;
    }
    app.set_fast_mode_state(model_state);
}

/// `apply_fast_mode_state` adapter for the System("status") path,
/// which still reads from a `Value` payload (the `data` field on the
/// generic system envelope). Drops silently when the field is absent
/// or doesn't deserialize to a known variant.
fn apply_fast_mode_state_from_value(app: &mut App, data: &Value) {
    let Some(wire_state) = forge_workspace::translate::state_parsing::parse_fast_mode_state(
        data.get("fast_mode_state"),
    ) else {
        return;
    };
    apply_fast_mode_state(app, wire_state);
}

// Per-variant handlers. Each takes ownership of the full `Message`
// so it can destructure ownership-tracked fields freely.

fn handle_assistant(app: &mut App, msg: Message) {
    let Message::Assistant { message, parent_tool_use_id, error, .. } = msg else {
        return;
    };
    // Lifecycle: any *live* assistant message arrival means this
    // bucket has a turn in flight. Set lifecycle = Running so the
    // Projects pane row spins. Covers the case where the user
    // clicks a project whose turn was ALREADY in flight when they
    // clicked (e.g. auto_start session received its first
    // continuation from claude before the user looked at it) -
    // input_submit's lifecycle write only fires on user-typed
    // prompts, so without this hook the bucket would stay Idle
    // even while assistant content streams in. Active or
    // pivoted-background routing both end up here with the right
    // bucket as `active_session_key` (see
    // `apply_sdk_message_presentation`'s `with_pivoted` scope).
    //
    // The `replay_in_progress` gate: `load_resume_history` walks
    // on-disk history through this same dispatcher so content
    // blocks / tool_use / todos land via shared code. Replay
    // messages are historical, not live wire content, so they
    // must not flip lifecycle to Running - otherwise an
    // auto_start session's resume leaves the bucket pinned on
    // Running with no balancing Result to flip it back, and the
    // Projects pane spinner sticks until the first real turn
    // completes.
    if !app.replay_in_progress
        && let Some(key) = app.active_session_key.clone()
    {
        super::set_bucket_lifecycle_state(
            app,
            &key,
            crate::app::session::SessionLifecycleState::Running,
        );
    }
    // Per-turn model observation. The CLI's `system/init` carries the
    // resolved model id once per session; every subsequent Assistant
    // envelope re-states the model at `message.model`. Tracking the
    // most recent observed model lets the App verify that the chip
    // matches what the CLI is actually using on each turn.
    if !message.model.is_empty() {
        app.set_observed_assistant_model(Some(message.model.clone()));
    }
    // Outer-envelope error capture - `app.turn_state.last_assistant_error`
    // is consulted by `apply_result_finalize` to classify TurnError
    // variants.
    if let Some(err) = error {
        let err_str = match err {
            forge_primitives::AssistantMessageError::AuthenticationFailed => {
                "authentication_failed"
            }
            forge_primitives::AssistantMessageError::BillingError => "billing_error",
            forge_primitives::AssistantMessageError::RateLimit => "rate_limit",
            forge_primitives::AssistantMessageError::InvalidRequest => "invalid_request",
            forge_primitives::AssistantMessageError::ServerError => "server_error",
            forge_primitives::AssistantMessageError::Unknown => "unknown",
        };
        let _: () =
            app.with_turn_state_mut(|ts| ts.last_assistant_error = Some(err_str.to_owned()));
    }
    walk_assistant_content(app, &message.content, parent_tool_use_id.as_deref());
}

/// Walk the typed `Message::Assistant` content blocks, applying text,
/// thinking, tool_use, and tool_result blocks to App state directly.
fn walk_assistant_content(
    app: &mut App,
    content: &[forge_primitives::ContentBlock],
    parent_tool_use_id: Option<&str>,
) {
    use crate::agent::model;
    use forge_primitives::ContentBlock;

    // A subagent message (parent_tool_use_id set) belongs in the
    // SUBAGENTS inspector, so its narration/thinking must not leak into
    // the main chat (2.1.204 local agents stream it into the parent wire).
    let is_subagent = parent_tool_use_id.is_some_and(|parent| !parent.trim().is_empty());

    for block in content {
        match block {
            ContentBlock::Text { text } => {
                if is_subagent || text.is_empty() {
                    continue;
                }
                super::clear_compaction_state(app, true);
                let chunk = model::ContentChunk::new(model::ContentBlock::Text(
                    model::TextContent::new(text.clone()),
                ));
                super::streaming::handle_agent_message_chunk(app, chunk);
            }
            ContentBlock::Thinking { thinking, .. } => {
                if is_subagent || thinking.is_empty() {
                    continue;
                }
                let chunk_chars = thinking.chars().count();
                tracing::trace!(
                    target: crate::logging::targets::APP_SESSION,
                    event_name = "agent_thought_chunk_applied",
                    message = "agent thought chunk applied",
                    outcome = "success",
                    chunk_chars,
                );
                app.status = crate::app::AppStatus::Thinking;
            }
            ContentBlock::ToolUse { id, name, input }
            | ContentBlock::ServerToolUse { id, name, input } => {
                if id.is_empty() {
                    continue;
                }
                apply_tool_use_block(app, id, name, input, parent_tool_use_id);
            }
            ContentBlock::ToolResult { tool_use_id, content, is_error } => {
                if tool_use_id.is_empty() {
                    continue;
                }
                let raw_block = serde_json::to_value(block).map_err(|err| { tracing::warn!(target: "forge_tui::sdk_message", error = %err, "ContentBlock failed to serialize to Value"); err }).ok();
                apply_tool_result_block(
                    app,
                    tool_use_id,
                    *is_error,
                    Some(content),
                    raw_block.as_ref(),
                );
            }
            ContentBlock::ServerToolResult { tool_use_id, content } => {
                // `advisor_tool_result` lands as the typed enum
                // variant (per `ContentBlock::from_raw_block`'s match).
                // Without an arm here it gets dropped; the server-tool
                // card then renders without its result. The other
                // server-tool result wire types
                // (`tool_search_tool_result`, `web_search_tool_result`,
                // `web_fetch_tool_result`) deserialise to `Unknown` and
                // travel through the `is_tool_result_block_type` path
                // below. `ServerToolResult` carries no `is_error` flag;
                // failure is encoded inside `content`.
                if tool_use_id.is_empty() {
                    continue;
                }
                let raw_block = serde_json::to_value(block).map_err(|err| { tracing::warn!(target: "forge_tui::sdk_message", error = %err, "ContentBlock failed to serialize to Value"); err }).ok();
                apply_tool_result_block(app, tool_use_id, false, Some(content), raw_block.as_ref());
            }
            ContentBlock::Unknown { type_str, raw }
                if forge_workspace::tooling::is_tool_result_block_type(type_str) =>
            {
                // Wire-side tool-result variants the typed enum
                // doesn't enumerate: `mcp_tool_result`,
                // `web_fetch_tool_result`, etc. (full set lives in the
                // tooling module's `TOOL_RESULT_TYPES`). They share the
                // `tool_use_id` + `content` + `is_error` shape - pull
                // those off the raw value.
                let Some(record) = raw.as_object() else { continue };
                let Some(tool_use_id) =
                    record.get("tool_use_id").and_then(Value::as_str).filter(|s| !s.is_empty())
                else {
                    continue;
                };
                let is_error = record.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                let raw_content = record.get("content");
                apply_tool_result_block(app, tool_use_id, is_error, raw_content, Some(raw));
            }
            ContentBlock::QueuedCommand { prompt, .. } => {
                // Symmetric coverage with the user-content walker -
                // claude *might* embed `queued_command` blocks on an
                // assistant content array as well as the user side.
                // Edge case (we've only seen them on the user side
                // in JSONL captures), but the walker pair is the
                // safer placement.
                let prompt_text = extract_queued_command_text(prompt);
                handle_queued_command_echo(app, &prompt_text);
            }
            _ => {}
        }
    }
}

fn handle_user(app: &mut App, msg: Message) {
    let Message::User { message, parent_tool_use_id, tool_use_result, .. } = msg else {
        return;
    };
    // a genuine new user turn invalidates the previous
    // turn's thinking-token tally. Without this clear, a multi-tool-call
    // turn that ends without a `Result` (in-flight when the next user
    // turn lands) leaves `latest_thinking_tokens` holding the prior
    // cumulative value (e.g. 150); the first `thinking_tokens` event of
    // the new turn arrives with a reset cumulative (50) and the chip
    // drops 150 -> 50, which reads as the chip "going up and down".
    // Tool-result echoes (`tool_use_result.is_some()`) are mid-turn
    // continuations of the assistant's tool-call loop, not new user
    // turns - leave the count alone there. The existing Result-side
    // clear in `handle_result` covers the clean turn-end case; this
    // user-side clear is additive for the in-flight case.
    if tool_use_result.is_none() {
        app.set_latest_thinking_tokens(None);
    }
    walk_user_tool_results(app, &message.content, tool_use_result.as_ref());
    // Peer-coordination user-turn echoes (#114). `Command::Prompt`
    // dispatched via `Workspace::deliver_peer_prompt` injects the
    // wrapped envelope into the target session's CLI as a user turn.
    // The CLI echoes it back as `Message::User`, but the input-submit
    // local-push convention (the typed-user case already pushed the
    // bubble) doesn't apply here - no local typing happened, so the
    // chat buffer never sees the user-turn unless we push it from
    // the SDK echo. Detected via the peer-wrapper prefix so non-peer
    // user echoes (the existing pattern) keep their no-push behavior.
    push_peer_envelope_user_turn_if_present(app, &message.content);
    // Sub-agent tool_use_result envelopes carry parent_tool_use_id at
    // the message level - wire the implicit parent linkage so the
    // tool_call lifecycle picks up sub-agent results correctly.
    if let Some(result) = tool_use_result.as_ref()
        && let Some(tool_use_id) = parent_tool_use_id.as_deref()
        && !tool_use_id.is_empty()
    {
        let parsed = forge_workspace::tooling::unwrap_tool_use_result(result);
        apply_tool_result_block(
            app,
            tool_use_id,
            parsed.is_error,
            Some(&parsed.content),
            Some(result),
        );
    }
}

/// Push a peer-wrapper-prefixed user turn into the chat buffer.
///
/// The detection key is `peer_block::detect_inbound` - same matcher
/// the renderer uses, so any envelope shape recognised at render time
/// is also pushed here. Falls through silently for plain user echoes
/// (the dominant case) so we don't double-push the locally-pushed
/// bubble.
fn push_peer_envelope_user_turn_if_present(
    app: &mut App,
    content: &[forge_primitives::ContentBlock],
) {
    use crate::app::{ChatMessage, MessageBlock, MessageRole, TextBlock};
    use forge_primitives::ContentBlock;

    for block in content {
        let ContentBlock::Text { text } = block else {
            continue;
        };
        let Some(kind) = crate::ui::peer_block::detect_inbound(text) else {
            continue;
        };
        let blocks = vec![MessageBlock::Text(TextBlock::from_complete(text))];
        // #143 item 2: cache the envelope flag at push time so the chat
        // renderer doesn't walk text blocks every frame. Gotify + cron each
        // stamp a distinct flag (drives their distinct role label); every
        // other envelope shape is a peer envelope.
        let msg = match kind {
            crate::ui::peer_block::PeerInboundKind::Gotify { .. } => {
                ChatMessage::new_gotify_envelope(MessageRole::User, blocks, None)
            }
            crate::ui::peer_block::PeerInboundKind::Cron { .. } => {
                ChatMessage::new_cron_envelope(MessageRole::User, blocks, None)
            }
            _ => ChatMessage::new_peer_envelope(MessageRole::User, blocks, None),
        };
        // Replay reconstructs the chat bubble only - no live turn
        // ceremony. load_resume_history walks historical envelopes through
        // this dispatcher and has no balancing Result to clear a Running
        // flip or a freshly-opened placeholder (the stuck-spinner failure
        // mode), matching handle_assistant's replay gate.
        if app.replay_in_progress {
            app.push_message_tracked(msg);
            app.enforce_history_retention_tracked();
            return;
        }
        // Shares dispatch_prompt's turn-open (strip a stranded placeholder,
        // append the user turn, open a fresh tail placeholder + reparent
        // the spinner) but deliberately skips its auto-scroll, so a
        // delivered turn does not yank a scrolled-up reader.
        app.strip_trailing_empty_assistant_placeholder();
        app.push_message_tracked(msg);
        app.push_active_turn_assistant_placeholder();
        app.status = crate::app::AppStatus::Thinking;
        if let Some(key) = app.active_session_key.clone() {
            super::set_bucket_lifecycle_state(
                app,
                &key,
                crate::app::session::SessionLifecycleState::Running,
            );
        }
        app.enforce_history_retention_tracked();
        return;
    }
}

/// Walk the typed `Message::User` content blocks and apply
/// tool_result blocks via `apply_tool_result_block`. `tool_use_result`
/// is the outer envelope from `Message::User` (the CLI attaches a
/// typed payload alongside the inner content blocks); some tools
/// stamp post-result state from it (e.g. `CronCreate`'s job id at
/// `tool_use_result.id`).
fn walk_user_tool_results(
    app: &mut App,
    content: &[forge_primitives::ContentBlock],
    tool_use_result: Option<&Value>,
) {
    use forge_primitives::ContentBlock;

    for block in content {
        match block {
            ContentBlock::ToolResult { tool_use_id, content, is_error } => {
                if tool_use_id.is_empty() {
                    continue;
                }
                stamp_cron_job_id_if_applicable(app, tool_use_id, tool_use_result);
                let raw_block = serde_json::to_value(block).map_err(|err| { tracing::warn!(target: "forge_tui::sdk_message", error = %err, "ContentBlock failed to serialize to Value"); err }).ok();
                apply_tool_result_block(
                    app,
                    tool_use_id,
                    *is_error,
                    Some(content),
                    raw_block.as_ref(),
                );
            }
            ContentBlock::Unknown { type_str, raw }
                if forge_workspace::tooling::is_tool_result_block_type(type_str) =>
            {
                // Same fallback as `walk_assistant_content` - wire
                // tool-result variants outside the typed enum.
                let Some(record) = raw.as_object() else { continue };
                let Some(tool_use_id) =
                    record.get("tool_use_id").and_then(Value::as_str).filter(|s| !s.is_empty())
                else {
                    continue;
                };
                let is_error = record.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                let raw_content = record.get("content");
                apply_tool_result_block(app, tool_use_id, is_error, raw_content, Some(raw));
            }
            ContentBlock::QueuedCommand { prompt, .. } => {
                // Claude bundled a user-typed-while-busy message as
                // a `queued_command` content block - match against
                // a pending dimmed bubble (live) or push a fresh
                // user bubble (replay).
                let prompt_text = extract_queued_command_text(prompt);
                handle_queued_command_echo(app, &prompt_text);
            }
            _ => {}
        }
    }
}

/// Extract a renderable text string from a `queued_command` block's
/// `prompt` field. Wire shape for `prompt` is `Value` so it could be
/// a plain string, OR a content-block array for multi-modal inputs
/// (e.g. text + image). For the latter, walk the inner blocks and
/// concatenate the text content. Image/document blocks render as
/// `[image]` / `[document]` placeholders so the user sees something
/// rather than blank.
///
/// This is invoked twice - once for the user-content walker (live
/// mid-turn / replay), once for the assistant-content walker (edge
/// case).
pub(super) fn extract_queued_command_text(prompt: &Value) -> String {
    if let Some(s) = prompt.as_str() {
        return s.to_owned();
    }
    let Some(blocks) = prompt.as_array() else {
        // Object or other - render as JSON literal so the user can
        // see SOMETHING. Should never hit in practice.
        return serde_json::to_string(prompt).unwrap_or_else(|_| String::from("[unrenderable]"));
    };
    let mut parts = Vec::new();
    for block in blocks {
        let Some(obj) = block.as_object() else { continue };
        match obj.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = obj.get("text").and_then(Value::as_str) {
                    parts.push(t.to_owned());
                }
            }
            Some("image") => parts.push(String::from("[image]")),
            Some("document") => parts.push(String::from("[document]")),
            Some(other) => parts.push(format!("[{other}]")),
            None => {}
        }
    }
    parts.join("\n")
}

/// Process a `queued_command` content-block.
///
/// **Reachability**: claude does NOT emit `queued_command` on
/// stream-json stdout - it only persists those messages to the
/// session JSONL as `type:"attachment"` rows. The replay scanner in
/// `forge_agent::userdata::catalog::scan` hoists those rows into
/// synthetic user envelopes carrying a single `queued_command`
/// content block each. So in practice this walker only runs during
/// session resume; live mid-turn submits never hit it.
///
/// Action: push a regular user bubble. (Live mid-turn submits
/// already pushed their own bubble at submit time - see
/// `input_submit::dispatch_prompt` - and never reach this code.)
pub(super) fn handle_queued_command_echo(app: &mut App, prompt_text: &str) {
    use crate::app::{ChatMessage, MessageBlock, MessageRole, TextBlock};
    // Harness-injected `<task-notification>` blobs (background-task
    // completion events) get queued through the same path as
    // user-typed input. They're plumbing, not a user message - render
    // them as user bubbles is misleading on resume (the bottom of the
    // chat fills up with notification chatter that looks like the user
    // typed it). Skip them at the echo path so they don't reach the
    // chat buffer at all. Mirrors the `<local-command-caveat>` /
    // `<command-name>` filter in `events::session_reset` for the
    // user-text path.
    if prompt_text.trim_start().starts_with("<task-notification>") {
        return;
    }
    let blocks = vec![MessageBlock::Text(TextBlock::from_complete(prompt_text))];
    app.push_message_tracked(ChatMessage::new(MessageRole::User, blocks, None));
    app.enforce_history_retention_tracked();
    tracing::debug!(
        target: crate::logging::targets::APP_INPUT,
        event_name = "queued_command_replayed",
        message = "pushed user bubble for replayed queued_command (session resume path)",
        outcome = "success",
        prompt_chars = prompt_text.chars().count(),
    );
}

/// Reads `meta.claudeCode.parentToolUseId` from a tool_call's meta
/// blob.
fn parent_tool_use_id_from_meta(meta: Option<&Value>) -> Option<String> {
    let claude_code = meta?.get("claudeCode")?.as_object()?;
    let id = claude_code.get("parentToolUseId")?.as_str()?;
    if id.is_empty() { None } else { Some(id.to_owned()) }
}

/// Insert or update an entry in `app.turn_state.tool_calls` for a
/// `tool_use` content block, and dispatch the resulting initial
/// `ToolCall` or `ToolCallUpdate` via the existing App handlers.
fn apply_tool_use_block(
    app: &mut App,
    tool_use_id: &str,
    name: &str,
    input: &Value,
    parent_tool_use_id: Option<&str>,
) {
    use crate::app::connect::type_converters::convert_tool_call;
    use forge_primitives::ToolCallUpdateFields;
    use forge_workspace::tooling::create_tool_call;

    let existing = app.with_turn_state(|ts| ts.tool_calls.get(tool_use_id).cloned());
    let resolved_parent = parent_tool_use_id
        .map(str::to_owned)
        .or_else(|| parent_tool_use_id_from_meta(existing.as_ref().and_then(|e| e.meta.as_ref())));
    let mut tool_call = create_tool_call(tool_use_id, name, input, resolved_parent.as_deref());
    tool_call.status = forge_primitives::ToolCallStatus::InProgress;

    if existing.is_none() {
        let tc = tool_call.clone();
        let _: () = app.with_turn_state_mut(|ts| {
            ts.tool_calls.insert(tool_use_id.to_owned(), tc);
        });
        let model_tc = convert_tool_call(tool_call);
        super::tool_calls::handle_tool_call(app, model_tc);
        return;
    }

    let mut fields = ToolCallUpdateFields {
        title: Some(tool_call.title.clone()),
        kind: Some(tool_call.kind),
        status: Some(forge_primitives::ToolCallStatus::InProgress),
        raw_input: tool_call.raw_input.clone(),
        locations: Some(tool_call.locations.clone()),
        meta: tool_call.meta.clone(),
        ..Default::default()
    };
    if !tool_call.content.is_empty() {
        fields.content = Some(tool_call.content.clone());
    }
    apply_tool_call_update(app, tool_use_id, fields);
}

/// Apply a tool_result content block to App state. Looks up the
/// tool_call in `app.turn_state.tool_calls`, builds result fields
/// via `agent::tooling::build_tool_result_fields`, and dispatches a
/// `ToolCallUpdate` through the existing App handler.
fn apply_tool_result_block(
    app: &mut App,
    tool_use_id: &str,
    is_error: bool,
    raw_content: Option<&Value>,
    raw_block: Option<&Value>,
) {
    use forge_workspace::tooling::build_tool_result_fields;

    let base = app.with_turn_state(|ts| ts.tool_calls.get(tool_use_id).cloned());
    let fields = build_tool_result_fields(is_error, raw_content, base.as_ref(), raw_block);
    apply_tool_call_update(app, tool_use_id, fields);
}

/// Stamp the cron job id onto the matching SCHEDULES entry when a
/// `CronCreate` tool_use_result arrives. The CLI's result shape can
/// be a bare-string id OR a `{"id": "..."}` object - cover both.
/// No-op when the tool_use_id doesn't match a CronCreate call.
///
/// Identifies the call via `meta.claudeCode.toolName` (the SDK
/// canonical name) instead of the wire-level `title`. The wire
/// `title` is the formatter output from `forge_agent::tooling::tool_title`
/// and would silently diverge from `"CronCreate"` if a future per-tool
/// branch lands there (e.g. `"Cron */5 * * * *"`); the canonical
/// name from meta stays stable.
fn stamp_cron_job_id_if_applicable(
    app: &mut App,
    tool_use_id: &str,
    tool_use_result: Option<&Value>,
) {
    let is_cron_create = app.with_turn_state(|ts| {
        ts.tool_calls.get(tool_use_id).is_some_and(|tc| {
            super::tool_calls::sdk_tool_name_from_meta(tc.meta.as_ref()) == Some("CronCreate")
        })
    });
    if !is_cron_create {
        return;
    }
    // The canonical job id is `tool_use_result.id` (the envelope).
    // The inner content text contains the id as a substring
    // ("Scheduled recurring job <id> ..."), so reading from it stamps
    // the wrong string and CronDelete's `remove_cron_by_id` never
    // matches - the SCHEDULES entry persists as a phantom past its
    // delete (#302).
    let Some(job_id) = tool_use_result.and_then(|env| env.get("id")).and_then(Value::as_str) else {
        return;
    };
    app.stamp_cron_id_from_result(tool_use_id, job_id);
}

/// Mutate `app.turn_state.tool_calls` with the supplied update
/// fields, then dispatch a `ToolCallUpdate` via the existing App
/// handler.
fn apply_tool_call_update(
    app: &mut App,
    tool_use_id: &str,
    fields: forge_primitives::ToolCallUpdateFields,
) {
    use crate::app::connect::type_converters::convert_tool_call_update;
    use forge_primitives::ToolCallUpdate;

    let merge_fields = fields.clone();
    let _: () = app.with_turn_state_mut(|ts| {
        if let Some(base) = ts.tool_calls.get_mut(tool_use_id) {
            base.merge(merge_fields);
        }
    });
    let wire_update = ToolCallUpdate { tool_call_id: tool_use_id.to_owned(), fields };
    let model_update = convert_tool_call_update(wire_update);
    super::tool_updates::handle_tool_call_update_session(app, &model_update);
}

/// Walk `app.turn_state.tool_calls` and emit a terminal status
/// update for every still-pending entry. Called from
/// `apply_result_finalize` when a turn ends.
///
/// Persistent `Monitor` calls (those launched with
/// `raw_input.persistent == true`) are deliberately skipped: by
/// design they outlive the turn that started them and are only
/// terminated via `TaskStop` / explicit `KillBash`. Sweeping them
/// to `Completed` here would visually mark a still-running monitor
/// as done in both the chat-stream card and the Inspector PROCESSES
/// section.
///
/// Backgrounded tasks still in the session roster are skipped for the same
/// reason: a `run_in_background` Bash or Task/Agent root outlives its
/// spawning turn, so its card must not flip terminal until `task_updated`.
fn finalize_open_tool_calls(app: &mut App, status: forge_primitives::ToolCallStatus) {
    use crate::app::state::tool_call_info::is_monitor_tool_name;
    use forge_primitives::{ToolCallStatus, ToolCallUpdateFields};

    // Backgrounded tasks the CLI still lists as running outlive the turn;
    // their tool_use_ids resolve from the session roster so the sweep leaves
    // them alone (their terminal status arrives later via `task_updated`).
    let backgrounded_alive: std::collections::HashSet<String> = app
        .active_session()
        .map(|session| {
            session.backgrounded_alive_tool_use_ids().into_iter().map(str::to_owned).collect()
        })
        .unwrap_or_default();
    let pending: Vec<String> = app.with_turn_state(|ts| {
        ts.tool_calls
            .iter()
            .filter(|(_, t)| {
                matches!(t.status, ToolCallStatus::Pending | ToolCallStatus::InProgress)
            })
            .filter(|(id, t)| {
                // Skip explicit persistent monitors - the docs and
                // wire shape both say these outlive the turn that
                // started them.
                if raw_input_is_persistent(t.raw_input.as_ref()) {
                    return false;
                }
                // Defensive: when raw_input is None for a Monitor-named
                // tool, we can't tell if it's persistent yet - the
                // tool_use's input block may not have decoded by the
                // time the turn ends. Treat as "could be persistent"
                // and skip finalization rather than risk flipping a
                // still-running watch to Completed. Falls through to
                // the normal sweep on the next turn boundary once
                // raw_input has arrived.
                // `ToolCall` in TurnState uses `title` as the
                // SDK-supplied tool name (e.g. `"Monitor"`,
                // `"Bash"`). The renderer-side `ToolCallInfo` carries
                // a separate `sdk_tool_name`, but it's not on the
                // wire-level struct stored here.
                if t.raw_input.is_none() && is_monitor_tool_name(&t.title) {
                    return false;
                }
                // Still-running backgrounded work (bash / Task root) settles
                // via its own task_updated, not the turn boundary.
                if backgrounded_alive.contains(id.as_str()) {
                    return false;
                }
                true
            })
            .map(|(id, _)| id.clone())
            .collect()
    });
    for id in pending {
        apply_tool_call_update(
            app,
            &id,
            ToolCallUpdateFields { status: Some(status), ..Default::default() },
        );
    }
}

/// True when `raw_input` is `Some` AND carries `"persistent": true`
/// at the top level. The `is_some` guard matters: an absent
/// `raw_input` (tool_use observed but body not yet applied) MUST NOT
/// be treated as non-persistent - that would race against the
/// out-of-order arrival of the tool_use content block and let a
/// persistent monitor flip to `Completed` before its inputs land.
fn raw_input_is_persistent(raw_input: Option<&Value>) -> bool {
    raw_input
        .and_then(|v| v.as_object())
        .and_then(|obj| obj.get("persistent"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn handle_system(app: &mut App, msg: Message) {
    let Message::System { subtype, data, .. } = msg else { return };
    match subtype.as_str() {
        "status" => {
            apply_fast_mode_state_from_value(app, &data);
            // permissionMode → CurrentModeUpdate + typed turn_state.mode
            // mirror + supported_mode_ids recompute. The App-side
            // `apply_current_mode_update` only touches the display
            // struct, so we still need to update `turn_state.mode`
            // and refresh `supported_mode_ids` ourselves - otherwise
            // the typed mode the `/mode` picker reads goes stale on
            // server-side mode switches.
            if let Some(mode_str) = data.get("permissionMode").and_then(Value::as_str) {
                super::apply_current_mode_update(
                    app,
                    &crate::agent::model::CurrentModeUpdate::new(mode_str),
                );
                if let Some(parsed) = forge_workspace::PermissionMode::from_wire(mode_str) {
                    use forge_workspace::commands::supported_mode_ids_filtered;
                    let _: () = app.with_turn_state_mut(|ts| ts.mode = Some(parsed));
                    let supports_auto_mode =
                        app.current_model().is_some_and(|m| m.supports_auto_mode == Some(true));
                    let (supports_bypass, unavailable_modes) = app.with_turn_state(|ts| {
                        (
                            ts.supports_bypass_permissions_mode,
                            ts.runtime_unavailable_mode_ids.clone(),
                        )
                    });
                    let supported = supported_mode_ids_filtered(
                        supports_auto_mode,
                        supports_bypass,
                        Some(parsed),
                        &unavailable_modes,
                    );
                    let _: () = app.with_turn_state_mut(|ts| ts.supported_mode_ids = supported);
                }
            }
            // status: "compacting" → Compacting, null → Idle.
            if let Some(status_field) = data.get("status") {
                if status_field.as_str() == Some("compacting") {
                    super::apply_session_status_update(
                        app,
                        crate::agent::model::SessionStatus::Compacting,
                    );
                } else if status_field.is_null() {
                    super::apply_session_status_update(
                        app,
                        crate::agent::model::SessionStatus::Idle,
                    );
                }
            }
        }
        "session_state_changed" => {
            if let Some(wire_state) = parse_runtime_session_state(data.get("state")) {
                let model_state = convert_runtime_session_state(wire_state);
                super::handle_runtime_session_state_update(app, model_state);
            }
        }
        "api_retry" => {
            apply_api_retry_update(app, &data);
        }
        "init" => {
            apply_settings_parse_errors(app, &data);
            apply_available_commands_from_init(app, &data);
            apply_available_agents_from_init(app, &data);
            apply_current_model_from_init(app, &data);
            apply_mode_state_from_init(app, &data);
        }
        "compact_boundary" => {
            apply_compaction_boundary(app, &data);
        }
        "local_command_output" => {
            apply_local_command_output(app, &data);
        }
        _ => {}
    }
}

/// Drain `settings_errors` / `settingsErrors` from a System(init)
/// data record and call the App's settings-parse-error notice handler
/// once per error.
/// Parse a System(compact_boundary) record and call the App's
/// rate_limit::handle_compaction_boundary_update with the typed
/// boundary value.
fn apply_compaction_boundary(app: &mut App, data: &Value) {
    #[derive(serde::Deserialize)]
    struct Boundary {
        compact_metadata: Inner,
    }
    #[derive(serde::Deserialize)]
    struct Inner {
        trigger: String,
        // CLI emits both snake_case and camelCase shapes across versions.
        #[serde(alias = "preTokens")]
        pre_tokens: u64,
    }

    let Ok(boundary) = serde_json::from_value::<Boundary>(data.clone()) else {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            ?data,
            "apply_compaction_boundary: failed to decode compact_boundary record; skipped",
        );
        return;
    };
    let model_trigger = match boundary.compact_metadata.trigger.as_str() {
        "manual" => crate::agent::model::CompactionTrigger::Manual,
        "auto" => crate::agent::model::CompactionTrigger::Auto,
        _ => return,
    };
    super::rate_limit::handle_compaction_boundary_update(
        app,
        crate::agent::model::CompactionBoundary {
            trigger: model_trigger,
            pre_tokens: boundary.compact_metadata.pre_tokens,
        },
    );
}

/// Parse a `slash_commands` / `commands` array into `AvailableCommand`s.
/// Entries are either bare name strings (the `system/init`
/// `slash_commands` shape) or `{name, description, argumentHint}`
/// objects (the `commands_changed` shape); both flow through here so
/// init and the live refresh share one boundary. Non-string / nameless
/// entries are skipped; an empty `argumentHint` collapses to `None`.
fn available_commands_from_json(arr: &[Value]) -> Vec<forge_primitives::AvailableCommand> {
    arr.iter()
        .filter_map(|entry| {
            if let Some(name) = entry.as_str() {
                if name.is_empty() {
                    return None;
                }
                return Some(forge_primitives::AvailableCommand {
                    name: name.to_owned(),
                    description: String::new(),
                    input_hint: None,
                });
            }
            let obj = entry.as_object()?;
            let name = obj.get("name")?.as_str().filter(|s| !s.is_empty())?.to_owned();
            let description =
                obj.get("description").and_then(Value::as_str).unwrap_or_default().to_owned();
            let input_hint = obj
                .get("argumentHint")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            Some(forge_primitives::AvailableCommand { name, description, input_hint })
        })
        .collect()
}

/// Build `AvailableCommandsUpdate` from System(init).slash_commands.
fn apply_available_commands_from_init(app: &mut App, data: &Value) {
    let Some(record) = data.as_object() else { return };
    let Some(arr) = record.get("slash_commands").and_then(Value::as_array) else { return };
    let commands = available_commands_from_json(arr);
    if commands.is_empty() {
        return;
    }
    let model_update =
        crate::app::connect::type_converters::map_available_commands_update(commands);
    super::apply_available_commands_update(app, model_update);
}

/// Build `AvailableAgentsUpdate` from the first `system/init` of a turn.
/// Subsequent re-fires within the same turn are dropped.
fn apply_available_agents_from_init(app: &mut App, data: &Value) {
    let Some(record) = data.as_object() else { return };
    if app.with_turn_state(|ts| ts.agents_emitted_this_turn) {
        return;
    }
    let Some(agents_value) = record.get("agents") else { return };
    let agents =
        forge_workspace::translate::agents::map_available_agents_from_names(Some(agents_value));
    let _: () = app.with_turn_state_mut(|ts| ts.agents_emitted_this_turn = true);
    let model_update = crate::app::connect::type_converters::map_available_agents_update(agents);
    super::apply_available_agents_update(app, model_update);
}

/// Resolve `current_model` from System(init) data and apply via the
/// App-side `apply_current_model_update` helper if it differs from
/// the existing `app.current_model`.
///
/// The CLI's `system/init` carries the resolved `model` id but
/// NOT the `models` catalogue (that lives in the initialize
/// control_response, which the App already absorbed via Connected).
/// Reuse `app.available_models` - re-deriving from the init payload's
/// missing `models` field would return an empty catalogue and reset
/// `current_model.supports_effort` (and friends) to `false`, dropping
/// the footer's effort chip and adaptive-thinking flags after the
/// first turn lands.
fn apply_current_model_from_init(app: &mut App, data: &Value) {
    use forge_primitives as wire;
    use forge_workspace::session_lifecycle::resolve_current_model_from_inputs;

    let Some(record) = data.as_object() else { return };
    let model_id = record.get("model").and_then(Value::as_str).unwrap_or("");
    let (requested, resolved_runtime) = app.with_turn_state(|ts| {
        (ts.requested_model_id.clone(), ts.resolved_runtime_model_id.clone())
    });
    if !model_id.is_empty() {
        let model_id_owned = model_id.to_owned();
        let _: () = app.with_turn_state_mut(|ts| ts.model_id = model_id_owned);
    }
    let requested = requested.as_deref();
    let resolved_runtime = resolved_runtime.as_deref();

    // Round-trip the App's typed `model::AvailableModel` list back
    // into the wire shape the catalogue resolver expects. Cheap, runs
    // once on init.
    let available_models: Vec<wire::AvailableModel> = app
        .available_models()
        .iter()
        .map(|m| wire::AvailableModel {
            id: m.id.clone(),
            display_name: m.display_name.clone(),
            description: m.description.clone(),
            supports_effort: m.supports_effort,
            supported_effort_levels: m
                .supported_effort_levels
                .iter()
                .map(|level| match level {
                    crate::agent::model::EffortLevel::Low => wire::EffortLevel::Low,
                    crate::agent::model::EffortLevel::Medium => wire::EffortLevel::Medium,
                    crate::agent::model::EffortLevel::High => wire::EffortLevel::High,
                    crate::agent::model::EffortLevel::Xhigh => wire::EffortLevel::Xhigh,
                    crate::agent::model::EffortLevel::Max => wire::EffortLevel::Max,
                })
                .collect(),
            supports_adaptive_thinking: m.supports_adaptive_thinking,
            supports_fast_mode: m.supports_fast_mode,
            supports_auto_mode: m.supports_auto_mode,
        })
        .collect();

    let next_wire =
        resolve_current_model_from_inputs(model_id, requested, resolved_runtime, &available_models);
    if app.current_model() == Some(&next_wire) {
        return;
    }
    super::apply_current_model_update(app, next_wire);
}

/// Resolve `mode_state` from System(init) data and apply via the
/// existing App-side ModeStateUpdate dispatch arm.
///
/// Reads `permissionMode` from data, parses to a typed
/// `PermissionMode`, mirrors into `app.turn_state.mode`, recomputes
/// `supported_mode_ids` (using the App's current_model auto-mode
/// support + the bypass flag), then builds a `ModeState` and applies.
fn apply_mode_state_from_init(app: &mut App, data: &Value) {
    use forge_workspace::PermissionMode;
    use forge_workspace::commands::{build_mode_state_from_supported, supported_mode_ids_filtered};

    let Some(record) = data.as_object() else { return };
    let Some(mode_str) = record.get("permissionMode").and_then(Value::as_str) else { return };
    let Some(mode) = PermissionMode::from_wire(mode_str) else { return };
    let _: () = app.with_turn_state_mut(|ts| ts.mode = Some(mode));

    // System(init) is the canonical source for `supportsBypassPermissionsMode`.
    // Without this write the bypass chip / `/mode` option stays hidden even
    // when the CLI declares it supported.
    if let Some(supports_bypass) =
        record.get("supportsBypassPermissionsMode").and_then(Value::as_bool)
    {
        let _: () =
            app.with_turn_state_mut(|ts| ts.supports_bypass_permissions_mode = supports_bypass);
    }

    let supports_auto_mode =
        app.current_model().is_some_and(|m| m.supports_auto_mode == Some(true));
    let (supports_bypass, unavailable_modes) = app.with_turn_state(|ts| {
        (ts.supports_bypass_permissions_mode, ts.runtime_unavailable_mode_ids.clone())
    });
    let supported = supported_mode_ids_filtered(
        supports_auto_mode,
        supports_bypass,
        Some(mode),
        &unavailable_modes,
    );
    let _: () = app.with_turn_state_mut(|ts| ts.supported_mode_ids.clone_from(&supported));

    let wire_mode_state = build_mode_state_from_supported(mode, &supported);
    super::apply_mode_state_update(app, wire_mode_state);
}

/// When the SDK fires a System(local_command_output), forward the
/// content as an `AgentMessageChunk` so it appears inline in the
/// chat transcript.
fn apply_local_command_output(app: &mut App, data: &Value) {
    use crate::agent::model;
    let Some(record) = data.as_object() else { return };
    let content = record.get("content").and_then(Value::as_str).unwrap_or("");
    if content.trim().is_empty() {
        return;
    }
    super::clear_compaction_state(app, true);
    let chunk = model::ContentChunk::new(model::ContentBlock::Text(model::TextContent::new(
        content.to_owned(),
    )));
    super::streaming::handle_agent_message_chunk(app, chunk);
}

fn apply_settings_parse_errors(app: &mut App, data: &Value) {
    let Some(record) = data.as_object() else { return };
    let Some(errors) = record.get("settings_errors").or_else(|| record.get("settingsErrors"))
    else {
        return;
    };
    for err in normalize_settings_parse_errors(errors) {
        super::handle_settings_parse_error(app, err.file.as_deref(), &err.path, &err.message);
    }
}

/// Apply an api_retry system message to the App. Parses via
/// `build_api_retry_update` and calls into the existing api_retry
/// event handler.
fn apply_api_retry_update(app: &mut App, data: &Value) {
    let Some(record) = data.as_object() else { return };
    let Some(forge_primitives::ApiRetryUpdate {
        attempt,
        max_retries,
        retry_delay_ms,
        error_status,
        error,
    }) = build_api_retry_update(record)
    else {
        return;
    };
    super::api_retry::handle_api_retry_update(
        app,
        attempt,
        max_retries,
        retry_delay_ms,
        error_status,
        error,
    );
}

fn convert_runtime_session_state(
    wire: forge_primitives::RuntimeSessionState,
) -> crate::agent::model::RuntimeSessionState {
    use crate::agent::model::RuntimeSessionState as Model;
    use forge_primitives::RuntimeSessionState as Wire;
    match wire {
        Wire::Idle => Model::Idle,
        Wire::Running => Model::Running,
        Wire::RequiresAction => Model::RequiresAction,
    }
}

fn handle_task_started(app: &mut App, msg: Message) {
    let Message::TaskStarted { tool_use_id, task_id, .. } = msg else { return };
    let id = tool_use_id.as_deref().unwrap_or("");
    if id.is_empty() {
        return;
    }
    apply_tool_progress_update(app, id, "Task");
    if !task_id.is_empty() {
        let id_owned = id.to_owned();
        let _: () = app.with_turn_state_mut(|ts| {
            ts.task_tool_use_ids.insert(task_id.clone(), id_owned);
        });
        // Session-scoped mirror that survives turn finalisation - the
        // cross-turn resolver for backgrounded agents (SUBAGENTS) and
        // the PROCESSES local_bash feed.
        app.insert_session_task_mapping(task_id.clone(), id.to_owned());
        // stamp the wire-level task_id on the matching
        // MonitorEntry so subsequent `task_notification` / `task_updated`
        // events (keyed by task_id) can route to the right row.
        app.stamp_monitor_task_id(id, task_id.clone());
        // same shape for WorkflowEntry - both surfaces
        // share the task_id↔tool_use_id routing key.
        app.stamp_workflow_task_id(id, task_id);
    }
}

fn handle_task_progress(app: &mut App, msg: Message) {
    let Message::TaskProgress { tool_use_id, task_id, workflow_progress, .. } = msg else {
        return;
    };
    let id = tool_use_id.as_deref().unwrap_or("");
    if !id.is_empty() {
        apply_tool_progress_update(app, id, "Task");
    }
    // Workflow's per-event state arrives ridden in
    // `workflow_progress` on the same system/task_progress envelope.
    // Drop into the matching WorkflowEntry's per-phase tree; the
    // all-completed clear fires when the final `state: done`
    // transitions the entry to Completed.
    if !workflow_progress.is_empty() && !task_id.is_empty() {
        app.apply_workflow_progress_by_task_id(&task_id, &workflow_progress);
    }
    // refresh the matching Monitor's output_tail from
    // disk on each progress event so the file's growth is reflected
    // in the Inspector without waiting for the next task_notification.
    // No-op when the task isn't a Monitor or its output_file isn't
    // stamped yet.
    if !task_id.is_empty() {
        app.refresh_monitor_output_tail_from_file(&task_id);
    }
}

/// Apply a `TaskUpdated` patch to the originating tool call.
///
/// The wire emits `task_updated` for any long-running tool task
/// (subagent `Task`, backgrounded `Bash`, `Monitor`) carrying a
/// `patch` object with status / end_time deltas. For PROCESSES
/// rendering this is the canonical signal that a backgrounded Bash
/// transitioned from running to completed - without consuming it,
/// the chat-stream tool card and Inspector row both stay stuck on
/// `in_progress` forever.
///
/// Resolution path: `task_started` populates
/// `TurnState::task_tool_use_ids` as `task_id` → `tool_use_id`.
/// This handler reverses the lookup to find which tool call to
/// update. If the mapping is absent (out-of-order arrival or
/// task_started lost), the update is dropped with a debug log -
/// there's no recovery path that doesn't risk corrupting an
/// unrelated tool call.
fn handle_task_updated(app: &mut App, msg: Message) {
    use forge_primitives::ToolCallUpdateFields;

    let Message::TaskUpdated { task_id, patch, .. } = msg else { return };
    let Some(wire_status) = patch.status.as_deref() else {
        // No status delta in this patch (e.g. partial update that
        // only stamped end_time). Nothing for the renderer to do.
        return;
    };
    let is_terminal = matches!(wire_status, "completed" | "failed" | "killed" | "stopped");

    // Monitor + Workflow status transitions are keyed
    // by `task_id` directly (not `tool_use_id`), so they run
    // BEFORE the `task_tool_use_ids` lookup. The lookup is gated
    // on TurnState, which `default()`-resets at every turn
    // finalisation (5 sites in `events/turn.rs`). Monitor's
    // `task_updated { status: "completed" }` arrives AFTER its
    // initiating turn Result'd (Monitor runs in the background) -
    // by then the mapping is empty and the old structure
    // early-returned, leaving the MonitorEntry stuck on `· running`
    // forever and `clear_monitors_if_all_terminal` never
    // re-evaluating. `set_monitor_status_by_task_id` mirrors
    // `handle_task_notification`'s direct `task_id` routing
    // pattern (the working precedent for the per-event tail
    // surface).
    if is_terminal {
        let monitor_status = match wire_status {
            "completed" => Some(crate::app::state::types::MonitorStatus::Completed),
            "failed" | "killed" | "stopped" => {
                Some(crate::app::state::types::MonitorStatus::Stopped)
            }
            _ => None,
        };
        if let Some(status) = monitor_status {
            app.set_monitor_status_by_task_id(&task_id, status);
        }
        // same shape for WorkflowEntry - any terminal
        // status (completed | failed | killed | stopped) collapses
        // the workflow row to its summarised one-liner. Idempotent
        // when the entry is already Completed.
        app.set_workflow_completed_by_task_id(&task_id);
    }

    // The standard tool-call card update still requires the
    // `tool_use_id` mapping to apply a status patch to the
    // chat-stream card. If the mapping is missing (turn already
    // finalised + TurnState reset), skip just THIS path - the
    // MONITORS / WORKFLOWS sections are already updated above.
    let tool_use_id = app.with_turn_state(|ts| ts.task_tool_use_ids.get(&task_id).cloned());
    let Some(tool_use_id) = tool_use_id else {
        tracing::debug!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "task_updated_no_tool_use_mapping",
            message = "task_updated tool-call card skipped (mapping reset by turn finalisation); section row already updated",
            outcome = "skipped",
            task_id = %task_id,
        );
        return;
    };
    let mapped_status = map_task_updated_status_to_tool_status(wire_status);
    apply_tool_call_update(
        app,
        &tool_use_id,
        ToolCallUpdateFields { status: Some(mapped_status), ..Default::default() },
    );
}

/// Map the wire-side `task_updated.patch.status` string to the
/// typed `ToolCallStatus`. The wire uses `"running"`; the internal
/// type uses `InProgress`. `stopped` is the wire spelling for a
/// graceful cancel; map to `Killed` so the renderer picks the same
/// glyph as for explicit kill operations. Unknown wire strings fall
/// through to `Pending` (matches `#[serde(other)]` on the enum).
fn map_task_updated_status_to_tool_status(wire_status: &str) -> forge_primitives::ToolCallStatus {
    use forge_primitives::ToolCallStatus;
    match wire_status {
        "running" => ToolCallStatus::InProgress,
        "completed" => ToolCallStatus::Completed,
        "failed" => ToolCallStatus::Failed,
        "killed" | "stopped" => ToolCallStatus::Killed,
        _ => ToolCallStatus::Pending,
    }
}

fn handle_task_notification(app: &mut App, msg: Message) {
    let Message::TaskNotification { tool_use_id, task_id, output_file, summary, .. } = msg else {
        return;
    };
    let id = tool_use_id.as_deref().unwrap_or("");
    if !id.is_empty() {
        apply_tool_summary_update(app, id, &summary);
    }
    // A subagent's true completion. Every subagent - foreground and
    // backgrounded - fires a task_notification, so this reliably drops its
    // `task_id -> tool_use_id` resolver; rostered non-agents get none and are
    // cleaned by the roster diff in `handle_background_tasks_changed` instead.
    if !task_id.is_empty() {
        app.remove_session_task_mapping(&task_id);
    }
    // stamp the `output_file` path on the matching
    // MonitorEntry (idempotent) and refresh the tail from disk.
    // The wire carries `output_file` because the CLI's local-bash
    // Monitor flavour streams the watched command's stdout to
    // `/private/tmp/.../tasks/<task_id>.output` rather than over the
    // wire. The summary line alone is insufficient (it's always
    // "Monitor X stream ended" or similar boilerplate); the user
    // needs the actual command output.
    if !task_id.is_empty() && !output_file.is_empty() {
        app.set_monitor_output_file_by_task_id(&task_id, std::path::PathBuf::from(&output_file));
        app.refresh_monitor_output_tail_from_file(&task_id);
    }
    // The wire ordering is
    // `task_updated terminal -> task_notification with output_file`.
    // The status flip lands first (transitioning the MonitorEntry
    // out of Running); without deferring the auto-clear, the
    // single-monitor case would drain the Vec at task_updated time
    // and the subsequent task_notification would find no entry to
    // stamp the tail into. Auto-clear runs HERE so the tail has
    // already populated by the time the section drops out (or
    // persists, for completed Monitors with a non-empty tail).
    app.clear_monitors_if_all_terminal();
}

/// Apply a `TaskProgress` notification to App state - bumps the
/// `tool_use_id`'s status to `in_progress` if it isn't already in a
/// terminal/active state.
fn apply_tool_progress_update(app: &mut App, tool_use_id: &str, name: &str) {
    use forge_primitives::{ToolCallStatus, ToolCallUpdateFields};

    let existing = app.with_turn_state(|ts| ts.tool_calls.get(tool_use_id).cloned());
    let Some(existing) = existing else {
        apply_tool_use_block(app, tool_use_id, name, &Value::Object(serde_json::Map::new()), None);
        return;
    };
    if matches!(
        existing.status,
        ToolCallStatus::InProgress
            | ToolCallStatus::Completed
            | ToolCallStatus::Failed
            | ToolCallStatus::Killed
    ) {
        return;
    }
    apply_tool_call_update(
        app,
        tool_use_id,
        ToolCallUpdateFields { status: Some(ToolCallStatus::InProgress), ..Default::default() },
    );
}

/// Apply a `TaskNotification` summary to App state - finalises the
/// matching `tool_use_id` with `completed` status (preserving any
/// existing terminal `failed`/`killed` status) and updates content.
///
/// Persistent monitors (`raw_input.persistent == true`) are
/// deliberately exempted from the status flip even though their
/// `summary` content still updates. By design a persistent monitor
/// keeps streaming after each event notification; flipping its
/// status to `Completed` per event would mark a still-running watch
/// as done in chat-stream + Inspector. Wire captures (2026-05-14)
/// show `Monitor` doesn't actually deliver via `TaskNotification`
/// in practice - events arrive via `Result` frames with
/// `origin: task-notification`. This guard remains as defensive
/// hardening: future CLI versions may route persistent-monitor
/// events through the same handler, and the cost is one cheap
/// JSON lookup.
fn apply_tool_summary_update(app: &mut App, tool_use_id: &str, summary: &str) {
    use forge_primitives::{ToolCallContent, ToolCallStatus, ToolCallUpdateFields};

    let Some(base) = app.with_turn_state(|ts| ts.tool_calls.get(tool_use_id).cloned()) else {
        return;
    };
    let persistent = raw_input_is_persistent(base.raw_input.as_ref());
    let status = if matches!(base.status, ToolCallStatus::Failed | ToolCallStatus::Killed) {
        base.status
    } else if persistent {
        // Keep the persistent monitor visibly running: only the
        // summary content + raw_output update through.
        base.status
    } else {
        ToolCallStatus::Completed
    };
    let fields = ToolCallUpdateFields {
        status: Some(status),
        raw_output: Some(summary.to_owned()),
        content: Some(vec![ToolCallContent::Content {
            content: forge_primitives::ChunkContent::Text { text: summary.to_owned() },
        }]),
        ..Default::default()
    };
    apply_tool_call_update(app, tool_use_id, fields);
}

/// Replace the active session's background-task snapshot from a
/// `background_tasks_changed` event. The event carries the CLI's full
/// registry every change, so this overwrites wholesale; an empty
/// `tasks` array clears the list so the PROCESSES `local_bash` feed
/// drains. Entries missing `task_id` / `task_type` / `description`
/// (or not objects) are skipped rather than panicked.
fn handle_background_tasks_changed(app: &mut App, msg: Message) {
    use crate::app::state::types::BackgroundTask;
    let Message::BackgroundTasksChanged { tasks, .. } = msg else { return };
    let parsed: Vec<BackgroundTask> = tasks
        .iter()
        .filter_map(|task| {
            let obj = task.as_object()?;
            Some(BackgroundTask {
                task_id: obj.get("task_id")?.as_str()?.to_owned(),
                task_type: obj.get("task_type")?.as_str()?.to_owned(),
                description: obj.get("description")?.as_str()?.to_owned(),
            })
        })
        .collect();
    // Drift breadcrumb (Hard Rule #16): if the CLI renamed a field
    // every entry fails the parse and the section silently never
    // appears. An empty snapshot is a legitimate state (section
    // auto-hides), so this only logs - the replace still applies.
    if tasks.len() != parsed.len() {
        tracing::debug!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "background_tasks_parse_dropped",
            message = "background_tasks_changed dropped unparseable entries; possible wire drift",
            outcome = "partial",
            dropped = tasks.len() - parsed.len(),
            entry_count = tasks.len(),
        );
    }
    // Drift breadcrumb (Hard Rule #16): every kind must route to a section
    // (local_bash -> PROCESSES; agent/local_agent -> SUBAGENTS;
    // local_workflow/workflow -> WORKFLOWS via the tool-call-driven
    // WorkflowEntry, which - unlike bash/agents - only goes terminal on
    // workflow_progress `done` / terminal task_updated, never the
    // backgrounding sentinel, so a registered workflow can't false-terminal
    // and needs no registry backstop). An unrecognised kind renders
    // nowhere - warn so a renamed CLI kind is caught rather than silent.
    for task in &parsed {
        if !matches!(
            task.task_type.as_str(),
            "local_bash" | "agent" | "local_agent" | "local_workflow" | "workflow"
        ) {
            tracing::warn!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "background_task_unrouted_kind",
                message = "background_tasks entry has an unrecognised task_type; renders in no Inspector section (possible wire drift)",
                outcome = "partial",
                task_type = %task.task_type,
            );
        }
    }
    // Cleanup for rostered non-agent tasks (bash/monitor/workflow), which get
    // no task_notification: a task_id in the previous snapshot but absent from
    // this one has left the roster, so its `task_id -> tool_use_id` resolver is
    // dropped here (this also backstops an agent if its roster drop arrives).
    // Diffing old-vs-new (not pruning every unrostered entry) spares a resolver
    // whose `task_started` preceded the task's first roster event.
    let departed: Vec<String> = {
        let new_ids: std::collections::HashSet<&str> =
            parsed.iter().map(|task| task.task_id.as_str()).collect();
        app.active_session()
            .map(|session| {
                session
                    .background_tasks
                    .iter()
                    .filter(|task| !new_ids.contains(task.task_id.as_str()))
                    .map(|task| task.task_id.clone())
                    .collect()
            })
            .unwrap_or_default()
    };
    for task_id in &departed {
        app.remove_session_task_mapping(task_id);
    }
    *app.background_tasks_mut() = parsed;
}

/// Refresh the active session's slash-command list from a
/// `commands_changed` event (emitted after a plugin/command reload).
/// Parses via the shared boundary and applies through
/// `apply_available_commands_update`, so the `/` dropdown, `/help`,
/// and autocomplete all pick up the fresh list instead of the stale
/// init-time seed.
fn handle_commands_changed(app: &mut App, msg: Message) {
    let Message::CommandsChanged { commands, .. } = msg else { return };
    let parsed = available_commands_from_json(&commands);
    // Drift guard (Hard Rule #16): a non-empty payload that parses to
    // nothing means the CLI's command-entry shape changed under us.
    // Applying it would silently wipe the `/` dropdown + `/help`, so
    // skip and leave the prior list intact. A legitimately empty
    // `commands: []` (plugin uninstall) still falls through to clear.
    if parsed.is_empty() && !commands.is_empty() {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "commands_changed_parse_empty",
            message = "commands_changed carried entries but none parsed; likely wire drift, keeping prior list",
            outcome = "skipped",
            entry_count = commands.len(),
        );
        return;
    }
    let model_update = crate::app::connect::type_converters::map_available_commands_update(parsed);
    super::apply_available_commands_update(app, model_update);
}

fn handle_rate_limit_event(app: &mut App, msg: Message) {
    let Message::RateLimitEvent { rate_limit_info, .. } = msg else { return };
    let value = serde_json::to_value(&rate_limit_info).unwrap_or(Value::Null);
    // Per-session config_dir from the workspace's session binding,
    // NOT `std::env::var("CLAUDE_CONFIG_DIR")` - multiple accounts
    // mean each session has its own bound config_dir, distinct from
    // forge's own host config_dir. Reading from env here would log
    // forge's path on every event regardless of which account
    // actually owns this rate-limit signal.
    let config_dir = app
        .workspace
        .as_ref()
        .and_then(|ws| app.active_session_key.as_ref().and_then(|k| ws.config_dir_for(k)))
        .map_or_else(|| "(unbound)".to_owned(), |p| p.to_string_lossy().into_owned());
    // Raw payload at debug - useful for triaging whether a notice
    // surfaces from forge cache vs. an account-level Anthropic signal.
    tracing::debug!(
        target: crate::logging::targets::APP_SESSION,
        event_name = "rate_limit_event_received",
        message = "raw RateLimitEvent payload from forge-sdk",
        outcome = "wire_evidence",
        config_dir = %config_dir,
        session_id = app.session_id().map(|s| s.to_string()).as_deref().unwrap_or(""),
        rate_limit_info = %value,
    );
    let Some(update) = build_rate_limit_update(Some(&value)) else {
        return;
    };
    super::rate_limit::handle_rate_limit_update(app, &update);
}

fn handle_result(app: &mut App, msg: Message) {
    let Message::Result {
        duration_ms,
        is_error,
        subtype,
        errors,
        terminal_reason,
        fast_mode_state,
        ..
    } = msg
    else {
        return;
    };
    stamp_turn_duration_on_latest_assistant(app, duration_ms);
    // #273: Turn ended - clear per-turn thinking-token chip so the
    // next in-progress turn starts with a bare spinner (it'll
    // re-populate once `Message::ThinkingTokens` fires for the new
    // turn). `stop_hook_summary` is left intact: it belongs to the
    // just-completed turn's end-of-turn surface.
    app.set_latest_thinking_tokens(None);
    if let Some(state) = fast_mode_state {
        apply_fast_mode_state(app, state);
    }
    apply_result_finalize(app, is_error, &subtype, errors.unwrap_or_default(), terminal_reason);
}

/// Stamp `Message::Result.duration_ms` onto the latest Assistant
/// ChatMessage in the active session, invalidating its render cache so
/// the `Forge - N.Ns` chip in the role-label line re-renders.
///
/// No-op when no Assistant message is present (rare: Result fires
/// before any assistant content has been pushed). The wire stamp +
/// chip render are decoupled - the chip just won't appear that turn,
/// no panic.
fn stamp_turn_duration_on_latest_assistant(app: &mut App, duration_ms: u64) {
    let Some(msg) = app
        .active_messages_mut()
        .iter_mut()
        .rev()
        .find(|m| matches!(m.role, crate::app::MessageRole::Assistant))
    else {
        return;
    };
    msg.turn_duration_ms = Some(duration_ms);
    msg.invalidate_render_cache();
}

/// On a successful Result, finalise any still-open tool_calls
/// (terminal "completed") and trigger the App's TurnComplete handler.
/// On a failed Result, finalise with "failed", classify the
/// error_kind, and trigger the App's TurnError handler.
fn apply_result_finalize(
    app: &mut App,
    is_error: bool,
    subtype: &str,
    errors_array: Vec<String>,
    terminal_reason: Option<forge_primitives::TerminalReason>,
) {
    // `apply_result_finalize` only runs on the active session - the
    // SDK message dispatcher in `super::client` adopts the message's
    // session_id onto the active bucket before firing the sub-
    // handlers. Cloning the active session_key here threads it
    // through to the lifecycle handlers without leaking the
    // multiplexer's routing concern into every sub-handler.
    let active_key = app
        .active_session_key
        .clone()
        .unwrap_or_else(|| forge_workspace::SessionKey::from_session_id(App::PRE_CONNECT_KEY));
    if !is_error && subtype == "success" {
        let _: () = app.with_turn_state_mut(|ts| ts.last_assistant_error = None);
        finalize_open_tool_calls(app, forge_primitives::ToolCallStatus::Completed);
        super::turn::handle_turn_complete_event(app, &active_key, terminal_reason);
        return;
    }

    let assistant_error = app.with_turn_state(|ts| ts.last_assistant_error.clone());
    finalize_open_tool_calls(app, forge_primitives::ToolCallStatus::Failed);
    // Build a clean detail string for the renderer to use after its
    // canonical "Turn failed: " prefix. Drop the SDK's default
    // `subtype="success"` (an internal bookkeeping value, not a user
    // reason). Don't prepend "turn failed:" here - the renderer adds
    // the prefix once, and doubling it produces "Turn failed: turn
    // failed: ..." in the chat.
    let message = if !errors_array.is_empty() {
        errors_array.join("\n")
    } else if !subtype.is_empty() && subtype != "success" {
        subtype.to_owned()
    } else {
        String::new()
    };
    let class = classify_turn_error_kind(subtype, &errors_array, assistant_error.as_deref());
    super::turn::handle_turn_error_event(app, &active_key, &message, Some(class), terminal_reason);
    let _: () = app.with_turn_state_mut(|ts| ts.last_assistant_error = None);
}

/// App-side classifier for `TurnError` payloads - picks one of the
/// `TurnErrorClass` variants based on subtype + error strings, used to
/// drive UI rendering for the failure case.
fn classify_turn_error_kind(
    subtype: &str,
    errors: &[String],
    assistant_error: Option<&str>,
) -> forge_workspace::translate::error_handling::TurnErrorClass {
    use forge_workspace::translate::error_handling::TurnErrorClass;
    let plan_limit_signals =
        ["error_max_turns", "error_max_budget_usd", "billing_error", "rate_limit"];
    if plan_limit_signals.iter().any(|s| subtype.contains(s)) {
        return TurnErrorClass::PlanLimit;
    }
    if errors.iter().any(|e| plan_limit_signals.iter().any(|s| e.contains(s))) {
        return TurnErrorClass::PlanLimit;
    }
    if assistant_error == Some("authentication_failed") {
        return TurnErrorClass::AuthRequired;
    }
    if errors.iter().any(|e| {
        forge_workspace::translate::error_handling::looks_like_auth_required_error_lower(
            &e.to_ascii_lowercase(),
        )
    }) {
        return TurnErrorClass::AuthRequired;
    }
    if assistant_error == Some("server_error") {
        return TurnErrorClass::Internal;
    }
    TurnErrorClass::Other
}

#[cfg(test)]
mod task_updated_mapping_tests {
    use super::map_task_updated_status_to_tool_status;

    #[test]
    fn running_maps_to_in_progress() {
        // Wire spelling vs forge-internal spelling. Without this
        // mapping, a backgrounded Bash transitioning through
        // running would be left in the Pending fallback and the
        // renderer would pick an unintended glyph.
        assert_eq!(
            map_task_updated_status_to_tool_status("running"),
            forge_primitives::ToolCallStatus::InProgress,
        );
    }

    #[test]
    fn stopped_maps_to_killed() {
        // `stopped` is the wire spelling for a graceful cancel
        // (e.g. claude TaskStop). Map to the internal `killed`
        // so the renderer picks the same red glyph as for explicit
        // kills.
        assert_eq!(
            map_task_updated_status_to_tool_status("stopped"),
            forge_primitives::ToolCallStatus::Killed,
        );
    }

    #[test]
    fn round_trip_statuses_unchanged() {
        use forge_primitives::ToolCallStatus;
        assert_eq!(map_task_updated_status_to_tool_status("completed"), ToolCallStatus::Completed);
        assert_eq!(map_task_updated_status_to_tool_status("failed"), ToolCallStatus::Failed);
        assert_eq!(map_task_updated_status_to_tool_status("killed"), ToolCallStatus::Killed);
        assert_eq!(map_task_updated_status_to_tool_status("pending"), ToolCallStatus::Pending);
    }

    #[test]
    fn unknown_status_falls_through_to_pending() {
        // Forward-compat: a future CLI version adding `degraded`
        // (say) must not crash forge. Unknown wire values land in
        // the `Pending` fallback so the renderer's glyph picker
        // has a defined fallback.
        assert_eq!(
            map_task_updated_status_to_tool_status("degraded"),
            forge_primitives::ToolCallStatus::Pending,
        );
    }
}

#[cfg(test)]
mod persistent_guard_tests {
    use super::raw_input_is_persistent;
    use serde_json::json;

    #[test]
    fn returns_false_for_none_raw_input() {
        // CRITICAL: an absent raw_input MUST NOT be treated as
        // persistent-false implicitly - but it also can't be treated
        // as persistent-true. The function returns false here, which
        // is the safe fallback for the `finalize_open_tool_calls`
        // caller (a tool with no decoded raw_input gets swept normally)
        // and for `apply_tool_summary_update` (the status-flip path
        // applies, matching pre-fix behaviour for tools without
        // raw_input).
        assert!(!raw_input_is_persistent(None));
    }

    #[test]
    fn returns_false_when_persistent_field_missing() {
        let input = json!({"command": "echo hi", "description": "test"});
        assert!(!raw_input_is_persistent(Some(&input)));
    }

    #[test]
    fn returns_false_when_persistent_field_is_false() {
        let input = json!({"persistent": false, "command": "ls"});
        assert!(!raw_input_is_persistent(Some(&input)));
    }

    #[test]
    fn returns_true_when_persistent_field_is_true() {
        let input = json!({"persistent": true, "command": "tail -F /var/log/x"});
        assert!(raw_input_is_persistent(Some(&input)));
    }

    #[test]
    fn returns_false_when_persistent_field_is_non_bool() {
        // Defensive: if `persistent` is somehow a string (wire drift
        // or test fixture), the guard treats it as not-persistent
        // rather than panicking or evaluating truthy.
        let input = json!({"persistent": "true"});
        assert!(!raw_input_is_persistent(Some(&input)));
    }

    #[test]
    fn returns_false_when_raw_input_is_not_an_object() {
        // Wire could conceivably emit a non-object value here
        // (e.g. a literal string for a malformed tool). The guard
        // must not panic on `as_object()` returning None.
        let input = json!("not-an-object");
        assert!(!raw_input_is_persistent(Some(&input)));
    }
}

#[cfg(test)]
mod stamp_turn_duration_tests {
    //! Unit coverage for `stamp_turn_duration_on_latest_assistant`,
    //! the helper called from `handle_result` when a `Message::Result`
    //! frame arrives carrying the wire `duration_ms`. The full
    //! handle_result -> finalize chain pushes a placeholder Assistant
    //! for buffered-next-turn anticipation, so this module tests the
    //! stamp helper in isolation; the wire-driven end-to-end path is
    //! pinned in `replay.rs`.
    use super::stamp_turn_duration_on_latest_assistant;
    use crate::app::{App, ChatMessage, MessageRole};

    #[test]
    fn stamps_duration_on_latest_assistant_message() {
        let mut app = App::test_default();
        app.push_message_tracked(ChatMessage::new(MessageRole::Assistant, Vec::new(), None));

        stamp_turn_duration_on_latest_assistant(&mut app, 12_768);

        let latest = app
            .messages()
            .iter()
            .rev()
            .find(|m| matches!(m.role, MessageRole::Assistant))
            .expect("seeded assistant message present");
        assert_eq!(latest.turn_duration_ms, Some(12_768));
    }

    #[test]
    fn no_op_when_no_assistant_message_present() {
        let mut app = App::test_default();
        // No assistant messages seeded; helper's rev().find() returns
        // None and the stamp call is a no-op. Verifying no panic +
        // no spurious mutation is the contract.
        stamp_turn_duration_on_latest_assistant(&mut app, 99);
        assert!(app.messages().is_empty());
    }

    #[test]
    fn stamps_latest_assistant_skipping_intervening_user() {
        let mut app = App::test_default();
        app.push_message_tracked(ChatMessage::new(MessageRole::Assistant, Vec::new(), None));
        app.push_message_tracked(ChatMessage::new(MessageRole::User, Vec::new(), None));
        app.push_message_tracked(ChatMessage::new(MessageRole::Assistant, Vec::new(), None));
        app.push_message_tracked(ChatMessage::new(MessageRole::User, Vec::new(), None));

        stamp_turn_duration_on_latest_assistant(&mut app, 5_000);

        // Latest (idx 2) Assistant gets the stamp; earlier (idx 0) stays None.
        let assistants: Vec<Option<u64>> = app
            .messages()
            .iter()
            .filter(|m| matches!(m.role, MessageRole::Assistant))
            .map(|m| m.turn_duration_ms)
            .collect();
        assert_eq!(assistants, vec![None, Some(5_000)]);
    }
}

#[cfg(test)]
mod assistant_lifecycle_gate_tests {
    //! Regression coverage for the launchpad-spinner-stuck bug.
    //!
    //! `handle_assistant` flips `lifecycle = Running` on every assistant
    //! envelope so that switching into a project mid-turn surfaces the
    //! spinning glyph in the Projects pane (see commit `1d30062`). But
    //! `load_resume_history` reuses the same dispatcher to walk on-disk
    //! history, so without a gate every replayed assistant message
    //! flipped a freshly-resumed bucket to Running with no balancing
    //! `Result` to flip it back - the Projects pane row stuck on the
    //! spinner glyph until the user submitted a real prompt.
    //!
    //! These tests pin the contract: live messages still flip lifecycle
    //! to Running; replayed messages (flagged via
    //! `App.replay_in_progress`) do not.
    use super::handle_assistant;
    use crate::app::App;
    use crate::app::session::SessionLifecycleState;
    use forge_primitives::{AssistantEnvelope, ContentBlock, Message};

    fn assistant_text_message(text: &str) -> Message {
        Message::Assistant {
            message: AssistantEnvelope {
                id: "msg_test".to_owned(),
                role: "assistant".to_owned(),
                model: "claude-test".to_owned(),
                content: vec![ContentBlock::Text { text: text.to_owned() }],
                stop_reason: None,
                stop_sequence: None,
                usage: None,
            },
            session_id: String::new(),
            parent_tool_use_id: None,
            error: None,
            uuid: None,
        }
    }

    #[test]
    fn live_assistant_message_flips_lifecycle_to_running() {
        let mut app = App::test_default();
        // Baseline: bucket starts at Idle (the pre-Connect bucket
        // initialiser leaves lifecycle at the default `Sleeping`, so
        // pin Idle here explicitly to model a connected bucket).
        if let Some(key) = app.active_session_key.clone()
            && let Some(bucket) = app.sessions.get_mut(&key)
        {
            bucket.lifecycle_state = SessionLifecycleState::Idle;
        }
        // Live wire content: replay flag stays false.
        assert!(!app.replay_in_progress);
        handle_assistant(&mut app, assistant_text_message("hi"));
        let key = app.active_session_key.clone().expect("active key");
        let bucket = app.sessions.get(&key).expect("bucket");
        assert_eq!(
            bucket.lifecycle_state,
            SessionLifecycleState::Running,
            "live assistant arrival must flip lifecycle to Running",
        );
    }

    #[test]
    fn replay_assistant_message_does_not_flip_lifecycle() {
        let mut app = App::test_default();
        if let Some(key) = app.active_session_key.clone()
            && let Some(bucket) = app.sessions.get_mut(&key)
        {
            bucket.lifecycle_state = SessionLifecycleState::Idle;
        }
        // Replay walking: flag is true while load_resume_history
        // iterates the on-disk history through this dispatcher.
        app.replay_in_progress = true;
        handle_assistant(&mut app, assistant_text_message("historical reply"));
        let key = app.active_session_key.clone().expect("active key");
        let bucket = app.sessions.get(&key).expect("bucket");
        assert_eq!(
            bucket.lifecycle_state,
            SessionLifecycleState::Idle,
            "replayed assistant message must NOT flip lifecycle - that's what \
             leaves the Projects pane spinner stuck after a launchpad resume",
        );
    }
}

#[cfg(test)]
mod queued_command_tests {
    use super::extract_queued_command_text;
    use serde_json::json;

    #[test]
    fn plain_string_prompt_round_trips() {
        let prompt = json!("Q1, let's give.");
        assert_eq!(extract_queued_command_text(&prompt), "Q1, let's give.");
    }

    #[test]
    fn multi_block_prompt_concatenates_text_blocks() {
        // Multi-modal queued input: text + image.
        let prompt = json!([
            {"type": "text", "text": "look at this"},
            {"type": "image", "source": {"type": "base64", "data": "..."}},
        ]);
        assert_eq!(extract_queued_command_text(&prompt), "look at this\n[image]");
    }

    #[test]
    fn unknown_inner_block_type_renders_as_placeholder() {
        // Forward-compat: unrecognised inner block types render as
        // `[<type>]` placeholders so the user sees something.
        let prompt = json!([
            {"type": "text", "text": "hi"},
            {"type": "future_block_type", "payload": "..."},
        ]);
        assert_eq!(extract_queued_command_text(&prompt), "hi\n[future_block_type]");
    }

    #[test]
    fn empty_array_returns_empty_string() {
        let prompt = json!([]);
        assert_eq!(extract_queued_command_text(&prompt), "");
    }

    #[test]
    fn non_array_non_string_falls_back_to_json_literal() {
        // Object shape - render as JSON literal so the user sees
        // something rather than blank.
        let prompt = json!({"weird": "shape"});
        let out = extract_queued_command_text(&prompt);
        assert!(out.contains("weird"));
    }
}

#[cfg(test)]
mod task_updated_section_routing_tests {
    //! Monitor + Workflow status transitions in
    //! `handle_task_updated` run BEFORE the `task_tool_use_ids`
    //! lookup so they survive the turn-finalisation reset that
    //! drops the mapping. Without this, Monitor's terminal
    //! `task_updated` arriving after Result early-returned and
    //! the MONITORS row stayed on `· running` forever.
    use super::handle_task_updated;
    use crate::app::App;
    use crate::app::state::types::{MonitorEntry, MonitorStatus};
    use forge_primitives::{Message, messages::TaskUpdatePatch};

    fn push_monitor(app: &mut App, task_id: &str) {
        let entry = MonitorEntry {
            tool_use_id: format!("tu_{task_id}"),
            task_id: Some(task_id.to_owned()),
            description: format!("test monitor {task_id}"),
            command: "true".to_owned(),
            persistent: false,
            timeout_ms: 0,
            status: MonitorStatus::Running,
            output_file: None,
            output_tail: std::collections::VecDeque::new(),
            expanded_in_inspector: false,
        };
        app.monitors_mut().push(entry);
    }

    fn task_updated(task_id: &str, status: &str) -> Message {
        Message::TaskUpdated {
            task_id: task_id.to_owned(),
            patch: TaskUpdatePatch { status: Some(status.to_owned()), end_time: None },
            uuid: String::new(),
            session_id: String::new(),
        }
    }

    #[test]
    fn monitor_transitions_to_completed_after_turn_finalisation() {
        // Reproduce the user-reported Bug 3a sequence: Monitor
        // launched in turn A -> turn A Result'd (mapping reset) ->
        // Monitor's `task_updated { status: "completed" }` arrives.
        // BEFORE this fix the early-return at the missing
        // `task_tool_use_ids` lookup dropped the transition. After
        // the fix the section row flips to Completed.
        //
        // the entry STAYS in the list after
        // task_updated; auto-clear is deferred to
        // `handle_task_notification` so the tail can populate first.
        let mut app = App::test_default();
        push_monitor(&mut app, "task_1");
        // Simulate turn finalisation: TurnState's task_tool_use_ids
        // mapping is empty (the `default()` after Result).
        // No task_started has populated it for `task_1`.
        handle_task_updated(&mut app, task_updated("task_1", "completed"));
        // Status transitioned; entry persists waiting for
        // task_notification.
        assert_eq!(app.monitors().len(), 1);
        assert_eq!(app.monitors()[0].status, MonitorStatus::Completed);
    }

    #[test]
    fn monitor_killed_status_maps_to_stopped() {
        // status transitions but auto-clear waits for
        // task_notification. Status check pins the mapping
        // (killed/stopped wire string -> Stopped MonitorStatus).
        let mut app = App::test_default();
        push_monitor(&mut app, "task_2");
        handle_task_updated(&mut app, task_updated("task_2", "killed"));
        assert_eq!(app.monitors().len(), 1);
        assert_eq!(app.monitors()[0].status, MonitorStatus::Stopped);
    }

    #[test]
    fn two_monitors_completing_in_order_clears_section() {
        // Contract: `clear_monitors_if_all_terminal` only drains the
        // section once every entry has transitioned out of Running.
        // The clear is driven by `handle_task_notification`; this
        // test invokes the helper directly to model that call site.
        let mut app = App::test_default();
        push_monitor(&mut app, "task_a");
        push_monitor(&mut app, "task_b");
        handle_task_updated(&mut app, task_updated("task_a", "completed"));
        // task_a transitioned to Completed; task_b still Running.
        // No clear runs yet (Bug 5a defers it).
        assert_eq!(app.monitors().len(), 2);
        let task_a = app.monitors().iter().find(|m| m.task_id.as_deref() == Some("task_a"));
        let task_b = app.monitors().iter().find(|m| m.task_id.as_deref() == Some("task_b"));
        assert_eq!(task_a.map(|m| m.status), Some(MonitorStatus::Completed));
        assert_eq!(task_b.map(|m| m.status), Some(MonitorStatus::Running));
        handle_task_updated(&mut app, task_updated("task_b", "completed"));
        // Both terminal. Still 2 entries until clear fires.
        assert_eq!(app.monitors().len(), 2);
        // Explicit clear (what `handle_task_notification` runs in
        // production) drains the section.
        app.clear_monitors_if_all_terminal();
        assert!(app.monitors().is_empty());
    }

    #[test]
    fn double_completed_event_is_idempotent() {
        // Contract: duplicate task_updated events (e.g. retry) must
        // re-stamp Completed without panicking or duplicating the
        // entry.
        let mut app = App::test_default();
        push_monitor(&mut app, "task_dup");
        handle_task_updated(&mut app, task_updated("task_dup", "completed"));
        assert_eq!(app.monitors().len(), 1);
        assert_eq!(app.monitors()[0].status, MonitorStatus::Completed);
        // Second arrival: same task_id, same target status.
        // Idempotent - no panic, no duplicate entry.
        handle_task_updated(&mut app, task_updated("task_dup", "completed"));
        assert_eq!(app.monitors().len(), 1);
        assert_eq!(app.monitors()[0].status, MonitorStatus::Completed);
    }

    #[test]
    fn no_status_field_in_patch_skips_transition() {
        // Partial patch (end_time only, no status) leaves the
        // monitor running.
        let mut app = App::test_default();
        push_monitor(&mut app, "task_partial");
        let msg = Message::TaskUpdated {
            task_id: "task_partial".to_owned(),
            patch: TaskUpdatePatch { status: None, end_time: Some(42) },
            uuid: String::new(),
            session_id: String::new(),
        };
        handle_task_updated(&mut app, msg);
        assert_eq!(app.monitors().len(), 1);
        assert_eq!(app.monitors()[0].status, MonitorStatus::Running);
    }
}

#[cfg(test)]
mod monitor_output_file_wiring_tests {
    //! `handle_task_notification` reads the
    //! `output_file` from disk and replaces the MonitorEntry's
    //! `output_tail` with the last 12 lines. `handle_task_progress`
    //! re-reads on each event so the tail grows with the running
    //! command.
    use super::{handle_task_notification, handle_task_progress};
    use crate::app::App;
    use crate::app::state::types::{MonitorEntry, MonitorStatus};
    use forge_primitives::{
        Message, TaskNotificationStatus, TaskUsage, messages::WorkflowProgressEvent,
    };
    use std::io::Write;

    fn push_monitor(app: &mut App, task_id: &str) {
        let entry = MonitorEntry {
            tool_use_id: format!("tu_{task_id}"),
            task_id: Some(task_id.to_owned()),
            description: format!("monitor {task_id}"),
            command: "true".to_owned(),
            persistent: true,
            timeout_ms: 0,
            status: MonitorStatus::Running,
            output_file: None,
            output_tail: std::collections::VecDeque::new(),
            expanded_in_inspector: false,
        };
        app.monitors_mut().push(entry);
    }

    fn write_tmp(contents: &str) -> std::path::PathBuf {
        use std::hash::{Hash, Hasher};
        let dir = std::env::temp_dir();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        contents.hash(&mut hasher);
        let id = hasher.finish();
        let pid = std::process::id();
        let path = dir.join(format!("forge_monitor_wiring_test_{pid}_{id}.log"));
        let mut f = std::fs::File::create(&path).expect("create");
        f.write_all(contents.as_bytes()).expect("write");
        path
    }

    fn notification(task_id: &str, output_file: &str, summary: &str) -> Message {
        Message::TaskNotification {
            task_id: task_id.to_owned(),
            status: TaskNotificationStatus::Completed,
            output_file: output_file.to_owned(),
            summary: summary.to_owned(),
            uuid: String::new(),
            session_id: String::new(),
            tool_use_id: None,
            usage: None,
        }
    }

    #[test]
    fn task_notification_replaces_tail_with_file_contents() {
        let path = write_tmp(
            "line 01\nline 02\nline 03\nline 04\nline 05\nline 06\nline 07\nline 08\nline 09\nline 10\nline 11\nline 12\nline 13\nline 14\nline 15\n",
        );
        let mut app = App::test_default();
        push_monitor(&mut app, "task_tail");
        handle_task_notification(
            &mut app,
            notification("task_tail", path.to_str().unwrap(), "Monitor stream ended"),
        );
        let tail: Vec<&str> = app.monitors()[0].output_tail.iter().map(String::as_str).collect();
        // Last 12 of 15.
        assert_eq!(tail.len(), 12);
        assert_eq!(tail.first(), Some(&"line 04"));
        assert_eq!(tail.last(), Some(&"line 15"));
        // output_file stamped for the next refresh.
        assert_eq!(app.monitors()[0].output_file.as_ref(), Some(&path));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn empty_output_file_path_falls_back_to_summary_only_behaviour() {
        // Wire field absent (empty string): no file read, no panic,
        // no path stamp - the summary line is the only tail signal.
        let mut app = App::test_default();
        push_monitor(&mut app, "task_no_file");
        handle_task_notification(&mut app, notification("task_no_file", "", "Monitor X"));
        assert!(app.monitors()[0].output_file.is_none());
        assert!(app.monitors()[0].output_tail.is_empty());
    }

    #[test]
    fn missing_output_file_on_disk_preserves_prior_tail() {
        // Wire carries an output_file path that doesn't exist on
        // disk (Monitor just started, OS hasn't created it yet).
        // `read_output_file_tail` returns None; we preserve the
        // previously-stored tail. Stamp the path so subsequent
        // refreshes pick it up once the file lands.
        let mut app = App::test_default();
        push_monitor(&mut app, "task_late");
        // Pre-populate tail to verify it survives the missing-file path.
        app.monitors_mut()[0].output_tail.push_back("prior tail line".to_owned());
        handle_task_notification(
            &mut app,
            notification(
                "task_late",
                "/nonexistent/forge_monitor_late_file.log",
                "Monitor just started",
            ),
        );
        let tail: Vec<&str> = app.monitors()[0].output_tail.iter().map(String::as_str).collect();
        // Prior tail preserved.
        assert_eq!(tail, vec!["prior tail line"]);
        // Path stamped for next refresh.
        assert!(app.monitors()[0].output_file.is_some());
    }

    #[test]
    fn task_progress_re_reads_stored_output_file() {
        // Simulate Monitor grow: first notification reads the file
        // with 3 lines; we then append more lines on disk + fire
        // task_progress. The progress handler re-reads and updates
        // the tail to reflect the growth.
        let path = write_tmp("a\nb\nc\n");
        let mut app = App::test_default();
        push_monitor(&mut app, "task_grow");
        handle_task_notification(
            &mut app,
            notification("task_grow", path.to_str().unwrap(), "started"),
        );
        let tail_before: Vec<String> = app.monitors()[0].output_tail.iter().cloned().collect();
        assert_eq!(tail_before, vec!["a", "b", "c"]);

        // Append more lines.
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).expect("append");
        f.write_all(b"d\ne\n").expect("write");
        drop(f);

        // task_progress (no workflow_progress; pure refresh trigger).
        let progress = Message::TaskProgress {
            task_id: "task_grow".to_owned(),
            description: String::new(),
            usage: TaskUsage { total_tokens: 0, tool_uses: 0, duration_ms: 0 },
            uuid: String::new(),
            session_id: String::new(),
            tool_use_id: None,
            last_tool_name: None,
            workflow_progress: Vec::<WorkflowProgressEvent>::new(),
        };
        handle_task_progress(&mut app, progress);
        let tail_after: Vec<String> = app.monitors()[0].output_tail.iter().cloned().collect();
        assert_eq!(tail_after, vec!["a", "b", "c", "d", "e"]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bug_5a_wire_sequence_populates_tail_before_section_drains() {
        // full end-to-end wire sequence. The actual
        // wire ordering is `task_updated terminal -> task_notification
        // with output_file`. Without Bug 5a, the status setter
        // would drain the monitors Vec at task_updated, leaving
        // task_notification with no entry to stamp the file path /
        // tail into. This test threads the entire flow through the
        // production call sites so a future refactor that drops
        // the explicit `clear_monitors_if_all_terminal()` from
        // `handle_task_notification` (or restores it to the status
        // setter) fails loudly.
        use forge_primitives::messages::TaskUpdatePatch;
        let path = write_tmp("captured-line-1\ncaptured-line-2\n");
        let mut app = App::test_default();
        push_monitor(&mut app, "task_wire");

        // Step 1: task_updated terminal. Status flips to Completed;
        // entry persists (Bug 5a deferred the auto-clear).
        let task_updated_msg = Message::TaskUpdated {
            task_id: "task_wire".to_owned(),
            patch: TaskUpdatePatch { status: Some("completed".to_owned()), end_time: None },
            uuid: String::new(),
            session_id: String::new(),
        };
        super::handle_task_updated(&mut app, task_updated_msg);
        assert_eq!(
            app.monitors().len(),
            1,
            "task_updated must not drain the section before task_notification stamps the tail",
        );
        assert_eq!(app.monitors()[0].status, MonitorStatus::Completed);
        assert!(
            app.monitors()[0].output_tail.is_empty(),
            "tail not stamped yet (task_notification hasn't arrived)",
        );

        // Step 2: task_notification with output_file. Tail
        // populates BEFORE the explicit clear at the end of
        // handle_task_notification drains the section.
        handle_task_notification(
            &mut app,
            notification("task_wire", path.to_str().unwrap(), "Monitor stream ended"),
        );

        // Section drained (only Monitor was terminal). Capture the
        // tail-was-populated invariant via a fresh push + render
        // path - we can't read the section after the drain, so the
        // assertion is "section drained AS A RESULT OF the
        // notification, not the task_updated".
        assert!(
            app.monitors().is_empty(),
            "section drained at task_notification time, after tail stamp",
        );

        // Counter-test the same flow with TWO monitors: the second
        // one stays Running, so the section persists and we can
        // inspect the first monitor's tail directly.
        let mut app = App::test_default();
        push_monitor(&mut app, "task_done");
        push_monitor(&mut app, "task_run");
        let task_updated_msg = Message::TaskUpdated {
            task_id: "task_done".to_owned(),
            patch: TaskUpdatePatch { status: Some("completed".to_owned()), end_time: None },
            uuid: String::new(),
            session_id: String::new(),
        };
        super::handle_task_updated(&mut app, task_updated_msg);
        handle_task_notification(
            &mut app,
            notification("task_done", path.to_str().unwrap(), "Monitor stream ended"),
        );
        // task_run still Running so the section persists; the
        // completed monitor's tail captured from disk.
        assert_eq!(app.monitors().len(), 2);
        let done = app
            .monitors()
            .iter()
            .find(|m| m.task_id.as_deref() == Some("task_done"))
            .expect("completed monitor present");
        assert_eq!(done.status, MonitorStatus::Completed);
        let captured: Vec<&str> = done.output_tail.iter().map(String::as_str).collect();
        assert_eq!(
            captured,
            vec!["captured-line-1", "captured-line-2"],
            "tail must populate before clear fires",
        );
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod thinking_tokens_clear_on_user_tests {
    //! A new genuine user turn must clear the
    //! `latest_thinking_tokens` carry-over from the prior turn so the
    //! chip never reads stale 150 when the new turn opens with its
    //! reset cumulative 50.
    use super::handle_user;
    use crate::app::App;
    use forge_primitives::{ContentBlock, Message, UserEnvelope};

    fn user_prompt(text: &str) -> Message {
        Message::User {
            message: UserEnvelope {
                role: "user".to_owned(),
                content: vec![ContentBlock::Text { text: text.to_owned() }],
            },
            session_id: String::new(),
            parent_tool_use_id: None,
            uuid: None,
            tool_use_result: None,
        }
    }

    fn tool_result_echo() -> Message {
        Message::User {
            message: UserEnvelope { role: "user".to_owned(), content: Vec::new() },
            session_id: String::new(),
            parent_tool_use_id: Some("toolu_test".to_owned()),
            uuid: None,
            // Non-null tool_use_result marks this as a mid-turn
            // tool-result echo, NOT a genuine user prompt.
            tool_use_result: Some(serde_json::json!({})),
        }
    }

    #[test]
    fn genuine_user_prompt_clears_stale_thinking_tokens() {
        let mut app = App::test_default();
        app.set_latest_thinking_tokens(Some(150));
        handle_user(&mut app, user_prompt("next prompt"));
        assert_eq!(
            app.latest_thinking_tokens(),
            None,
            "real user turn must clear the prior turn's carry-over",
        );
    }

    #[test]
    fn tool_result_echo_preserves_thinking_tokens() {
        // Tool-result echoes are intra-turn continuations of the
        // assistant's tool-call loop. They share the same
        // accumulating thinking_tokens budget; clearing on them
        // would drop the chip in the middle of an active turn.
        let mut app = App::test_default();
        app.set_latest_thinking_tokens(Some(150));
        handle_user(&mut app, tool_result_echo());
        assert_eq!(
            app.latest_thinking_tokens(),
            Some(150),
            "tool-result echo must NOT clear the active turn's count",
        );
    }
}

#[cfg(test)]
mod inbound_message_surfacing_tests {
    //! Coverage for the two inbound-surfacing fixes:
    //!
    //! - Arrival order: a delivered peer / worker / gotify user turn
    //!   appends at the TAIL, never inserted above an in-flight
    //!   assistant turn (which is where the outbound send lives).
    //! - Running indicator: a delivered prompt flips `status` to
    //!   Thinking and the bucket lifecycle to Running the moment it
    //!   lands, mirroring the local input-submit path, so the session
    //!   reads as active while the agent works rather than idle-then-burst.
    use super::handle_user;
    use crate::app::session::SessionLifecycleState;
    use crate::app::{App, AppStatus, ChatMessage, MessageBlock, MessageRole, TextBlock};
    use forge_primitives::{ContentBlock, Message, UserEnvelope};

    fn delivered_user_turn(text: &str) -> Message {
        Message::User {
            message: UserEnvelope {
                role: "user".to_owned(),
                content: vec![ContentBlock::Text { text: text.to_owned() }],
            },
            session_id: String::new(),
            parent_tool_use_id: None,
            uuid: None,
            tool_use_result: None,
        }
    }

    const PEER_REPLY: &str =
        "[Reply id=q-1 from agent 'planner' (org 'Personal') to your earlier ask]\n\nDone.";
    const GOTIFY_NOTE: &str =
        "[Gotify - app 'Backups', priority 3]\nNightly backup\nAll volumes done";

    fn block_text(msg: &ChatMessage) -> String {
        msg.blocks
            .iter()
            .find_map(|b| match b {
                MessageBlock::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    fn idle_active_bucket(app: &mut App) {
        if let Some(key) = app.active_session_key.clone()
            && let Some(bucket) = app.sessions.get_mut(&key)
        {
            bucket.lifecycle_state = SessionLifecycleState::Idle;
        }
    }

    fn active_lifecycle(app: &App) -> SessionLifecycleState {
        let key = app.active_session_key.clone().expect("active key");
        app.sessions.get(&key).expect("bucket").lifecycle_state
    }

    /// Bug A: a peer reply that arrives while an assistant turn is in
    /// flight must APPEND at the tail, not be inserted above the
    /// in-flight assistant (which carries the outbound send).
    #[test]
    fn peer_reply_appends_at_tail_not_above_in_flight_send() {
        let mut app = App::test_default();
        app.active_messages_mut().push(ChatMessage::new(
            MessageRole::User,
            vec![MessageBlock::Text(TextBlock::from_complete("orchestrate the workers"))],
            None,
        ));
        // The assistant turn holding the outbound ask is still active.
        app.active_messages_mut().push(ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::Text(TextBlock::from_complete("asking planner..."))],
            None,
        ));
        app.set_active_turn_assistant_message_idx(Some(1));

        handle_user(&mut app, delivered_user_turn(PEER_REPLY));

        // user(0), in-flight asst(1), reply(2), fresh placeholder(3).
        assert_eq!(app.messages().len(), 4, "user + in-flight asst + reply + fresh placeholder");
        assert!(
            matches!(app.messages()[1].role, MessageRole::Assistant),
            "the in-flight assistant send stays at idx 1 - the reply is NOT inserted above it",
        );
        assert!(
            app.messages()[2].is_peer_envelope,
            "the peer reply lands below the in-flight send (idx 2) in arrival order",
        );
        assert!(block_text(&app.messages()[2]).contains("Done."), "reply body on the peer turn");
        assert!(
            matches!(app.messages()[3].role, MessageRole::Assistant)
                && app.messages()[3].blocks.is_empty(),
            "a fresh empty assistant placeholder opens at the tail for the spinner",
        );
        assert_eq!(
            app.active_turn_assistant_message_idx(),
            Some(3),
            "pointer reparents onto the tail placeholder (spinner pins to the bottom)",
        );
    }

    /// Bug A extends to gotify: the external notification also appends
    /// at the tail, never repositioned above an in-flight turn.
    #[test]
    fn gotify_notification_appends_at_tail_not_above_in_flight_turn() {
        let mut app = App::test_default();
        app.active_messages_mut().push(ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::Text(TextBlock::from_complete("mid response..."))],
            None,
        ));
        app.set_active_turn_assistant_message_idx(Some(0));

        handle_user(&mut app, delivered_user_turn(GOTIFY_NOTE));

        // in-flight asst(0), gotify note(1), fresh placeholder(2).
        assert_eq!(app.messages().len(), 3, "in-flight asst + gotify note + fresh placeholder");
        assert!(
            matches!(app.messages()[0].role, MessageRole::Assistant),
            "assistant turn stays at idx 0",
        );
        assert!(
            app.messages()[1].is_gotify_envelope,
            "gotify note appends below the in-flight turn (idx 1) in arrival order",
        );
        assert!(
            matches!(app.messages()[2].role, MessageRole::Assistant)
                && app.messages()[2].blocks.is_empty(),
            "a fresh empty assistant placeholder opens at the tail for the spinner",
        );
        assert_eq!(app.active_turn_assistant_message_idx(), Some(2));
    }

    /// Bug B: a delivered peer prompt flips the chat status to Thinking
    /// and the bucket lifecycle to Running immediately, so the session
    /// reads as active instead of idle-then-burst.
    #[test]
    fn delivered_peer_prompt_shows_running_indicator() {
        let mut app = App::test_default();
        idle_active_bucket(&mut app);
        app.status = AppStatus::Ready;

        handle_user(&mut app, delivered_user_turn(PEER_REPLY));

        assert!(
            matches!(app.status, AppStatus::Thinking),
            "chat status flips to Thinking the moment the delivered prompt lands",
        );
        assert_eq!(
            active_lifecycle(&app),
            SessionLifecycleState::Running,
            "the Projects-pane row spins while the agent works the delivered prompt",
        );
    }

    /// Bug B for gotify: the same running indicator fires for a
    /// delivered gotify notification.
    #[test]
    fn delivered_gotify_note_shows_running_indicator() {
        let mut app = App::test_default();
        idle_active_bucket(&mut app);
        app.status = AppStatus::Ready;

        handle_user(&mut app, delivered_user_turn(GOTIFY_NOTE));

        assert!(matches!(app.status, AppStatus::Thinking));
        assert_eq!(active_lifecycle(&app), SessionLifecycleState::Running);
    }

    /// Replay gate: walking on-disk history through this dispatcher must
    /// NOT flip a resumed bucket to Running (no balancing Result would
    /// arrive to flip it back - the stuck-spinner failure mode).
    #[test]
    fn replayed_peer_envelope_does_not_flip_running() {
        let mut app = App::test_default();
        idle_active_bucket(&mut app);
        app.status = AppStatus::Ready;
        app.replay_in_progress = true;

        handle_user(&mut app, delivered_user_turn(PEER_REPLY));

        assert_eq!(
            active_lifecycle(&app),
            SessionLifecycleState::Idle,
            "replayed peer envelope must NOT flip lifecycle to Running",
        );
        assert!(
            matches!(app.status, AppStatus::Ready),
            "replayed peer envelope must NOT flip status to Thinking",
        );
    }

    /// Spinner position: a delivered prompt landing on an idle session
    /// (prior turn completed -> pointer cleared) must open a fresh
    /// assistant placeholder at the tail and bind the active-turn
    /// pointer onto it, so the thinking spinner pins to the bottom
    /// (above the input) exactly like a typed turn - not on the stale
    /// prior reply near the top.
    #[test]
    fn delivered_peer_prompt_opens_tail_placeholder_and_binds_spinner() {
        let mut app = App::test_default();
        idle_active_bucket(&mut app);
        app.status = AppStatus::Ready;
        app.active_messages_mut().push(ChatMessage::new(
            MessageRole::User,
            vec![MessageBlock::Text(TextBlock::from_complete("prior prompt"))],
            None,
        ));
        app.active_messages_mut().push(ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::Text(TextBlock::from_complete("prior reply"))],
            None,
        ));
        // TurnComplete clears the pointer (turn.rs); model that idle state.
        app.clear_active_turn_assistant();

        handle_user(&mut app, delivered_user_turn(PEER_REPLY));

        let tail = app.messages().len() - 1;
        assert!(
            matches!(app.messages()[tail].role, MessageRole::Assistant)
                && app.messages()[tail].blocks.is_empty(),
            "delivered turn opens a fresh empty assistant placeholder at the tail",
        );
        assert_eq!(
            app.active_turn_assistant_message_idx(),
            Some(tail),
            "pointer targets the tail placeholder so the spinner pins to the \
             bottom, not the stale prior reply",
        );
    }

    /// Spinner position, gotify variant: an idle-delivered gotify note
    /// opens the same tail placeholder + pointer binding.
    #[test]
    fn delivered_gotify_note_opens_tail_placeholder_and_binds_spinner() {
        let mut app = App::test_default();
        idle_active_bucket(&mut app);
        app.status = AppStatus::Ready;
        app.clear_active_turn_assistant();

        handle_user(&mut app, delivered_user_turn(GOTIFY_NOTE));

        let tail = app.messages().len() - 1;
        assert!(
            matches!(app.messages()[tail].role, MessageRole::Assistant)
                && app.messages()[tail].blocks.is_empty(),
            "gotify delivery opens a fresh empty assistant placeholder at the tail",
        );
        assert_eq!(app.active_turn_assistant_message_idx(), Some(tail));
    }

    /// Spinner position, mid-turn "stale index" case: a delivered prompt
    /// arriving while an assistant turn is in flight repoints the pointer
    /// OFF the in-flight assistant onto a fresh tail placeholder (mirrors
    /// input_submit's mid-turn reparent), so the spinner follows the new
    /// turn to the bottom instead of pinning to the in-flight send.
    #[test]
    fn delivered_prompt_reparents_spinner_off_in_flight_assistant() {
        let mut app = App::test_default();
        app.active_messages_mut().push(ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::Text(TextBlock::from_complete("asking planner..."))],
            None,
        ));
        app.set_active_turn_assistant_message_idx(Some(0));

        handle_user(&mut app, delivered_user_turn(PEER_REPLY));

        let tail = app.messages().len() - 1;
        assert_ne!(
            app.active_turn_assistant_message_idx(),
            Some(0),
            "pointer no longer bound to the stale in-flight assistant",
        );
        assert_eq!(
            app.active_turn_assistant_message_idx(),
            Some(tail),
            "pointer reparented onto the fresh tail placeholder",
        );
        assert!(
            matches!(app.messages()[tail].role, MessageRole::Assistant)
                && app.messages()[tail].blocks.is_empty(),
        );
    }

    /// Rapid back-to-back delivery (a Gotify flood / burst of peer
    /// replies): a second delivered turn arriving before the first
    /// streamed any token must strip the first, now-stranded, empty
    /// placeholder - so deliveries don't accumulate blank assistant
    /// bubbles between them. Mirrors input_submit's rapid-submit strip.
    #[test]
    fn back_to_back_delivery_strips_stranded_empty_placeholder() {
        let mut app = App::test_default();
        idle_active_bucket(&mut app);
        app.status = AppStatus::Ready;
        app.clear_active_turn_assistant();

        // First delivery opens [gotify1, placeholder1(empty)].
        handle_user(&mut app, delivered_user_turn(GOTIFY_NOTE));
        // Second delivery lands before any token streamed into placeholder1.
        handle_user(&mut app, delivered_user_turn(PEER_REPLY));

        // placeholder1 stripped -> [gotify1, reply2, placeholder2]; no
        // blank bubble stranded between the two delivered turns.
        assert_eq!(app.messages().len(), 3, "stranded empty placeholder stripped");
        assert!(app.messages()[0].is_gotify_envelope, "first delivered turn");
        assert!(app.messages()[1].is_peer_envelope, "second delivered turn, adjacent");
        assert!(
            matches!(app.messages()[2].role, MessageRole::Assistant)
                && app.messages()[2].blocks.is_empty(),
            "exactly one empty placeholder, at the tail",
        );
        assert_eq!(
            app.active_turn_assistant_message_idx(),
            Some(2),
            "pointer on the surviving tail placeholder",
        );
    }
}

#[cfg(test)]
mod background_tasks_changed_tests {
    //! `background_tasks_changed` carries the CLI's authoritative
    //! snapshot of every backgrounded task. Each event REPLACES the
    //! session's list wholesale; an empty `tasks` array clears it so
    //! the PROCESSES `local_bash` feed drains.
    use super::handle_sdk_message;
    use crate::app::App;
    use forge_primitives::Message;
    use serde_json::json;

    fn background_event(tasks: Vec<serde_json::Value>) -> Message {
        Message::BackgroundTasksChanged { tasks, uuid: String::new(), session_id: String::new() }
    }

    fn background_tasks(app: &App) -> &[crate::app::state::types::BackgroundTask] {
        app.active_session().map_or(&[], |s| s.background_tasks.as_slice())
    }

    #[test]
    fn snapshot_populates_then_empty_snapshot_clears() {
        let mut app = App::test_default();
        handle_sdk_message(
            &mut app,
            background_event(vec![json!({
                "task_id": "b3cjfmhsq",
                "task_type": "local_bash",
                "description": "Print marker after 1s in background",
            })]),
        );
        let tasks = background_tasks(&app);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].task_id, "b3cjfmhsq");
        assert_eq!(tasks[0].task_type, "local_bash");
        assert_eq!(tasks[0].description, "Print marker after 1s in background");

        // Full-snapshot semantics: an empty array replaces the list
        // with nothing.
        handle_sdk_message(&mut app, background_event(Vec::new()));
        assert!(background_tasks(&app).is_empty());
    }

    #[test]
    fn later_snapshot_replaces_rather_than_appends() {
        let mut app = App::test_default();
        handle_sdk_message(
            &mut app,
            background_event(vec![json!({
                "task_id": "first",
                "task_type": "local_bash",
                "description": "one",
            })]),
        );
        handle_sdk_message(
            &mut app,
            background_event(vec![json!({
                "task_id": "second",
                "task_type": "agent",
                "description": "two",
            })]),
        );
        let tasks = background_tasks(&app);
        assert_eq!(tasks.len(), 1, "each event replaces the whole list");
        assert_eq!(tasks[0].task_id, "second");
        assert_eq!(tasks[0].task_type, "agent");
    }

    #[test]
    fn malformed_entries_are_skipped_not_panicked() {
        let mut app = App::test_default();
        handle_sdk_message(
            &mut app,
            background_event(vec![
                json!({"task_id": "ok", "task_type": "local_bash", "description": "good"}),
                json!({"task_type": "local_bash"}),
                json!("not-an-object"),
            ]),
        );
        let tasks = background_tasks(&app);
        assert_eq!(tasks.len(), 1, "malformed entries dropped, valid one kept");
        assert_eq!(tasks[0].task_id, "ok");
    }

    fn map_has(app: &App, task_id: &str) -> bool {
        app.active_session().is_some_and(|s| s.session_task_tool_use_ids.contains_key(task_id))
    }

    #[test]
    fn bash_leaving_roster_drops_its_session_map_entry() {
        let mut app = App::test_default();
        // A rostered non-agent (bash) gets no task_notification, so the roster
        // diff is its only map cleanup. task_started recorded the resolver; the
        // task is then listed in the roster.
        app.insert_session_task_mapping("task-bash".to_owned(), "tu-bash".to_owned());
        handle_sdk_message(
            &mut app,
            background_event(vec![json!({
                "task_id": "task-bash",
                "task_type": "local_bash",
                "description": "sleep 60",
            })]),
        );
        assert!(map_has(&app, "task-bash"), "still rostered -> resolver kept");

        // Leaving the roster drops its resolver in the same event.
        handle_sdk_message(&mut app, background_event(Vec::new()));
        assert!(!map_has(&app, "task-bash"), "a bash that left the roster loses its map entry");
    }

    #[test]
    fn session_map_entry_before_first_roster_event_survives() {
        let mut app = App::test_default();
        // task_started can precede a task's first roster event, so a snapshot
        // that doesn't list it yet must NOT drop the resolver (diff old-vs-new,
        // not prune-all).
        app.insert_session_task_mapping("task-y".to_owned(), "tu-y".to_owned());
        handle_sdk_message(
            &mut app,
            background_event(vec![json!({
                "task_id": "task-other",
                "task_type": "local_bash",
                "description": "o",
            })]),
        );
        assert!(map_has(&app, "task-y"), "a not-yet-rostered task keeps its resolver");
    }
}

#[cfg(test)]
mod subagent_sentinel_tests {
    //! A backgrounded subagent gets a terminal `task_updated` (the
    //! backgrounding sentinel) that flips its root card `Completed`
    //! seconds before its true completion (`task_notification`). The
    //! session roster - the `background_tasks` registry intersected with
    //! the session task map - is the liveness gate, so the SUBAGENTS row
    //! must survive the sentinel and clear only at the subagent's
    //! `task_notification`.
    use super::handle_sdk_message;
    use crate::agent::model::ToolCallStatus;
    use crate::app::TerminalSnapshotMode;
    use crate::app::state::types::ToolCallScope;
    use crate::app::{App, BlockCache, ChatMessage, MessageBlock, MessageRole, ToolCallInfo};
    use forge_primitives::messages::TaskUpdatePatch;
    use forge_primitives::{Message, TaskNotificationStatus};

    fn subagent_tool_call(
        id: &str,
        sdk_tool_name: &str,
        title: &str,
        raw_input: Option<serde_json::Value>,
        hidden: bool,
        status: ToolCallStatus,
    ) -> ToolCallInfo {
        ToolCallInfo {
            id: id.to_owned(),
            title: title.to_owned(),
            sdk_tool_name: sdk_tool_name.to_owned(),
            raw_input,
            raw_input_bytes: 0,
            output_metadata: None,
            task_metadata: None,
            status,
            content: Vec::new(),
            hidden,
            terminal_id: None,
            terminal_command: None,
            terminal_output: None,
            terminal_output_len: 0,
            terminal_bytes_seen: 0,
            terminal_snapshot_mode: TerminalSnapshotMode::AppendOnly,
            monitor_output_tail: Vec::default(),
            render_epoch: 0,
            layout_epoch: 0,
            last_measured_width: 0,
            last_measured_height: 0,
            last_measured_layout_epoch: 0,
            last_measured_layout_generation: 0,
            cache: BlockCache::default(),
            collapsed_override: None,
            last_measured_y_in_msg: 0,
            answered_questions: Vec::new(),
        }
    }

    /// Seed a backgrounded subagent whose spawning turn already Result'd:
    /// root + one child in the message list plus the session task-map entry.
    /// The root's card reads `Completed` because the backgrounding sentinel
    /// already flipped it. The caller sets the roster separately.
    fn seed_subagent(app: &mut App, task_id: &str, root_id: &str, child_id: &str) {
        let root = subagent_tool_call(
            root_id,
            "Task",
            "Task",
            Some(serde_json::json!({
                "subagent_type": "Explore",
                "description": "long-running background scan",
                "prompt": "long-running background scan",
            })),
            false,
            ToolCallStatus::Completed,
        );
        let child = subagent_tool_call(
            child_id,
            "Read",
            "conv-row.tsx",
            None,
            true,
            ToolCallStatus::Completed,
        );
        app.register_tool_call_scope(root_id.to_owned(), ToolCallScope::SubagentRoot);
        app.register_tool_call_scope(
            child.id.clone(),
            ToolCallScope::SubagentChild { parent_tool_use_id: root_id.to_owned() },
        );
        app.push_message_tracked(ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::ToolCall(Box::new(root)), MessageBlock::ToolCall(Box::new(child))],
            None,
        ));
        app.insert_session_task_mapping(task_id.to_owned(), root_id.to_owned());
    }

    fn terminal_task_updated(task_id: &str) -> Message {
        Message::TaskUpdated {
            task_id: task_id.to_owned(),
            patch: TaskUpdatePatch { status: Some("completed".to_owned()), end_time: None },
            uuid: String::new(),
            session_id: String::new(),
        }
    }

    fn task_notification(task_id: &str, root_id: &str) -> Message {
        Message::TaskNotification {
            task_id: task_id.to_owned(),
            status: TaskNotificationStatus::Completed,
            output_file: String::new(),
            summary: "done".to_owned(),
            uuid: String::new(),
            session_id: String::new(),
            tool_use_id: Some(root_id.to_owned()),
            usage: None,
        }
    }

    fn roster_changed(task_ids: &[&str]) -> Message {
        Message::BackgroundTasksChanged {
            tasks: task_ids
                .iter()
                .map(|id| {
                    serde_json::json!({
                        "task_id": id,
                        "task_type": "local_agent",
                        "description": "long-running background scan",
                    })
                })
                .collect(),
            uuid: String::new(),
            session_id: String::new(),
        }
    }

    fn map_has(app: &App, task_id: &str) -> bool {
        app.active_session().is_some_and(|s| s.session_task_tool_use_ids.contains_key(task_id))
    }

    #[test]
    fn sentinel_keeps_subagent_until_task_notification() {
        let mut app = App::test_default();
        let task_id = "task-sub";
        let root_id = "tu-root-sub";
        seed_subagent(&mut app, task_id, root_id, "tu-child-1");
        handle_sdk_message(&mut app, roster_changed(&[task_id]));

        let view = app.subagents_view();
        assert_eq!(view.len(), 1, "baseline: backgrounded subagent visible; got {view:?}");
        assert_eq!(view[0].status, ToolCallStatus::InProgress);
        assert_eq!(view[0].label, "Explore \u{b7} long-running background scan");

        // The sentinel arrives while the roster still lists the task.
        handle_sdk_message(&mut app, terminal_task_updated(task_id));

        let view = app.subagents_view();
        assert_eq!(
            view.len(),
            1,
            "the terminal task_updated sentinel must not evict a still-registered subagent; got {view:?}",
        );
        assert_eq!(
            view[0].status,
            ToolCallStatus::InProgress,
            "subagent still renders running through the sentinel; got {:?}",
            view[0].status,
        );
        assert_eq!(view[0].label, "Explore \u{b7} long-running background scan");
        assert_eq!(
            view[0].tail.len(),
            1,
            "its live tool tail is preserved; got {:?}",
            view[0].tail
        );

        // task_notification is a subagent's true completion: it clears the row
        // and drops the map entry even while the roster still lists the task.
        handle_sdk_message(&mut app, task_notification(task_id, root_id));
        assert!(
            app.subagents_view().is_empty(),
            "task_notification clears the SUBAGENTS row; got {:?}",
            app.subagents_view(),
        );
        assert!(!map_has(&app, task_id), "task_notification drops the agent's map entry");
    }

    #[test]
    fn roster_drop_evicts_only_the_departed_subagent() {
        let mut app = App::test_default();
        seed_subagent(&mut app, "task-a", "tu-root-a", "tu-child-a");
        seed_subagent(&mut app, "task-b", "tu-root-b", "tu-child-b");
        handle_sdk_message(&mut app, roster_changed(&["task-a", "task-b"]));
        let view = app.subagents_view();
        assert_eq!(view.len(), 2, "both backgrounded subagents visible; got {view:?}");
        assert!(
            view.iter().all(|e| e.status == ToolCallStatus::InProgress),
            "both render running while rostered; got {view:?}",
        );

        // Only task-b leaves the roster; task-a must be untouched.
        handle_sdk_message(&mut app, roster_changed(&["task-a"]));

        let view = app.subagents_view();
        let entry_a = view.iter().find(|e| e.tool_use_id == "tu-root-a").expect("root-a present");
        let entry_b = view.iter().find(|e| e.tool_use_id == "tu-root-b").expect("root-b present");
        assert_eq!(
            entry_a.status,
            ToolCallStatus::InProgress,
            "the still-listed subagent stays running; got {:?}",
            entry_a.status,
        );
        assert_eq!(
            entry_b.status,
            ToolCallStatus::Completed,
            "the departed subagent's liveness is evicted (its terminal card lingers only because a sibling is live); got {:?}",
            entry_b.status,
        );
        assert!(map_has(&app, "task-a"), "the still-listed subagent keeps its map entry");
        assert!(!map_has(&app, "task-b"), "the departed subagent loses its map entry");
    }
}

#[cfg(test)]
mod commands_changed_tests {
    //! `commands_changed` carries the fresh slash-command list after a
    //! plugin/command reload. It must REPLACE the session's
    //! `available_commands` (feeding the `/` dropdown + `/help`), not
    //! append to it.
    use super::handle_sdk_message;
    use crate::app::App;
    use forge_primitives::{AvailableCommand, Message};
    use serde_json::json;

    fn commands_event(commands: Vec<serde_json::Value>) -> Message {
        Message::CommandsChanged { commands, uuid: String::new(), session_id: String::new() }
    }

    #[test]
    fn refresh_replaces_the_command_list() {
        let mut app = App::test_default();
        *app.available_commands_mut() = vec![AvailableCommand::new("stale", "")];

        handle_sdk_message(
            &mut app,
            commands_event(vec![
                json!({"name": "gateway-upgrade", "description": "upgrade flow", "argumentHint": ""}),
                json!({"name": "greptile", "description": "code search", "argumentHint": "<query>"}),
            ]),
        );

        let names: Vec<&str> = app.available_commands().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["gateway-upgrade", "greptile"], "list replaced, not appended");
        // argumentHint parses into input_hint; an empty hint drops to None.
        let greptile =
            app.available_commands().iter().find(|c| c.name == "greptile").expect("greptile");
        assert_eq!(greptile.input_hint.as_deref(), Some("<query>"));
        let gateway =
            app.available_commands().iter().find(|c| c.name == "gateway-upgrade").expect("gateway");
        assert_eq!(gateway.input_hint, None, "empty argumentHint collapses to None");
    }

    #[test]
    fn helper_handles_both_string_and_object_shapes() {
        use super::available_commands_from_json;
        // init `slash_commands` shape: bare name strings -> name-only
        // commands (the pre-refactor init behaviour).
        let from_strings = available_commands_from_json(&[json!("audit"), json!("resume")]);
        assert_eq!(from_strings.len(), 2);
        assert_eq!(from_strings[0].name, "audit");
        assert_eq!(from_strings[0].description, "");
        assert_eq!(from_strings[0].input_hint, None);
        // commands_changed shape: objects; nameless / scalar entries drop.
        let from_objects = available_commands_from_json(&[
            json!({"name": "x", "description": "d", "argumentHint": "<a>"}),
            json!({"description": "no name"}),
            json!(42),
        ]);
        assert_eq!(from_objects.len(), 1, "nameless / scalar entries skipped");
        assert_eq!(from_objects[0].name, "x");
        assert_eq!(from_objects[0].description, "d");
        assert_eq!(from_objects[0].input_hint.as_deref(), Some("<a>"));
        // Empty names are degenerate (a blank, un-selectable dropdown
        // row) - skipped in both the string and object shapes.
        let empties = available_commands_from_json(&[
            json!(""),
            json!({"name": "", "description": "blank"}),
            json!({"name": "real"}),
        ]);
        assert_eq!(empties.len(), 1, "empty-name entries skipped in both shapes");
        assert_eq!(empties[0].name, "real");
    }

    #[test]
    fn non_empty_but_unparseable_payload_keeps_prior_list() {
        // Drift guard (Hard Rule #16): a payload that carries entries
        // but parses to nothing signals the CLI's entry shape changed;
        // applying it would wipe the dropdown + /help. Keep the prior
        // list instead.
        let mut app = App::test_default();
        *app.available_commands_mut() = vec![AvailableCommand::new("keep", "")];
        handle_sdk_message(&mut app, commands_event(vec![json!({"no_name": "x"}), json!(7)]));
        let names: Vec<&str> = app.available_commands().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["keep"], "drift guard keeps the prior list intact");
    }

    #[test]
    fn legit_empty_payload_clears_the_list() {
        // A genuinely empty `commands: []` (e.g. plugin uninstall) is
        // an intended clear, distinct from the drift case above.
        let mut app = App::test_default();
        *app.available_commands_mut() = vec![AvailableCommand::new("gone", "")];
        handle_sdk_message(&mut app, commands_event(Vec::new()));
        assert!(app.available_commands().is_empty(), "empty commands_changed clears the list");
    }
}

#[cfg(test)]
mod error_message_tests {
    //! `Message::Error` is the CLI's fatal transport signal. It must
    //! route through the turn-error path (surface the error, leave the
    //! pinned-spinner state), not fall into a no-op arm.
    use super::handle_sdk_message;
    use crate::app::{App, ChatMessage, MessageBlock, MessageRole, TextBlock};
    use forge_primitives::Message;

    #[test]
    fn error_message_surfaces_and_drops_the_stuck_spinner() {
        let mut app = App::test_default();
        app.status = crate::app::AppStatus::Thinking;
        app.active_messages_mut().push(ChatMessage::new(
            MessageRole::User,
            vec![MessageBlock::Text(TextBlock::from_complete("hi"))],
            None,
        ));
        app.active_messages_mut().push(ChatMessage::new(MessageRole::Assistant, Vec::new(), None));

        handle_sdk_message(&mut app, Message::Error { error: "read loop died".to_owned() });

        // The empty tail assistant is replaced by a surfaced system
        // error - proof the frame took the turn-error path, not the
        // old no-op arm that left it stuck.
        let last = app.messages().last().expect("a message remains");
        assert!(
            matches!(last.role, MessageRole::System(None)),
            "fatal error surfaces as a system message, got {:?}",
            last.role,
        );
    }
}

#[cfg(test)]
mod finalize_open_tool_calls_tests {
    //! The turn-end sweep force-completes lingering tool calls, EXCEPT
    //! persistent monitors and backgrounded tasks the CLI still lists as
    //! running - both outlive the turn and settle via their own lifecycle
    //! events, so flipping their cards terminal here paints an unearned
    //! checkmark.
    use super::finalize_open_tool_calls;
    use crate::app::App;
    use crate::app::state::types::BackgroundTask;
    use forge_primitives::ToolCallStatus;
    use forge_primitives::session_update::{ToolCall, ToolKind};
    use serde_json::json;

    fn turn_tool(id: &str, title: &str, raw_input: serde_json::Value) -> ToolCall {
        ToolCall {
            tool_call_id: id.to_owned(),
            title: title.to_owned(),
            kind: ToolKind::Other,
            status: ToolCallStatus::InProgress,
            content: Vec::new(),
            raw_input: Some(raw_input),
            raw_output: None,
            output_metadata: None,
            task_metadata: None,
            locations: Vec::new(),
            meta: None,
        }
    }

    #[test]
    fn exempts_persistent_monitor_and_roster_backgrounded_but_sweeps_ordinary() {
        let mut app = App::test_default();
        let _: () = app.with_turn_state_mut(|ts| {
            ts.tool_calls.insert(
                "tu-monitor".to_owned(),
                turn_tool("tu-monitor", "Monitor", json!({ "persistent": true })),
            );
            ts.tool_calls.insert(
                "tu-bash".to_owned(),
                turn_tool(
                    "tu-bash",
                    "Bash",
                    json!({ "command": "sleep 60", "run_in_background": true }),
                ),
            );
            ts.tool_calls.insert(
                "tu-agent".to_owned(),
                turn_tool("tu-agent", "Task", json!({ "subagent_type": "Explore" })),
            );
            ts.tool_calls
                .insert("tu-read".to_owned(), turn_tool("tu-read", "Read", json!({ "file": "x" })));
        });
        // Roster lists the bash + agent as still running; the session map
        // resolves each task_id back to its tool_use_id.
        app.insert_session_task_mapping("task-bash".to_owned(), "tu-bash".to_owned());
        app.insert_session_task_mapping("task-agent".to_owned(), "tu-agent".to_owned());
        *app.background_tasks_mut() = vec![
            BackgroundTask {
                task_id: "task-bash".to_owned(),
                task_type: "local_bash".to_owned(),
                description: String::new(),
            },
            BackgroundTask {
                task_id: "task-agent".to_owned(),
                task_type: "local_agent".to_owned(),
                description: String::new(),
            },
        ];

        finalize_open_tool_calls(&mut app, ToolCallStatus::Completed);

        let status = |id: &str| app.with_turn_state(|ts| ts.tool_calls.get(id).map(|t| t.status));
        assert_eq!(
            status("tu-monitor"),
            Some(ToolCallStatus::InProgress),
            "persistent monitor is left running",
        );
        assert_eq!(
            status("tu-bash"),
            Some(ToolCallStatus::InProgress),
            "roster-backgrounded bash is left running",
        );
        assert_eq!(
            status("tu-agent"),
            Some(ToolCallStatus::InProgress),
            "roster-backgrounded agent root is left running",
        );
        assert_eq!(
            status("tu-read"),
            Some(ToolCallStatus::Completed),
            "an ordinary in-flight tool is still swept to terminal",
        );
    }
}
