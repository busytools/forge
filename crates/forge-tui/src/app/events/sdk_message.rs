//! Direct `forge_primitives::Message` consumer for the App.
//!
//! [`handle_sdk_message`] is the App-side dispatcher that receives
//! raw `forge_primitives::Message` envelopes from the bridge worker
//! and routes them to per-variant handlers below. Each handler
//! destructures the typed message variant directly and mutates App
//! state. `Message::System { data: Value, .. }` is the one variant
//! that still walks JSON — its subtype shapes aren't first-class on
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
        // Streaming partial-message events and the transport-layer
        // `Error` / forward-compat `Unknown` frames are no-ops today.
        // Re-add a handler if a downstream consumer needs to react.
        Message::StreamEvent { .. } | Message::Error { .. } | Message::Unknown { .. } => {}
        // #273: typed wrappers around the CLI 2.1.156 system events.
        Message::ThinkingTokens { estimated_tokens, .. } => {
            handle_thinking_tokens(app, estimated_tokens);
        }
        Message::TurnDuration { ms, .. } => handle_turn_duration(app, ms),
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

/// #273: Persist the just-completed turn's wall-clock duration so the
/// assistant banner chip `Claude · N.Ns` shows on the active turn's
/// banner row. The field is per-session and survives across turns
/// (each turn overwrites with its own duration on completion).
fn handle_turn_duration(app: &mut App, ms: u64) {
    app.set_last_turn_duration_ms(Some(ms));
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
/// Idempotent — same state in re-applies as a no-op.
///
/// Converts the wire-side `forge_primitives::FastModeState` to the
/// App-side `model::FastModeState`. Both enums share the same
/// variant set; the conversion is a 1:1 match.
fn apply_fast_mode_state(app: &mut App, wire_state: forge_primitives::FastModeState) {
    use crate::agent::model::FastModeState as Model;
    use forge_primitives::FastModeState as Wire;
    let model_state = match wire_state {
        Wire::Off => Model::Off,
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
    // continuation from claude before the user looked at it) —
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
    // must not flip lifecycle to Running — otherwise an
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
    // Outer-envelope error capture — `app.turn_state.last_assistant_error`
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

    for block in content {
        match block {
            ContentBlock::Text { text } => {
                if text.is_empty() {
                    continue;
                }
                super::clear_compaction_state(app, true);
                let chunk = model::ContentChunk::new(model::ContentBlock::Text(
                    model::TextContent::new(text.clone()),
                ));
                super::streaming::handle_agent_message_chunk(app, chunk);
            }
            ContentBlock::Thinking { thinking, .. } => {
                if thinking.is_empty() {
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
            ContentBlock::Unknown { type_str, raw }
                if forge_workspace::tooling::is_tool_result_block_type(type_str) =>
            {
                // Wire-side tool-result variants the typed enum
                // doesn't enumerate: `mcp_tool_result`,
                // `web_fetch_tool_result`, etc. (full set lives in the
                // tooling module's `TOOL_RESULT_TYPES`). They share the
                // `tool_use_id` + `content` + `is_error` shape — pull
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
                // Symmetric coverage with the user-content walker —
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
    walk_user_tool_results(app, &message.content);
    // Peer-coordination user-turn echoes (#114). `Command::Prompt`
    // dispatched via `Workspace::deliver_peer_prompt` injects the
    // wrapped envelope into the target session's CLI as a user turn.
    // The CLI echoes it back as `Message::User`, but the input-submit
    // local-push convention (the typed-user case already pushed the
    // bubble) doesn't apply here — no local typing happened, so the
    // chat buffer never sees the user-turn unless we push it from
    // the SDK echo. Detected via the peer-wrapper prefix so non-peer
    // user echoes (the existing pattern) keep their no-push behavior.
    push_peer_envelope_user_turn_if_present(app, &message.content);
    // Sub-agent tool_use_result envelopes carry parent_tool_use_id at
    // the message level — wire the implicit parent linkage so the
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
/// The detection key is `peer_block::detect_inbound` — same matcher
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
        if crate::ui::peer_block::detect_inbound(text).is_none() {
            continue;
        }
        let blocks = vec![MessageBlock::Text(TextBlock::from_complete(text))];
        // #143 item 2: cache the peer-envelope flag at push time so
        // the chat renderer doesn't walk text blocks every frame.
        let msg = ChatMessage::new_peer_envelope(MessageRole::User, blocks, None);
        // When forge is mid-turn there's an empty assistant placeholder
        // at the tail (input_submit::dispatch_prompt pushed it before
        // the response stream started). A blind push appends the peer
        // user-turn AFTER the placeholder, which sandwiches the chat
        // visually: assistant placeholder up top, peer reply below,
        // then the streamed response eventually fills the placeholder
        // above the reply. Insert BEFORE the active placeholder so the
        // chronology reads cleanly: [previous user] → [peer reply] →
        // [assistant placeholder that will fill in].
        let placeholder_idx = app.active_turn_assistant_message_idx();
        match placeholder_idx {
            Some(idx) if idx <= app.messages().len() => {
                app.insert_message_tracked(idx, msg);
                // Re-bind the active turn pointer: the placeholder is
                // now one slot further down because we inserted before
                // it.
                app.set_active_turn_assistant_message_idx(Some(idx + 1));
            }
            _ => {
                app.push_message_tracked(msg);
            }
        }
        app.enforce_history_retention_tracked();
        return;
    }
}

/// Walk the typed `Message::User` content blocks and apply
/// tool_result blocks via `apply_tool_result_block`.
fn walk_user_tool_results(app: &mut App, content: &[forge_primitives::ContentBlock]) {
    use forge_primitives::ContentBlock;

    for block in content {
        match block {
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
            ContentBlock::Unknown { type_str, raw }
                if forge_workspace::tooling::is_tool_result_block_type(type_str) =>
            {
                // Same fallback as `walk_assistant_content` — wire
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
                // a `queued_command` content block — match against
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
/// This is invoked twice — once for the user-content walker (live
/// mid-turn / replay), once for the assistant-content walker (edge
/// case).
pub(super) fn extract_queued_command_text(prompt: &Value) -> String {
    if let Some(s) = prompt.as_str() {
        return s.to_owned();
    }
    let Some(blocks) = prompt.as_array() else {
        // Object or other — render as JSON literal so the user can
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
/// stream-json stdout — it only persists those messages to the
/// session JSONL as `type:"attachment"` rows. The replay scanner in
/// `forge_agent::userdata::catalog::scan` hoists those rows into
/// synthetic user envelopes carrying a single `queued_command`
/// content block each. So in practice this walker only runs during
/// session resume; live mid-turn submits never hit it.
///
/// Action: push a regular user bubble. (Live mid-turn submits
/// already pushed their own bubble at submit time — see
/// `input_submit::dispatch_prompt` — and never reach this code.)
pub(super) fn handle_queued_command_echo(app: &mut App, prompt_text: &str) {
    use crate::app::{ChatMessage, MessageBlock, MessageRole, TextBlock};
    // Harness-injected `<task-notification>` blobs (background-task
    // completion events) get queued through the same path as
    // user-typed input. They're plumbing, not a user message — render
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
fn finalize_open_tool_calls(app: &mut App, status: forge_primitives::ToolCallStatus) {
    use crate::app::state::tool_call_info::is_monitor_tool_name;
    use forge_primitives::{ToolCallStatus, ToolCallUpdateFields};

    let pending: Vec<String> = app.with_turn_state(|ts| {
        ts.tool_calls
            .iter()
            .filter(|(_, t)| {
                matches!(t.status, ToolCallStatus::Pending | ToolCallStatus::InProgress)
            })
            .filter(|(_, t)| {
                // Skip explicit persistent monitors — the docs and
                // wire shape both say these outlive the turn that
                // started them.
                if raw_input_is_persistent(t.raw_input.as_ref()) {
                    return false;
                }
                // Defensive: when raw_input is None for a Monitor-named
                // tool, we can't tell if it's persistent yet — the
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
/// be treated as non-persistent — that would race against the
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
            // and refresh `supported_mode_ids` ourselves — otherwise
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

/// Build `AvailableCommandsUpdate` from System(init).slash_commands.
fn apply_available_commands_from_init(app: &mut App, data: &Value) {
    let Some(record) = data.as_object() else { return };
    let Some(arr) = record.get("slash_commands").and_then(Value::as_array) else { return };
    let commands: Vec<forge_primitives::AvailableCommand> = arr
        .iter()
        .filter_map(|v| v.as_str())
        .map(|name| forge_primitives::AvailableCommand {
            name: name.to_owned(),
            description: String::new(),
            input_hint: None,
        })
        .collect();
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
/// Reuse `app.available_models` — re-deriving from the init payload's
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
        let task_id_owned = task_id.clone();
        let _: () = app.with_turn_state_mut(|ts| {
            ts.task_tool_use_ids.insert(task_id.clone(), id_owned);
            // Mark the task alive — drained by handle_task_updated
            // when patch.status is terminal. This drives PROCESSES
            // visibility: a backgrounded Bash whose tool_result has
            // already arrived (flipping tc.status to Completed) is
            // still alive here until its task_updated terminal
            // patch lands.
            ts.alive_task_ids.insert(task_id_owned);
        });
    }
}

fn handle_task_progress(app: &mut App, msg: Message) {
    let Message::TaskProgress { tool_use_id, .. } = msg else { return };
    let id = tool_use_id.as_deref().unwrap_or("");
    if !id.is_empty() {
        apply_tool_progress_update(app, id, "Task");
    }
}

/// Apply a `TaskUpdated` patch to the originating tool call.
///
/// The wire emits `task_updated` for any long-running tool task
/// (subagent `Task`, backgrounded `Bash`, `Monitor`) carrying a
/// `patch` object with status / end_time deltas. For PROCESSES
/// rendering this is the canonical signal that a backgrounded Bash
/// transitioned from running to completed — without consuming it,
/// the chat-stream tool card and Inspector row both stay stuck on
/// `in_progress` forever.
///
/// Resolution path: `task_started` populates
/// `TurnState::task_tool_use_ids` as `task_id` → `tool_use_id`.
/// This handler reverses the lookup to find which tool call to
/// update. If the mapping is absent (out-of-order arrival or
/// task_started lost), the update is dropped with a debug log —
/// there's no recovery path that doesn't risk corrupting an
/// unrelated tool call.
fn handle_task_updated(app: &mut App, msg: Message) {
    use forge_primitives::ToolCallUpdateFields;

    let Message::TaskUpdated { task_id, patch, .. } = msg else { return };
    let tool_use_id = app.with_turn_state(|ts| ts.task_tool_use_ids.get(&task_id).cloned());
    let Some(tool_use_id) = tool_use_id else {
        tracing::debug!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "task_updated_no_mapping",
            message = "task_updated for unknown task_id — dropped",
            outcome = "dropped",
            task_id = %task_id,
        );
        return;
    };

    let Some(wire_status) = patch.status.as_deref() else {
        // No status delta in this patch (e.g. partial update that
        // only stamped end_time). Nothing for the renderer to do.
        return;
    };
    let mapped_status = map_task_updated_status_to_tool_status(wire_status);
    apply_tool_call_update(
        app,
        &tool_use_id,
        ToolCallUpdateFields { status: Some(mapped_status), ..Default::default() },
    );

    // Drain the alive-task set on terminal transitions so the
    // PROCESSES section can drop the row. Wire vocabulary
    // `completed` / `failed` / `killed` / `stopped` all count as
    // terminal — anything else (`running`, `pending`, etc.) leaves
    // the task in the alive set.
    if matches!(wire_status, "completed" | "failed" | "killed" | "stopped") {
        let task_id_owned = task_id;
        let _: () = app.with_turn_state_mut(|ts| {
            ts.alive_task_ids.remove(&task_id_owned);
        });
    }
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
    let Message::TaskNotification { tool_use_id, summary, .. } = msg else { return };
    let id = tool_use_id.as_deref().unwrap_or("");
    if !id.is_empty() {
        apply_tool_summary_update(app, id, &summary);
    }
}

/// Apply a `TaskProgress` notification to App state — bumps the
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

/// Apply a `TaskNotification` summary to App state — finalises the
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
/// in practice — events arrive via `Result` frames with
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

fn handle_rate_limit_event(app: &mut App, msg: Message) {
    let Message::RateLimitEvent { rate_limit_info, .. } = msg else { return };
    let value = serde_json::to_value(&rate_limit_info).unwrap_or(Value::Null);
    // Per-session config_dir from the workspace's session binding,
    // NOT `std::env::var("CLAUDE_CONFIG_DIR")` — multiple accounts
    // mean each session has its own bound config_dir, distinct from
    // forge's own host config_dir. Reading from env here would log
    // forge's path on every event regardless of which account
    // actually owns this rate-limit signal.
    let config_dir = app
        .workspace
        .as_ref()
        .and_then(|ws| app.active_session_key.as_ref().and_then(|k| ws.config_dir_for(k)))
        .map_or_else(|| "(unbound)".to_owned(), |p| p.to_string_lossy().into_owned());
    // Raw payload at debug — useful for triaging whether a notice
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
    let Message::Result { is_error, subtype, errors, terminal_reason, fast_mode_state, .. } = msg
    else {
        return;
    };
    // #273: Turn ended - clear per-turn thinking-token chip so the
    // next in-progress turn starts with a bare spinner (it'll
    // re-populate once `Message::ThinkingTokens` fires for the new
    // turn). turn_duration + stop_hook_summary stay - they belong
    // to the just-completed turn's banner / end-of-turn surfaces.
    app.set_latest_thinking_tokens(None);
    if let Some(state) = fast_mode_state {
        apply_fast_mode_state(app, state);
    }
    apply_result_finalize(app, is_error, &subtype, errors.unwrap_or_default(), terminal_reason);
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
    // `apply_result_finalize` only runs on the active session — the
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
    // reason). Don't prepend "turn failed:" here — the renderer adds
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

/// App-side classifier for `TurnError` payloads — picks one of the
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
        // persistent-false implicitly — but it also can't be treated
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
mod assistant_lifecycle_gate_tests {
    //! Regression coverage for the launchpad-spinner-stuck bug.
    //!
    //! `handle_assistant` flips `lifecycle = Running` on every assistant
    //! envelope so that switching into a project mid-turn surfaces the
    //! spinning glyph in the Projects pane (see commit `1d30062`). But
    //! `load_resume_history` reuses the same dispatcher to walk on-disk
    //! history, so without a gate every replayed assistant message
    //! flipped a freshly-resumed bucket to Running with no balancing
    //! `Result` to flip it back — the Projects pane row stuck on the
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
            "replayed assistant message must NOT flip lifecycle — that's what \
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
        // Object shape — render as JSON literal so the user sees
        // something rather than blank.
        let prompt = json!({"weird": "shape"});
        let out = extract_queued_command_text(&prompt);
        assert!(out.contains("weird"));
    }
}
