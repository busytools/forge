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
        Message::ThinkingTokens { estimated_tokens_delta, .. } => {
            handle_thinking_tokens(app, estimated_tokens_delta);
        }
        Message::StopHookSummary { actions, hook_infos, .. } => {
            handle_stop_hook_summary(app, actions, hook_infos);
        }
        Message::CompactBoundary { trigger, pre_tokens, .. } => {
            handle_compact_boundary(app, &trigger, pre_tokens);
        }
    }
}

/// #273: Accumulate the turn's estimated thinking tokens and mirror
/// the running total onto the turn's own message.
///
/// Summed from the deltas rather than read off `estimated_tokens`,
/// which restarts at every thinking block and so understates any turn
/// that thought more than once. The session field is cleared at turn
/// end, which is why the row keeps its own copy.
fn handle_thinking_tokens(app: &mut App, estimated_tokens_delta: i64) {
    let delta = u64::try_from(estimated_tokens_delta).unwrap_or_else(|_| {
        // Every delta across the 2.1.220 baselines is non-negative, and
        // a block boundary restarts at the new block's first increment
        // rather than stepping back. A negative one means the field
        // changed meaning, so count nothing rather than guess.
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "thinking_tokens_negative_delta",
            message = "thinking_tokens delta was negative; wire shape may have changed",
            outcome = "ignored",
            estimated_tokens_delta,
        );
        0
    });
    let total = app.latest_thinking_tokens().unwrap_or(0).saturating_add(delta);
    app.set_latest_thinking_tokens(Some(total));
    mirror_thinking_tokens_onto_turn(app, total);
}

/// Write the turn's running estimate onto the message the row renders
/// from, skipping a settled one exactly as the live usage stamp does.
fn mirror_thinking_tokens_onto_turn(app: &mut App, total: u64) {
    let Some(idx) =
        app.messages().iter().rposition(|m| matches!(m.role, crate::app::MessageRole::Assistant))
    else {
        return;
    };
    if let Some(msg) = app.active_messages_mut().get_mut(idx) {
        if msg.turn_info.is_settled() {
            return;
        }
        msg.turn_info.thinking_tokens = Some(total);
        msg.invalidate_render_cache();
    }
    app.invalidate_layout(crate::app::InvalidationLevel::MessageChanged(idx));
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
    // A subagent's envelope is not this bucket's turn. A backgrounded
    // one keeps arriving after the main turn Resulted, and no Result
    // follows it, so flipping the bucket to Running here left it
    // Running for good - the row spinner and the tab title both pulsing
    // with nothing in flight.
    let is_subagent = parent_tool_use_id.as_deref().is_some_and(|p| !p.trim().is_empty());
    if !app.replay_in_progress
        && !is_subagent
        && let Some(key) = app.active_session_key.clone()
    {
        super::set_bucket_lifecycle_state(
            app,
            &key,
            crate::app::session::SessionLifecycleState::Running,
        );
        super::queued_turn::note_turn_started(app, &key);
    }
    // Per-turn model observation. The CLI's `system/init` carries the
    // resolved model id once per session; every subsequent Assistant
    // envelope re-states the model at `message.model`. Tracking the
    // most recent observed model lets the App verify that the chip
    // matches what the CLI is actually using on each turn.
    if !message.model.is_empty() {
        app.set_observed_assistant_model(Some(message.model.clone()));
    }
    record_live_turn_usage(app, &message, parent_tool_use_id.as_deref());
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

    // Unrecognised block types decode to `ContentBlock::Unknown` rather
    // than being dropped, so an empty slice is genuinely no content.
    if !content.is_empty() {
        super::clear_compaction_state(app, true);
    }

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
                let chunk = model::ContentChunk::new(model::RenderContentBlock::Text(
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
    // A genuine new user turn invalidates the previous turn's
    // thinking-token tally. Without this clear, a turn that ends
    // without a `Result` - in flight when the next user turn lands -
    // leaves the accumulator holding its total, and the new turn's
    // deltas add on top of it, so the row bills one turn for two turns
    // of reasoning with nothing on screen saying so. Tool-result echoes
    // (`tool_use_result.is_some()`) are mid-turn continuations of the
    // assistant's tool-call loop, not new user turns - the count must
    // survive them, since a turn's later thinking blocks arrive after
    // exactly these. The Result-side clear in `handle_result` covers
    // the clean turn-end case; this one is additive for the in-flight
    // case.
    if tool_use_result.is_none() {
        app.set_latest_thinking_tokens(None);
    }
    walk_user_tool_results(app, &message.content, tool_use_result.as_ref());
    // The CLI never echoes stdin-injected prompts live on stream-json
    // (live peer/worker turns are painted workspace-side via
    // `PeerEnvelopeAppended`); this is the resume path, where
    // `load_resume_history` replays the persisted envelope through here
    // as a `Message::User` the peer-wrapper prefix detects, so the
    // reconstructed bubble matches what live delivery rendered.
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

/// Which envelope constructor built a message. Merging is gated on this:
/// `role_label_line` picks the `Gotify` / `Cron` source label from these
/// per-message flags, so appending a notification to a peer message
/// would render an external alert as agent traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnvelopeKind {
    Peer,
    Gotify,
    Cron,
}

impl EnvelopeKind {
    fn of_inbound(kind: &crate::ui::peer_block::PeerInboundKind) -> Self {
        match kind {
            crate::ui::peer_block::PeerInboundKind::Gotify { .. } => Self::Gotify,
            crate::ui::peer_block::PeerInboundKind::Cron { .. } => Self::Cron,
            // Spelled out, not `_`: a new inbound kind must not silently
            // inherit peer traffic's unlabelled treatment and merge into it.
            crate::ui::peer_block::PeerInboundKind::Message { .. }
            | crate::ui::peer_block::PeerInboundKind::Question { .. }
            | crate::ui::peer_block::PeerInboundKind::Reply { .. }
            | crate::ui::peer_block::PeerInboundKind::DeliveryFailure { .. }
            | crate::ui::peer_block::PeerInboundKind::WorkerSpawnFailed { .. } => Self::Peer,
        }
    }

    fn of_message(msg: &crate::app::ChatMessage) -> Option<Self> {
        if msg.is_gotify_envelope {
            Some(Self::Gotify)
        } else if msg.is_cron_envelope {
            Some(Self::Cron)
        } else if msg.is_peer_envelope {
            Some(Self::Peer)
        } else {
            None
        }
    }
}

/// Append this envelope to the tail when that message is already an
/// envelope of the same kind, so a run of incoming messages becomes ONE
/// message with N blocks and can reach the group threshold.
///
/// The merge window bounds itself: once the agent produces any output the
/// tail is no longer an envelope message, so the next envelope starts
/// fresh. That is the behaviour, not a rule to maintain.
fn append_or_push_envelope(app: &mut App, kind: EnvelopeKind, text: &str) {
    use crate::app::{ChatMessage, MessageBlock, MessageRole, TextBlock};

    let block = MessageBlock::Text(TextBlock::from_complete(text));
    // Peer only. Gotify and Cron can never reach the group threshold -
    // `is_messaging_block` needs a `peer_sender_identity` and both return
    // None - so merging buys them nothing, while costing them one role
    // label for N alerts and one retention unit for N drops.
    if kind == EnvelopeKind::Peer
        && let Some(tail) = app.messages().len().checked_sub(1)
        && EnvelopeKind::of_message(&app.messages()[tail]) == Some(kind)
    {
        app.active_messages_mut()[tail].blocks.push(block);
        // Appending bypasses `push_message_tracked`, so the retained-byte
        // accounting and the layout invalidation have to be driven here.
        app.sync_after_message_tail_changed(tail);
        return;
    }
    let blocks = vec![block];
    // #143 item 2: cache the envelope flag at push time so the chat
    // renderer doesn't walk text blocks every frame.
    let msg = match kind {
        EnvelopeKind::Gotify => ChatMessage::new_gotify_envelope(MessageRole::User, blocks),
        EnvelopeKind::Cron => ChatMessage::new_cron_envelope(MessageRole::User, blocks),
        EnvelopeKind::Peer => ChatMessage::new_peer_envelope(MessageRole::User, blocks),
    };
    app.push_message_tracked(msg);
}

/// Resume-side painter: pushes `text` through the stamped envelope
/// constructors when it is an inbound envelope. Returns false for
/// plain text so the caller falls through to its own rendering.
pub(super) fn append_resume_envelope_if_present(app: &mut App, text: &str) -> bool {
    let Some(kind) = crate::ui::peer_block::detect_inbound(text) else {
        return false;
    };
    append_or_push_envelope(app, EnvelopeKind::of_inbound(&kind), text);
    app.enforce_history_retention_tracked();
    tracing::debug!(
        target: crate::logging::targets::APP_INPUT,
        event_name = "resume_envelope_replayed",
        message = "pushed stamped envelope card for replayed inbound text",
        outcome = "success",
    );
    true
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
    use forge_primitives::ContentBlock;

    for block in content {
        let ContentBlock::Text { text } = block else {
            continue;
        };
        let Some(kind) = crate::ui::peer_block::detect_inbound(text) else {
            continue;
        };
        let envelope_kind = EnvelopeKind::of_inbound(&kind);
        // Replay reconstructs the chat bubble only - no live turn
        // ceremony. load_resume_history walks historical envelopes through
        // this dispatcher and has no balancing Result to clear a Running
        // flip or a freshly-opened placeholder (the stuck-spinner failure
        // mode), matching handle_assistant's replay gate.
        if app.replay_in_progress {
            append_or_push_envelope(app, envelope_kind, text);
            app.enforce_history_retention_tracked();
            return;
        }
        // Shares dispatch_prompt's turn-open (strip a stranded placeholder,
        // append the user turn, open a fresh tail placeholder + reparent
        // the spinner) but deliberately skips its auto-scroll, so a
        // delivered turn does not yank a scrolled-up reader. The strip runs
        // FIRST, which is what leaves the previous envelope message at the
        // tail for `append_or_push_envelope` to merge into.
        app.strip_trailing_empty_assistant_placeholder();
        append_or_push_envelope(app, envelope_kind, text);
        app.push_active_turn_assistant_placeholder();
        // The clock starts here rather than at the first frame, so the
        // turn-info row never sits as a bare loader while it waits for
        // usage; a prompt delivered mid-turn rides the live bar instead
        // of starting a second one. This runs inside with_pivoted for a
        // background bucket, where app.status is the focused session's
        // snapshot, so the in-flight test reads the pivoted bucket's own
        // live turn.
        if app.active_session().is_some_and(|bucket| bucket.live_turn.started_at.is_some())
            && !app.pending_cancel()
            && !app.is_compacting()
        {
            app.continue_live_turn(std::time::Instant::now());
        } else {
            app.start_live_turn(std::time::Instant::now());
        }
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
    if append_resume_envelope_if_present(app, prompt_text) {
        return;
    }
    let blocks = vec![MessageBlock::Text(TextBlock::from_complete(prompt_text))];
    app.push_message_tracked(ChatMessage::new(MessageRole::User, blocks));
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
/// `RenderToolCall` or `RenderToolCallUpdate` via the existing App handlers.
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
        // A re-statement carries no news about the call's state, and a
        // transcript can re-state one whose result already landed.
        status: None,
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
/// `RenderToolCallUpdate` through the existing App handler.
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
/// fields, then dispatch a `RenderToolCallUpdate` via the existing App
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
/// A live backgrounded task is skipped for the same reason, root and
/// children alike: a `run_in_background` Bash or Task/Agent root outlives
/// its spawning turn, so its card must not flip terminal until
/// `task_updated`.
fn finalize_open_tool_calls(app: &mut App, status: forge_primitives::ToolCallStatus) {
    use crate::app::state::tool_call_info::is_monitor_tool_name;
    use forge_primitives::{ToolCallStatus, ToolCallUpdateFields};

    let open_ids: Vec<String> = app.with_turn_state(|ts| {
        ts.tool_calls
            .iter()
            .filter(|(_, t)| {
                matches!(t.status, ToolCallStatus::Pending | ToolCallStatus::InProgress)
            })
            .filter(|(_, t)| {
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
                true
            })
            .map(|(id, _)| id.clone())
            .collect()
    });
    // Liveness is answered per open call - O(depth) each - rather than
    // deriving the eager exempt set off the whole scope map (#793).
    let (pending, spared): (Vec<String>, Vec<String>) = open_ids.into_iter().partition(|id| {
        !app.active_session().is_some_and(|session| session.is_backgrounded_alive_or_descendant(id))
    });
    tracing::debug!(
        target: crate::logging::targets::APP_TOOL,
        event_name = "tool_call_sweep",
        message = "swept open tool calls at a turn boundary",
        outcome = "success",
        sweep_site = "result_finalize",
        new_status = ?status,
        exempt_count = spared.len(),
    );
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
                    // The CLI confirming a mode retires the optimistic
                    // `/mode` rollback snapshot - there is no switch
                    // awaiting a verdict anymore.
                    app.set_pending_mode_rollback(None);
                    let supports_auto_mode =
                        app.current_model().is_some_and(|m| m.supports_auto_mode == Some(true));
                    let unavailable_modes =
                        app.with_turn_state(|ts| ts.runtime_unavailable_mode_ids.clone());
                    let bypass_offered = super::bypass_mode_offered(app);
                    let supported = supported_mode_ids_filtered(
                        supports_auto_mode,
                        bypass_offered,
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
        // Only reachable once the payload has drifted out of the typed
        // variant, so the metadata is gone but the compaction still
        // happened. `EXPECTED_GENERIC_SYSTEM_SUBTYPES` omits the subtype,
        // which is what makes the next live capture report the drift.
        "compact_boundary" => {
            count_compaction(app);
            tracing::warn!(
                target: crate::logging::targets::APP_SESSION,
                ?data,
                "compact_boundary arrived untyped: counted the compaction but trigger and pre_tokens stay unset",
            );
        }
        "local_command_output" => {
            apply_local_command_output(app, &data);
        }
        _ => {}
    }
}

/// The row's presence is the compaction; its metadata is detail. The
/// transcript-seeded count keys on the subtype alone, so both arrival
/// shapes have to reach this - otherwise a metadata quirk makes the
/// number jump the next time a resume re-reads the file.
fn count_compaction(app: &mut App) {
    let usage = app.session_usage_mut();
    usage.compaction_count = usage.compaction_count.saturating_add(1);
}

/// Count a `Message::CompactBoundary` and apply its metadata. An
/// unrecognised trigger still counts; only the boundary update needs it.
fn handle_compact_boundary(app: &mut App, trigger: &str, pre_tokens: u64) {
    count_compaction(app);
    let model_trigger = match trigger {
        "manual" => crate::agent::model::CompactionTrigger::Manual,
        "auto" => crate::agent::model::CompactionTrigger::Auto,
        _ => return,
    };
    super::rate_limit::handle_compaction_boundary_update(
        app,
        crate::agent::model::CompactionBoundary { trigger: model_trigger, pre_tokens },
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
            supports_auto_mode: m.supports_auto_mode,
        })
        .collect();

    let next_wire =
        resolve_current_model_from_inputs(model_id, requested, resolved_runtime, &available_models);
    // The CLI reporting a model retires the optimistic `/model`
    // rollback snapshot - the verdict has landed, whatever it is. Sits
    // ahead of the equality early-return: the CLI confirming the model
    // the optimistic apply already set still closes the pending switch.
    app.set_pending_model_rollback(None);
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

    let supports_auto_mode =
        app.current_model().is_some_and(|m| m.supports_auto_mode == Some(true));
    let unavailable_modes = app.with_turn_state(|ts| ts.runtime_unavailable_mode_ids.clone());
    let bypass_offered = super::bypass_mode_offered(app);
    let supported = supported_mode_ids_filtered(
        supports_auto_mode,
        bypass_offered,
        Some(mode),
        &unavailable_modes,
    );
    let _: () = app.with_turn_state_mut(|ts| ts.supported_mode_ids.clone_from(&supported));

    let wire_mode_state = build_mode_state_from_supported(mode, &supported);
    super::apply_mode_state_update(app, wire_mode_state);
}

#[cfg(test)]
mod bypass_mode_list_tests {
    use super::apply_mode_state_from_init;
    use crate::app::App;
    use serde_json::json;

    fn offers_bypass(app: &App) -> bool {
        app.mode()
            .is_some_and(|state| state.available_modes.iter().any(|m| m.id == "bypassPermissions"))
    }

    #[test]
    fn bypass_launch_seeds_the_offer_and_it_survives_switching_away() {
        let mut app = App::test_default();

        // First init of a session launched into bypass.
        apply_mode_state_from_init(&mut app, &json!({"permissionMode": "bypassPermissions"}));
        assert!(offers_bypass(&app), "bypass launch seeds the picker offer");

        // The session cycles away to plan; the next turn's init
        // reports the new current mode, and bypass must stay offered
        // so cycling back works.
        apply_mode_state_from_init(&mut app, &json!({"permissionMode": "plan"}));
        assert!(offers_bypass(&app), "bypass stays offered after switching away");
    }

    #[test]
    fn normally_launched_sessions_never_get_the_offer() {
        let mut app = App::test_default();

        apply_mode_state_from_init(&mut app, &json!({"permissionMode": "acceptEdits"}));
        apply_mode_state_from_init(&mut app, &json!({"permissionMode": "plan"}));
        assert!(!offers_bypass(&app), "no bypass offer without a bypass launch");
    }
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
    let chunk = model::ContentChunk::new(model::RenderContentBlock::Text(model::TextContent::new(
        content.to_owned(),
    )));
    super::streaming::handle_agent_message_chunk(app, chunk);
}

/// Drain `settings_errors` / `settingsErrors` from a System(init)
/// data record and call the App's settings-parse-error notice handler
/// once per error.
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
    let Message::TaskStarted { tool_use_id, task_id, task_type, .. } = msg else { return };
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
        // Agent-kind dispatches are backgrounded work: sticky liveness
        // that survives a turn boundary the roster has not caught up
        // with yet (#790).
        if matches!(task_type.as_deref(), Some("agent" | "local_agent")) {
            app.mark_backgrounded_root(id.to_owned());
        } else if matches!(
            app.tool_call_scope(id),
            Some(crate::app::state::types::ToolCallScope::SubagentRoot)
        ) && task_type.as_deref().is_some_and(|kind| !kind.is_empty())
        {
            tracing::warn!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "task_started_unmarked_agent_kind",
                message = "a subagent dispatch's task_started names no known agent task_type; no sticky liveness mark is set (possible wire drift)",
                outcome = "partial",
                task_type = %task_type.as_deref().unwrap_or_default(),
            );
        }
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
        // A terminal patch is one of the two events that may clear the
        // sticky backgrounded marker. The turn-scoped lookup below
        // resets every turn, so resolve through the session map.
        let root_id = app
            .active_session()
            .and_then(|session| session.session_task_tool_use_ids.get(&task_id).cloned());
        if let Some(root_id) = root_id {
            app.clear_backgrounded_root(&root_id);
        } else {
            tracing::debug!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "task_updated_terminal_no_sticky_match",
                message = "terminal task_updated resolved no session-map entry; no sticky marker to clear",
                outcome = "skipped",
                task_id = %task_id,
            );
        }
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
        // The other event that may clear the sticky backgrounded marker:
        // resolve before dropping the mapping, falling back to the
        // notification's own tool_use_id.
        let root_id = app
            .active_session()
            .and_then(|session| session.session_task_tool_use_ids.get(&task_id).cloned())
            .or_else(|| (!id.is_empty()).then(|| id.to_owned()));
        app.remove_session_task_mapping(&task_id);
        if let Some(root_id) = root_id {
            app.clear_backgrounded_root(&root_id);
            // The true completion settles the root's open children here,
            // not only at the roster drain (#789): the drain frame finds
            // no mapping after this and could not resolve them.
            app.settle_departed_root_children(&root_id);
        }
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
/// Rebuild a `turn_state.tool_calls` entry from the block already in
/// the message list, preserving its real tool name so nothing
/// downstream mistakes it for a `Task`.
fn readopted_turn_state_entry(app: &App, tool_use_id: &str) -> Option<forge_primitives::ToolCall> {
    let (msg_idx, block_idx) = app.lookup_tool_call(tool_use_id)?;
    let crate::app::MessageBlock::ToolCall(tc) =
        app.messages().get(msg_idx)?.blocks.get(block_idx)?
    else {
        return None;
    };
    let mut rebuilt = forge_workspace::tooling::create_tool_call(
        tool_use_id,
        &tc.sdk_tool_name,
        tc.raw_input.as_ref().unwrap_or(&Value::Null),
        None,
    );
    // Status only. It has to carry over or the terminal guard in
    // `apply_tool_progress_update` misses and an already-finished tool
    // call gets reopened. Nothing reads the entry's title, and
    // `apply_tool_summary_update` takes content from the notification,
    // so the block in the message list stays the source of truth for
    // the rest.
    rebuilt.status = tc.status;
    Some(rebuilt)
}

fn apply_tool_progress_update(app: &mut App, tool_use_id: &str, name: &str) {
    use forge_primitives::{ToolCallStatus, ToolCallUpdateFields};

    // `turn_state` resets at every turn finalisation, so a frame
    // arriving after its launching turn ended finds nothing here.
    let existing =
        if let Some(existing) = app.with_turn_state(|ts| ts.tool_calls.get(tool_use_id).cloned()) {
            existing
        } else {
            // Re-adopt the block that already exists rather than
            // synthesizing over it. Synthesizing pushes a `"Task"` tool_use
            // on top, renaming it and hiding it; simply returning instead
            // leaves `turn_state` empty, and `apply_tool_summary_update`
            // early-returns without an entry, so the task_notification
            // never lands and the card sticks on in_progress with no
            // content.
            let Some(readopted) = readopted_turn_state_entry(app, tool_use_id) else {
                apply_tool_use_block(
                    app,
                    tool_use_id,
                    name,
                    &Value::Object(serde_json::Map::new()),
                    None,
                );
                return;
            };
            let entry = readopted.clone();
            let _: () = app.with_turn_state_mut(|ts| {
                ts.tool_calls.insert(tool_use_id.to_owned(), entry);
            });
            readopted
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
    // Drift breadcrumb: if the CLI renamed a field every entry fails
    // the parse and the section silently never appears. An empty
    // snapshot is a legitimate state (section auto-hides), so this
    // only logs - the replace still applies.
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
    // Drift breadcrumb: every kind must route to a section
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
        // Resolve the root before dropping the mapping, so its open
        // children settle now rather than waiting for the next turn
        // boundary (#789). The root's own card stays open for its
        // terminal `task_updated`, which lands a frame after the drain.
        let root_id = app
            .active_session()
            .and_then(|session| session.session_task_tool_use_ids.get(task_id).cloned());
        app.remove_session_task_mapping(task_id);
        if let Some(root_id) = root_id {
            app.clear_backgrounded_root(&root_id);
            app.settle_departed_root_children(&root_id);
        }
    }
    // Agent-kind roster rows extend the sticky backgrounded-marker set
    // (#790): a task whose `task_started` named no agent-kind type is
    // still backgrounded work once the roster lists it.
    let seeded_roots: Vec<String> = parsed
        .iter()
        .filter(|task| matches!(task.task_type.as_str(), "agent" | "local_agent"))
        .filter_map(|task| {
            app.active_session()
                .and_then(|session| session.session_task_tool_use_ids.get(&task.task_id).cloned())
        })
        .collect();
    *app.background_tasks_mut() = parsed;
    for root_id in seeded_roots {
        app.mark_backgrounded_root(root_id);
    }
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
    // Drift guard: a non-empty payload that parses to
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
        duration_api_ms,
        usage,
        total_cost_usd,
        is_error,
        subtype,
        errors,
        terminal_reason,
        ..
    } = msg
    else {
        return;
    };
    let api_ms = app.settle_live_turn(duration_api_ms);
    stamp_turn_info_on_latest_assistant(app, duration_ms, api_ms, usage, total_cost_usd);
    // #273: Turn ended - reset the accumulator so the next turn counts
    // its own reasoning from zero. The stamp above has already copied
    // the total onto this turn's row, which is what goes on showing it.
    // `stop_hook_summary` is left intact: it belongs to the
    // just-completed turn's end-of-turn surface.
    app.set_latest_thinking_tokens(None);
    apply_result_finalize(app, is_error, &subtype, errors.unwrap_or_default(), terminal_reason);
}

/// Settle the turn-info row on the latest Assistant ChatMessage, or
/// decline if the Result would leave that row describing two
/// different turns.
///
/// Invalidates the layout rather than just the render cache: the row
/// can change height, and turn exit reads the App-global status, so a
/// background session's turn ending takes neither path.
fn stamp_turn_info_on_latest_assistant(
    app: &mut App,
    duration_ms: u64,
    api_ms: Option<u64>,
    usage: Option<forge_primitives::Usage>,
    total_cost_usd: Option<f64>,
) {
    let Some(idx) =
        app.messages().iter().rposition(|m| matches!(m.role, crate::app::MessageRole::Assistant))
    else {
        return;
    };
    let model = app.observed_assistant_model().map(ToOwned::to_owned);
    let usage = usage.filter(|u| !is_unattributed_usage(*u));
    let thinking_tokens = app.latest_thinking_tokens();
    if let Some(msg) = app.active_messages_mut().get_mut(idx) {
        let info = &mut msg.turn_info;
        // A Result with no usable token counts cannot replace the ones
        // already on a settled row, so writing its clock there would
        // leave another turn's figures sitting underneath it.
        if usage.is_none() && info.is_settled() {
            return;
        }
        info.duration_ms = Some(duration_ms);
        info.api_ms = api_ms;
        info.ended_at_local = Some(local_clock_now());
        info.session_cost_usd = total_cost_usd;
        info.thinking_tokens = thinking_tokens;
        if model.is_some() {
            info.model = model;
        }
        if let Some(usage) = usage {
            info.input_tokens = Some(usage.input_tokens);
            info.output_tokens = Some(usage.output_tokens);
            info.cache_read_tokens = Some(usage.cache_read_input_tokens);
            info.cache_written_tokens = Some(usage.cache_creation_input_tokens);
        }
        msg.invalidate_render_cache();
    }
    app.invalidate_layout(crate::app::InvalidationLevel::MessageChanged(idx));
}

/// True when a present `usage` block carries no information.
///
/// A turn that reached the API always spends input tokens, so an
/// all-zero block means the CLI attributed none - which is why only
/// the whole block counts, an individual zero being a real
/// measurement. Destructured so a counter added to `Usage` fails to
/// build here.
fn is_unattributed_usage(usage: forge_primitives::Usage) -> bool {
    let forge_primitives::Usage {
        input_tokens,
        output_tokens,
        cache_read_input_tokens,
        cache_creation_input_tokens,
    } = usage;
    input_tokens == 0
        && output_tokens == 0
        && cache_read_input_tokens == 0
        && cache_creation_input_tokens == 0
}

/// Local wall-clock `HH:MM:SS` for the turn's end stamp.
fn local_clock_now() -> String {
    use time_tz::OffsetDateTimeExt;
    let tz = forge_workspace::env::timezone::system_timezone();
    let now = time::OffsetDateTime::now_utc().to_timezone(tz);
    format!("{:02}:{:02}:{:02}", now.hour(), now.minute(), now.second())
}

/// Fold one assistant frame's input-side usage into the live turn and
/// re-stamp the running totals onto its message, so the row counts up.
///
/// Skipped for a subagent frame, whose usage is not part of the
/// parent turn's `Result.usage`, and during resume replay. A settled
/// message bails after the fold, so the frame still counts toward the
/// session totals and only the re-stamp onto it is skipped - which is
/// why a turn appended to one shows nothing of its own until its
/// Result lands.
fn record_live_turn_usage(
    app: &mut App,
    message: &forge_primitives::AssistantEnvelope,
    parent_tool_use_id: Option<&str>,
) {
    if app.replay_in_progress || parent_tool_use_id.is_some() {
        return;
    }
    let Some(usage) = message.usage else {
        return;
    };
    let (started_at, totals) = app.record_live_turn_usage(
        message.id.clone(),
        crate::app::state::messages::LiveUsage {
            input_tokens: usage.input_tokens,
            cache_read_tokens: usage.cache_read_input_tokens,
            cache_written_tokens: usage.cache_creation_input_tokens,
        },
    );
    let Some(idx) =
        app.messages().iter().rposition(|m| matches!(m.role, crate::app::MessageRole::Assistant))
    else {
        return;
    };
    let model = (!message.model.is_empty()).then(|| message.model.clone());
    // The turn's first thinking events land before it has a message to
    // hold them, so the first frame that gives it one back-fills them.
    let thinking_tokens = app.latest_thinking_tokens();
    if let Some(msg) = app.active_messages_mut().get_mut(idx) {
        let info = &mut msg.turn_info;
        if info.is_settled() {
            return;
        }
        info.started_at = started_at;
        info.thinking_tokens = thinking_tokens;
        if model.is_some() {
            info.model = model;
        }
        if let Some(totals) = totals {
            info.input_tokens = Some(totals.input_tokens);
            info.cache_read_tokens = Some(totals.cache_read_tokens);
            info.cache_written_tokens = Some(totals.cache_written_tokens);
        }
        msg.invalidate_render_cache();
    }
    app.invalidate_layout(crate::app::InvalidationLevel::MessageChanged(idx));
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
mod stamp_turn_info_tests {
    //! Unit coverage for `stamp_turn_info_on_latest_assistant`. The
    //! full handle_result chain pushes a placeholder Assistant, so the
    //! stamp is tested in isolation here; the wire-driven path is
    //! pinned in `replay.rs`.
    use super::stamp_turn_info_on_latest_assistant;
    use super::stamp_turn_info_on_latest_assistant as stamp;
    use super::{handle_thinking_tokens, handle_user, record_live_turn_usage};
    use crate::app::{App, ChatMessage, MessageRole, TurnInfo};

    fn usage(input: u64, output: u64, read: u64, written: u64) -> forge_primitives::Usage {
        forge_primitives::Usage {
            input_tokens: input,
            output_tokens: output,
            cache_read_input_tokens: read,
            cache_creation_input_tokens: written,
        }
    }

    /// An app holding one assistant message, ready to be stamped.
    fn app_with_assistant() -> App {
        let mut app = App::test_default();
        app.push_message_tracked(ChatMessage::new(MessageRole::Assistant, Vec::new()));
        app
    }

    /// One assistant frame carrying usage, the shape that stamps a
    /// live row.
    fn assistant_frame(id: &str) -> forge_primitives::AssistantEnvelope {
        forge_primitives::AssistantEnvelope {
            id: id.to_owned(),
            role: "assistant".to_owned(),
            model: "claude-opus-5".to_owned(),
            content: Vec::new(),
            stop_reason: None,
            stop_sequence: None,
            usage: Some(usage(4, 1, 93_262, 62_840)),
        }
    }

    /// A genuine user prompt: no `tool_use_result`, so the reducer
    /// treats it as a new turn rather than a mid-turn echo.
    fn user_prompt() -> forge_primitives::Message {
        forge_primitives::Message::User {
            message: forge_primitives::UserEnvelope {
                role: "user".to_owned(),
                content: vec![forge_primitives::ContentBlock::Text {
                    text: "next prompt".to_owned(),
                }],
            },
            session_id: String::new(),
            parent_tool_use_id: None,
            uuid: None,
            tool_use_result: None,
        }
    }

    /// A turn that never gets a `Result` leaves its estimate on a row
    /// the next turn then reuses, because a plain user prompt opens no
    /// placeholder of its own - the case `handle_user`'s clear exists
    /// for. Both mirrors have to overwrite rather than skip, or turn
    /// two wears turn one's reasoning.
    ///
    /// Driven through the reducer rather than by seeding a `TurnInfo`:
    /// the point is that this sequence is reachable, which a fixture
    /// cannot show. No baseline happens to contain it.
    #[test]
    fn a_turn_reusing_an_unsettled_row_does_not_inherit_its_estimate() {
        let mut app = app_with_assistant();
        handle_thinking_tokens(&mut app, 50);
        handle_thinking_tokens(&mut app, 33);
        record_live_turn_usage(&mut app, &assistant_frame("msg_turn_one"), None);
        assert_eq!(
            latest_turn_info(&app).thinking_tokens,
            Some(83),
            "fixture guard: turn one's estimate is on the row, or the assertions below pass \
             for want of anything to inherit",
        );

        // Turn one never settles. The prompt resets the accumulator but
        // leaves the row it was mirrored onto.
        handle_user(&mut app, user_prompt());
        record_live_turn_usage(&mut app, &assistant_frame("msg_turn_two"), None);
        assert_eq!(
            latest_turn_info(&app).thinking_tokens,
            None,
            "the live mirror must write the absence onto the reused row, not skip and leave \
             turn one's 83 sitting under turn two's figures",
        );

        // And the settle: a Result reaching a still-live row, which is
        // the compaction shape.
        handle_thinking_tokens(&mut app, 40);
        handle_user(&mut app, user_prompt());
        stamp(&mut app, 9_717, Some(9_668), Some(usage(4, 186, 167_802, 825)), None);
        assert_eq!(
            latest_turn_info(&app).thinking_tokens,
            None,
            "and the settle overwrites too, or the row freezes an estimate belonging to a \
             different turn",
        );
    }

    /// The other half, and the commoner one: interrupt a turn and type
    /// the next prompt. That goes through the submit path, where
    /// `start_live_turn` replaces the row's whole `TurnInfo` - so the
    /// accumulator it does not reach would be added to rather than
    /// replaced, the deltas summing across the boundary.
    #[test]
    fn a_prompt_after_an_interrupted_turn_starts_the_estimate_over() {
        let mut app = app_with_assistant();
        handle_thinking_tokens(&mut app, 50);
        handle_thinking_tokens(&mut app, 33);
        record_live_turn_usage(&mut app, &assistant_frame("msg_turn_one"), None);
        assert_eq!(
            latest_turn_info(&app).thinking_tokens,
            Some(83),
            "fixture guard: turn one's estimate is on the row before it is interrupted",
        );

        // Interrupted: no Result. The user types, which is the submit
        // path rather than a wire frame.
        app.push_message_tracked(ChatMessage::new(MessageRole::Assistant, Vec::new()));
        app.start_live_turn(std::time::Instant::now());
        assert_eq!(
            latest_turn_info(&app).thinking_tokens,
            None,
            "the fresh turn's row carries no estimate yet",
        );
        assert_eq!(
            app.latest_thinking_tokens(),
            None,
            "and the accumulator behind it is reset too - clearing only the row would leave \
             the next delta adding to a number the user cannot see",
        );

        handle_thinking_tokens(&mut app, 50);
        assert_eq!(
            latest_turn_info(&app).thinking_tokens,
            Some(50),
            "turn two has thought 50, so that is what it reports - not 133",
        );
    }

    fn latest_turn_info(app: &App) -> TurnInfo {
        app.messages()
            .iter()
            .rev()
            .find(|m| matches!(m.role, MessageRole::Assistant))
            .expect("stamping must not drop the assistant message it targets")
            .turn_info
            .clone()
    }

    /// `Result.duration_api_ms` counts up across the session while
    /// `duration_ms` is per turn, so reading it directly makes the
    /// local split negative from the second turn onward. Figures are
    /// `compact.jsonl`'s first three results.
    #[test]
    fn api_time_is_the_delta_of_a_session_cumulative_counter() {
        let mut app = App::test_default();
        assert_eq!(
            app.settle_live_turn(3_403),
            Some(3_403),
            "the first turn has nothing to subtract"
        );
        assert_eq!(
            app.settle_live_turn(6_504),
            Some(3_101),
            "later turns are the delta, not the total"
        );
        assert_eq!(app.settle_live_turn(9_020), Some(2_516));
        assert_eq!(
            app.settle_live_turn(9_020),
            None,
            "a turn that advanced the counter by nothing spent no attributable API time; \
             treating the unchanged counter as a fresh start would report the session's \
             entire 9020ms as this turn's",
        );

        let mut info =
            TurnInfo { duration_ms: Some(2_569), api_ms: Some(2_516), ..TurnInfo::default() };
        assert_eq!(
            info.local_ms(),
            Some(53),
            "wall clock minus the delta is a plausible local overhead; minus the raw counter \
             it would be -6451",
        );
        // workflow.jsonl's first result: concurrent subagent calls
        // outran wall clock, so there is no local time to claim.
        info = TurnInfo { duration_ms: Some(11_973), api_ms: Some(18_486), ..TurnInfo::default() };
        assert_eq!(info.local_ms(), None, "an unsound split reads as absent, not as zero");
    }

    /// A compaction Result arrives with `duration_api_ms: 0` and an
    /// all-zero `usage`: the CLI attributing nothing, not measuring
    /// zero. The rule keys on the WHOLE block, so a real block
    /// carrying one zero still reports it.
    #[test]
    fn an_unattributed_result_stamps_nothing_but_a_real_zero_survives() {
        let mut app = App::test_default();
        let _ = app.settle_live_turn(16_244);
        assert_eq!(
            app.settle_live_turn(0),
            None,
            "the counter restarted to zero, which attributes no API time to this turn",
        );

        let mut app = app_with_assistant();
        stamp(&mut app, 44_410, None, Some(usage(0, 0, 0, 0)), Some(1.164_956_5));
        let info = latest_turn_info(&app);
        assert_eq!(info.duration_ms, Some(44_410), "the wall clock is real and is kept");
        assert_eq!(info.api_ms, None, "an unattributed API time is absent, not 0");
        assert_eq!(info.local_ms(), None, "with no API time there is no local time to derive");
        assert_eq!(
            (
                info.input_tokens,
                info.output_tokens,
                info.cache_read_tokens,
                info.cache_written_tokens
            ),
            (None, None, None, None),
            "an all-zero usage block carries no information, so no count is claimed",
        );

        let mut app = app_with_assistant();
        stamp(&mut app, 4_675, Some(3_807), Some(usage(2, 5, 15_262, 0)), None);
        assert_eq!(
            latest_turn_info(&app).cache_written_tokens,
            Some(0),
            "writing nothing to the cache is a real measurement and must not be suppressed",
        );
    }

    /// The refusal is narrow on purpose: an unattributed Result has no
    /// counts to replace the row's, so its clock would sit over
    /// another turn's figures. One carrying usage replaces every field
    /// together and is let through even onto a settled row, which is
    /// what a turn appended to one needs.
    #[test]
    fn only_a_result_with_no_token_counts_is_refused_by_a_settled_row() {
        let mut app = app_with_assistant();
        stamp(&mut app, 4_675, Some(3_807), Some(usage(2, 5, 15_262, 62_706)), Some(0.634_826));
        let settled = latest_turn_info(&app);
        assert_eq!(settled.duration_ms, Some(4_675), "a live turn's Result still stamps its row");
        assert_eq!(settled.api_ms, Some(3_807), "including its API time");

        stamp(&mut app, 44_410, None, None, None);
        let refused = latest_turn_info(&app);
        assert_eq!(
            (refused.duration_ms, refused.output_tokens),
            (Some(4_675), Some(5)),
            "an unattributed Result cannot supply tokens, so its clock must not land over the \
             counts already on the row",
        );

        stamp(&mut app, 9_717, Some(9_668), Some(usage(4, 186, 167_802, 825)), Some(0.961_126));
        let replaced = latest_turn_info(&app);
        assert_eq!(
            (replaced.duration_ms, replaced.output_tokens, replaced.cache_written_tokens),
            (Some(9_717), Some(186), Some(825)),
            "a Result carrying usage overwrites every field together, so it stays coherent \
             and is not refused",
        );
    }

    /// The row's defining behaviour is that it appears when the turn
    /// starts, and `start_live_turn`'s stamp is the only thing that
    /// makes it: deleting it leaves the whole suite green while the
    /// row silently stops rendering until the Result lands.
    #[test]
    fn starting_a_turn_stamps_the_row_but_leaves_a_settled_one_alone() {
        let mut app = app_with_assistant();
        let at = std::time::Instant::now();
        app.start_live_turn(at);
        let started = latest_turn_info(&app);
        assert!(started.started_at.is_some(), "the turn's start is stamped onto its row");
        assert!(!started.is_empty(), "so the row renders before any Result arrives");

        stamp(&mut app, 4_675, Some(3_807), Some(usage(2, 5, 15_262, 62_706)), None);
        app.start_live_turn(std::time::Instant::now());
        assert_eq!(
            latest_turn_info(&app).duration_ms,
            Some(4_675),
            "a settled row belongs to a finished turn and is not reset by the next start",
        );
    }

    /// The CLI emits one assistant frame per content block, all sharing
    /// a `message.id` and repeating the same usage, so summing frame by
    /// frame double-counts. Figures are `permission_deny`'s three
    /// distinct messages, whose totals match its Result exactly.
    #[test]
    fn repeated_frames_for_one_message_do_not_double_count() {
        use crate::app::state::messages::LiveUsage;
        let mut app = App::test_default();
        let frame = |input, read, written| LiveUsage {
            input_tokens: input,
            cache_read_tokens: read,
            cache_written_tokens: written,
        };
        for (id, f) in [
            ("msg_a", frame(2, 16_726, 62_767)),
            ("msg_a", frame(2, 16_726, 62_767)),
            ("msg_b", frame(2, 79_493, 123)),
            ("msg_b", frame(2, 79_493, 123)),
        ] {
            app.record_live_turn_usage(id.to_owned(), f);
        }
        let (_, totals) = app.record_live_turn_usage("msg_c".to_owned(), frame(2, 79_616, 1_605));

        let totals = totals.expect("three frames recorded");
        assert_eq!(totals.input_tokens, 6, "input is per distinct message, not per frame");
        assert_eq!(totals.cache_read_tokens, 175_835, "cache read matches Result.usage exactly");
        assert_eq!(totals.cache_written_tokens, 64_495, "cache write matches Result.usage exactly");
    }

    #[test]
    fn stamps_duration_on_latest_assistant_message() {
        let mut app = App::test_default();
        app.push_message_tracked(ChatMessage::new(MessageRole::Assistant, Vec::new()));

        stamp_turn_info_on_latest_assistant(&mut app, 12_768, Some(9_000), None, None);

        let latest = app
            .messages()
            .iter()
            .rev()
            .find(|m| matches!(m.role, MessageRole::Assistant))
            .expect("stamping must not drop the assistant message it targets");
        assert_eq!(
            latest.turn_info.duration_ms,
            Some(12_768),
            "Result.duration_ms lands on the latest assistant message verbatim",
        );
    }

    #[test]
    fn no_op_when_no_assistant_message_present() {
        let mut app = App::test_default();
        // No assistant messages seeded; helper's rev().find() returns
        // None and the stamp call is a no-op. Verifying no panic +
        // no spurious mutation is the contract.
        stamp_turn_info_on_latest_assistant(&mut app, 99, None, None, None);
        assert!(app.messages().is_empty());
    }

    #[test]
    fn stamps_latest_assistant_skipping_intervening_user() {
        let mut app = App::test_default();
        app.push_message_tracked(ChatMessage::new(MessageRole::Assistant, Vec::new()));
        app.push_message_tracked(ChatMessage::new(MessageRole::User, Vec::new()));
        app.push_message_tracked(ChatMessage::new(MessageRole::Assistant, Vec::new()));
        app.push_message_tracked(ChatMessage::new(MessageRole::User, Vec::new()));

        stamp_turn_info_on_latest_assistant(&mut app, 5_000, None, None, None);

        // Latest (idx 2) Assistant gets the stamp; earlier (idx 0) stays None.
        let assistants: Vec<Option<u64>> = app
            .messages()
            .iter()
            .filter(|m| matches!(m.role, MessageRole::Assistant))
            .map(|m| m.turn_info.duration_ms)
            .collect();
        assert_eq!(
            assistants,
            vec![None, Some(5_000)],
            "the stamp targets the LATEST assistant, so an earlier one keeps its own row",
        );
    }

    /// The row can change height, so the stamp has to invalidate the
    /// layout and not just the render cache. Turn exit invalidates too
    /// and would mask a missing call, so this drives the stamp alone
    /// against a viewport whose heights start valid.
    #[test]
    fn stamping_marks_its_message_stale_so_the_new_row_is_measured() {
        use crate::app::{MessageBlock, TextBlock};

        let mut app = App::test_default();
        app.push_message_tracked(ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::Text(TextBlock::from_complete("hello"))],
        ));
        let _ = app.active_viewport_mut().on_frame(40, 8);
        app.active_viewport_mut().set_message_height(0, 1);
        app.active_viewport_mut().mark_heights_valid();
        assert_eq!(
            app.active_viewport_mut().oldest_stale_index(),
            None,
            "fixture guard: heights start valid, so a stale index can only come from the stamp",
        );

        stamp_turn_info_on_latest_assistant(&mut app, 12_400, None, None, None);

        assert_eq!(
            app.active_viewport_mut().oldest_stale_index(),
            Some(0),
            "the stamp must invalidate the layout itself, not lean on turn exit doing it",
        );
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
    //! A new genuine user turn must reset the thinking accumulator, so
    //! the next turn's deltas never land on top of the previous turn's
    //! total. The turn info row copies this field, so a leak here is a
    //! wrong number frozen onto a settled row rather than a transient
    //! one.
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
        // assistant's tool-call loop, and a turn's second and later
        // thinking blocks arrive after exactly these. Clearing on one
        // would restart the count mid-turn and undercount the total.
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
    use super::*;

    fn envelope(text: &str) -> forge_primitives::ContentBlock {
        forge_primitives::ContentBlock::Text { text: text.to_owned() }
    }

    const FIRST: &str = "[Message id=t-1 from agent 'steward' (org 'forge')]\n\nthe window is lost";
    const SECOND: &str =
        "[Message id=t-2 from agent 'planner' (org 'forge')]\n\npicking up the migration";
    const GOTIFY: &str = "[Gotify - app 'ci', priority 5]\nbuild failed\nsee the log";

    /// Consecutive envelopes arrive as separate updates, each forging its
    /// own one-block message, so they must be merged here or a run of
    /// incoming messages can never reach the group threshold.
    #[test]
    fn consecutive_inbound_envelopes_merge_into_one_message() {
        let mut app = App::test_default();
        push_peer_envelope_user_turn_if_present(&mut app, &[envelope(FIRST)]);
        push_peer_envelope_user_turn_if_present(&mut app, &[envelope(SECOND)]);

        let envelopes: Vec<&crate::app::ChatMessage> =
            app.messages().iter().filter(|m| m.is_peer_envelope).collect();
        assert_eq!(
            envelopes.len(),
            1,
            "two envelopes must land in ONE message; got {} messages",
            envelopes.len(),
        );
        assert_eq!(envelopes[0].blocks.len(), 2, "both envelopes are blocks of that message");
    }

    /// The whole point of merging on the replay path too: a resumed
    /// session must render the same shape live rendered. Without it a
    /// run comes back as N cards where live showed one bundle.
    #[test]
    fn replayed_envelopes_merge_the_same_way_live_ones_do() {
        let mut app = App::test_default();
        app.replay_in_progress = true;
        push_peer_envelope_user_turn_if_present(&mut app, &[envelope(FIRST)]);
        push_peer_envelope_user_turn_if_present(&mut app, &[envelope(SECOND)]);

        let envelopes: Vec<&crate::app::ChatMessage> =
            app.messages().iter().filter(|m| m.is_peer_envelope).collect();
        assert_eq!(
            envelopes.len(),
            1,
            "replay must merge exactly as live does or a resume diverges; got {}",
            envelopes.len(),
        );
        assert_eq!(envelopes[0].blocks.len(), 2);
    }

    /// Appending bypasses `push_message_tracked`, so the block-count
    /// mutation has to announce itself. Without it the layout keeps a
    /// one-envelope height while two render - the desync class two
    /// previous merges were spent on.
    #[test]
    fn appending_an_envelope_announces_the_block_change() {
        let mut app = App::test_default();
        push_peer_envelope_user_turn_if_present(&mut app, &[envelope(FIRST)]);
        let tail = app.messages().len() - 1;
        let envelope_idx =
            app.messages().iter().position(|m| m.is_peer_envelope).expect("envelope");
        let bytes_before = app.message_retained_bytes()[envelope_idx];
        app.last_invalidation_level.set(None);

        push_peer_envelope_user_turn_if_present(&mut app, &[envelope(SECOND)]);

        assert_eq!(
            app.last_invalidation_level.get(),
            Some(crate::app::InvalidationLevel::MessageChanged(envelope_idx)),
            "the appended-to message must be invalidated",
        );
        assert!(
            app.message_retained_bytes()[envelope_idx] > bytes_before,
            "retained bytes must grow with the appended block",
        );
        let _ = tail;
    }

    /// The kind gate: `role_label_line` picks the Gotify / Cron source
    /// label from per-MESSAGE flags, so a notification sharing a message
    /// with peer traffic would lose its label to peer traffic's none.
    #[test]
    fn a_gotify_notification_never_merges_into_a_peer_envelope_message() {
        let mut app = App::test_default();
        push_peer_envelope_user_turn_if_present(&mut app, &[envelope(FIRST)]);
        push_peer_envelope_user_turn_if_present(&mut app, &[envelope(GOTIFY)]);

        let peer: Vec<&crate::app::ChatMessage> =
            app.messages().iter().filter(|m| m.is_peer_envelope).collect();
        let gotify: Vec<&crate::app::ChatMessage> =
            app.messages().iter().filter(|m| m.is_gotify_envelope).collect();
        assert_eq!(peer.len(), 1, "the peer envelope keeps its own message");
        assert_eq!(peer[0].blocks.len(), 1, "the notification must NOT be appended to it");
        assert_eq!(gotify.len(), 1, "the notification gets its own message");
    }

    /// Merging is Peer-only. Notifications and crons never reach the
    /// group threshold, so merging would buy them nothing and would cost
    /// them their separate role labels and separate retention units.
    #[test]
    fn gotify_and_cron_never_merge_even_with_their_own_kind() {
        const GOTIFY_2: &str = "[Gotify - app 'ci', priority 5]\nsecond alert\nbody";
        const CRON_1: &str = "[Cron]\n\nmorning summary";
        const CRON_2: &str = "[Cron]\n\nevening summary";

        let mut app = App::test_default();
        push_peer_envelope_user_turn_if_present(&mut app, &[envelope(GOTIFY)]);
        push_peer_envelope_user_turn_if_present(&mut app, &[envelope(GOTIFY_2)]);
        let gotify: Vec<&crate::app::ChatMessage> =
            app.messages().iter().filter(|m| m.is_gotify_envelope).collect();
        assert_eq!(gotify.len(), 2, "each notification keeps its own message");

        let mut app = App::test_default();
        push_peer_envelope_user_turn_if_present(&mut app, &[envelope(CRON_1)]);
        push_peer_envelope_user_turn_if_present(&mut app, &[envelope(CRON_2)]);
        let cron: Vec<&crate::app::ChatMessage> =
            app.messages().iter().filter(|m| m.is_cron_envelope).collect();
        assert_eq!(cron.len(), 2, "each fired cron keeps its own message");
    }

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
        ));
        // The assistant turn holding the outbound ask is still active.
        app.active_messages_mut().push(ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::Text(TextBlock::from_complete("asking planner..."))],
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

    /// A delivered prompt opens its turn's clock immediately. Between
    /// the turn-open and the first usage-carrying assistant frame the
    /// turn-info row has nothing to show but the loader glyph - a
    /// window that used to last the whole turn whenever the turn never
    /// produced such a frame.
    #[test]
    fn delivered_prompt_opens_its_turn_clock_immediately() {
        let mut app = App::test_default();
        idle_active_bucket(&mut app);
        app.status = AppStatus::Ready;

        handle_user(&mut app, delivered_user_turn(PEER_REPLY));

        let placeholder = app
            .messages()
            .iter()
            .rev()
            .find(|m| matches!(m.role, MessageRole::Assistant))
            .expect("the delivered turn opens an assistant placeholder");
        assert!(
            placeholder.turn_info.started_at.is_some(),
            "the delivered turn's clock starts at the turn-open, not at its first frame",
        );
    }

    fn seed_background_bucket(app: &mut App, session_id: &str) -> forge_workspace::SessionKey {
        use crate::app::session::UiSession;
        let key = forge_workspace::SessionKey::from_session_id(session_id.to_owned());
        let mut bucket = UiSession::new(key.clone());
        bucket.session_id = Some(crate::agent::model::SessionId::new(session_id.to_owned()));
        app.sessions.insert(key.clone(), bucket);
        key
    }

    /// A prompt delivered to a session the user is NOT watching runs the
    /// turn-open inside `with_pivoted`, where `app.status` still describes
    /// the focused session. A background turn that is mid-turn must ride
    /// its own live bar: the usage accumulator, the thinking estimate and
    /// the in-flight row survive untouched.
    #[test]
    fn delivered_prompt_to_a_background_mid_turn_session_rides_its_live_bar() {
        use crate::app::state::messages::LiveUsage;
        let mut app = App::test_default();
        app.set_session_id(Some(crate::agent::model::SessionId::new("session-a".to_owned())));
        app.status = AppStatus::Ready;
        let background_key = seed_background_bucket(&mut app, "session-b");
        let t0 = std::time::Instant::now();
        {
            let background = app.sessions.get_mut(&background_key).expect("bucket");
            background.live_turn.started_at = Some(t0);
            background
                .live_turn
                .record("msg-1".to_owned(), LiveUsage { input_tokens: 42, ..LiveUsage::default() });
            background.messages.push(ChatMessage::new(
                MessageRole::User,
                vec![MessageBlock::Text(TextBlock::from_complete("prior prompt"))],
            ));
            let mut streaming = ChatMessage::new(MessageRole::Assistant, Vec::new());
            streaming.turn_info = crate::app::state::messages::TurnInfo {
                started_at: Some(t0),
                ..crate::app::state::messages::TurnInfo::default()
            };
            background.messages.push(streaming);
        }

        crate::app::events::client::apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::CronPromptAppended {
                session_id: "session-b".to_owned(),
                text: "check the queue".to_owned(),
            },
        );

        let background = app.sessions.get(&background_key).expect("bucket");
        assert_eq!(
            background.live_turn.started_at,
            Some(t0),
            "the background turn's usage accumulator survives; a fresh start would have \
             wiped it and made the delivered turn bill two turns into one bar",
        );
        assert_eq!(
            background.live_turn.totals().map(|usage| usage.input_tokens),
            Some(42),
            "the recorded frame survives with the accumulator",
        );
        let live_bars: Vec<_> = background
            .messages
            .iter()
            .filter(|m| matches!(m.role, MessageRole::Assistant))
            .filter(|m| !m.turn_info.is_empty() && !m.turn_info.is_settled())
            .collect();
        assert_eq!(live_bars.len(), 1, "exactly one live bar on the background bucket");
        assert_eq!(
            live_bars[0].turn_info.started_at,
            Some(t0),
            "the carried bar keeps the background turn's clock",
        );
    }

    /// The mirror shape: the focused session is busy while the background
    /// bucket is idle. What the test pins: a delivered turn-open on an
    /// idle bucket starts the bucket clock at the turn-open and the row
    /// carries it. (The predicate itself is discriminated by the
    /// sibling mid-turn test; since the continue fallback also starts
    /// the clock, this test passes either way.)
    #[test]
    fn delivered_prompt_to_an_idle_background_bucket_starts_a_fresh_clock() {
        let mut app = App::test_default();
        app.set_session_id(Some(crate::agent::model::SessionId::new("session-a".to_owned())));
        app.status = AppStatus::Running;
        let background_key = seed_background_bucket(&mut app, "session-b");

        crate::app::events::client::apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::CronPromptAppended {
                session_id: "session-b".to_owned(),
                text: "check the queue".to_owned(),
            },
        );

        let background = app.sessions.get(&background_key).expect("bucket");
        assert!(
            background.live_turn.started_at.is_some(),
            "the fresh branch starts the accumulator, so the first usage frame stamps the \
             delivery time rather than restarting the row's elapsed",
        );
        let placeholder = background
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, MessageRole::Assistant))
            .expect("the delivered turn opens an assistant placeholder");
        assert!(placeholder.turn_info.started_at.is_some(), "the row carries the fresh clock");
    }

    /// A delivered dispatch that fails surfaces as TurnError for the
    /// target key (the workspace mirrors the typed-submit
    /// compensation), and the turn-error path must then drop the live
    /// clock and the unsettled row - a failed delivery must not leave a
    /// bar counting forever beside a Running spinner.
    #[test]
    fn a_turn_error_settles_a_delivered_turn_bar() {
        use crate::app::session::SessionLifecycleState;
        let mut app = App::test_default();
        app.set_session_id(Some(crate::agent::model::SessionId::new("session-a".to_owned())));
        let background_key = seed_background_bucket(&mut app, "session-b");

        crate::app::events::client::apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::CronPromptAppended {
                session_id: "session-b".to_owned(),
                text: "check the queue".to_owned(),
            },
        );
        assert!(
            app.sessions.get(&background_key).expect("bucket").live_turn.started_at.is_some(),
            "fixture guard: the delivered turn opened a clock",
        );

        crate::app::events::client::apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::TurnError {
                key: background_key.clone(),
                message: "dispatch failed".to_owned(),
                class: None,
                terminal_reason: None,
            },
        );

        let background = app.sessions.get(&background_key).expect("bucket");
        assert!(
            background.live_turn.started_at.is_none(),
            "no live clock survives the failed delivery",
        );
        let live_bars: Vec<_> = background
            .messages
            .iter()
            .filter(|m| matches!(m.role, MessageRole::Assistant))
            .filter(|m| !m.turn_info.is_empty() && !m.turn_info.is_settled())
            .collect();
        assert!(
            live_bars.is_empty(),
            "no live bar remains after the failed delivery; got {} unsettled rows",
            live_bars.len(),
        );
        assert_eq!(
            background.lifecycle_state,
            SessionLifecycleState::Idle,
            "the bucket spinner drops with the failed turn",
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
        ));
        app.active_messages_mut().push(ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::Text(TextBlock::from_complete("prior reply"))],
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
            terminal_output: None,
            monitor_output_tail: Vec::default(),
            monitor_status: None,
            render_epoch: 0,
            layout_epoch: 0,
            last_measured_width: 0,
            last_measured_height: 0,
            last_measured_layout_epoch: 0,
            last_measured_layout_generation: 0,
            last_measured_tools_collapsed: false,
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
        let child_block = child.id.clone();
        app.push_message_tracked(ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::ToolCall(Box::new(root)), MessageBlock::ToolCall(Box::new(child))],
        ));
        // Indexed so the TaskStarted below re-adopts the terminal card
        // instead of synthesizing over it.
        let root_msg = app.messages().len() - 1;
        app.index_tool_call(root_id.to_owned(), root_msg, 0);
        app.index_tool_call(child_block, root_msg, 1);
        // The real producer, sticky marker and all - the map entry alone
        // would leave the sticky-read gate in backgrounded_alive_tool_use_ids
        // unpinned by the departure assertions below.
        handle_sdk_message(
            app,
            Message::TaskStarted {
                task_id: task_id.to_owned(),
                description: "long-running background scan".to_owned(),
                uuid: String::new(),
                session_id: String::new(),
                tool_use_id: Some(root_id.to_owned()),
                task_type: Some("local_agent".to_owned()),
            },
        );
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

    fn card_status(app: &App, id: &str) -> ToolCallStatus {
        let (mi, bi) = app.lookup_tool_call(id).expect("indexed");
        match app.messages().get(mi).and_then(|m| m.blocks.get(bi)) {
            Some(MessageBlock::ToolCall(tc)) => tc.status,
            _ => panic!("expected ToolCall block for {id}"),
        }
    }

    /// The notification is the subagent's true completion, and it can
    /// land while the task is still rostered. Its open children settle
    /// here too: the later drain frame finds no mapping left to resolve
    /// and would otherwise strand them open until the next turn
    /// boundary (#789, second trigger).
    #[test]
    fn task_notification_settles_the_roots_open_children() {
        let mut app = App::test_default();
        seed_subagent(&mut app, "task-sub", "tu-root-sub", "tu-child-1");
        // Reopen the child: the shape where the notification is the only
        // settle signal left.
        let (mi, bi) = app.lookup_tool_call("tu-child-1").expect("child indexed");
        match app.active_messages_mut().get_mut(mi).and_then(|m| m.blocks.get_mut(bi)) {
            Some(MessageBlock::ToolCall(tc)) => tc.as_mut().status = ToolCallStatus::InProgress,
            _ => panic!("expected ToolCall block for tu-child-1"),
        }
        handle_sdk_message(&mut app, roster_changed(&["task-sub"]));
        assert_eq!(
            card_status(&app, "tu-child-1"),
            ToolCallStatus::InProgress,
            "precondition: the child is open before the notification",
        );

        handle_sdk_message(&mut app, task_notification("task-sub", "tu-root-sub"));

        assert_eq!(
            card_status(&app, "tu-child-1"),
            ToolCallStatus::Completed,
            "the true completion settles the root's open children; got {:?}",
            card_status(&app, "tu-child-1"),
        );
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
        // Drift guard: a payload that carries entries
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
        ));
        app.active_messages_mut().push(ChatMessage::new(MessageRole::Assistant, Vec::new()));

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

#[cfg(test)]
mod monitor_chat_block_tests {
    //! The chat lifecycle block over the wire sequence a real Monitor
    //! produces: `tool_use` -> `tool_result` -> `task_started` ->
    //! `task_notification` carrying `output_file` (CLI 2.1.220).
    use super::super::tool_updates;
    use super::{
        apply_tool_use_block, handle_task_notification, handle_task_progress, handle_task_started,
        handle_task_updated,
    };
    use crate::agent::model;
    use crate::app::{App, MessageBlock};
    use forge_primitives::Message;
    use forge_primitives::messages::TaskNotificationStatus;

    const TOOL_USE_ID: &str = "toolu_monitor";
    /// Narrow enough that the block must clip through the real render
    /// path, wide enough to still paint every row.
    const NARROW_RENDER_WIDTH: u16 = 40;
    const TASK_ID: &str = "task_monitor";

    fn monitor_input() -> serde_json::Value {
        serde_json::json!({
            "description": "tail-order probe",
            "command": "for i in 1 2 3; do echo line-$i; done",
            "persistent": false,
            "timeout_ms": 60000,
        })
    }

    /// Arm the Monitor the way the wire does. `apply_tool_use_block`
    /// populates `turn_state.tool_calls`; reaching for
    /// `handle_tool_call` directly does not, and the very next
    /// `task_started` then synthesizes a `"Task"` tool_use over the top
    /// of the block - renaming it and hiding it - while every
    /// tool_use_id-keyed assertion below stays green.
    fn arm_monitor(app: &mut App) {
        apply_tool_use_block(app, TOOL_USE_ID, "Monitor", &monitor_input(), None);
    }

    /// The second observable of #558's fix, and the one that reaches a
    /// live session. `Running` drops to `Thinking` once no tool call is
    /// in progress; a re-statement used to stamp the settled call back
    /// to `InProgress`, which held the spinner on `Running` on the
    /// strength of a call that had already finished.
    #[test]
    fn a_restated_settled_call_does_not_hold_the_spinner_on_running() {
        use crate::app::AppStatus;

        let mut app = App::test_default();
        arm_monitor(&mut app);
        settle_every_tool_call(&mut app);
        app.status = AppStatus::Running;

        arm_monitor(&mut app);

        assert_eq!(
            app.status,
            AppStatus::Thinking,
            "a re-statement of a finished call leaves nothing in progress to report",
        );
    }

    /// Drive both stores terminal, the way a `tool_result` would: the
    /// turn-state entry the walk reads and the rendered block
    /// `has_in_progress_tool_calls` counts.
    fn settle_every_tool_call(app: &mut App) {
        app.with_turn_state_mut(|ts| {
            for tc in ts.tool_calls.values_mut() {
                tc.status = forge_primitives::ToolCallStatus::Completed;
            }
        });
        for msg in app.active_messages_mut() {
            for block in &mut msg.blocks {
                if let MessageBlock::ToolCall(tc) = block {
                    tc.status = model::ToolCallStatus::Completed;
                }
            }
        }
    }

    fn task_started() -> Message {
        Message::TaskStarted {
            task_id: TASK_ID.to_owned(),
            description: "tail-order probe".to_owned(),
            uuid: String::new(),
            session_id: String::new(),
            tool_use_id: Some(TOOL_USE_ID.to_owned()),
            task_type: None,
        }
    }

    fn task_progress() -> Message {
        Message::TaskProgress {
            task_id: TASK_ID.to_owned(),
            uuid: String::new(),
            session_id: String::new(),
            tool_use_id: Some(TOOL_USE_ID.to_owned()),
            last_tool_name: None,
            description: String::new(),
            usage: forge_primitives::messages::TaskUsage {
                total_tokens: 0,
                tool_uses: 0,
                duration_ms: 0,
            },
            workflow_progress: Vec::new(),
        }
    }

    fn task_notification(output_file: &std::path::Path) -> Message {
        Message::TaskNotification {
            task_id: TASK_ID.to_owned(),
            status: TaskNotificationStatus::Completed,
            output_file: output_file.to_string_lossy().into_owned(),
            summary: "Monitor \"tail-order probe\" stream ended".to_owned(),
            uuid: String::new(),
            session_id: String::new(),
            tool_use_id: Some(TOOL_USE_ID.to_owned()),
            usage: None,
        }
    }

    fn with_tool_call<T>(app: &App, f: impl FnOnce(&crate::app::ToolCallInfo) -> T) -> T {
        let (mi, bi) = app.lookup_tool_call(TOOL_USE_ID).expect("Monitor stays indexed");
        let MessageBlock::ToolCall(tc) = &app.messages()[mi].blocks[bi] else {
            panic!("expected a ToolCall block");
        };
        f(tc)
    }

    /// Render the active session's chat exactly as the app does.
    fn rendered_chat(app: &mut App) -> String {
        rendered_chat_at(app, 80)
    }

    fn rendered_chat_at(app: &mut App, width: u16) -> String {
        let spinner = crate::ui::message::SpinnerState {
            glyph: '\u{280B}',
            is_active_turn_assistant: false,
            show_empty_thinking: false,
            show_thinking: false,
            show_compacting: false,
            running_subagents: None,
            live_turn_running: false,
        };
        let mut out = String::new();
        for idx in 0..app.messages().len() {
            let mut lines = Vec::new();
            let Some(msg) = app.active_messages_mut().get_mut(idx) else { continue };
            crate::ui::message::render_message(
                msg,
                &spinner,
                crate::ui::message::MessageRenderContext::new(
                    None,
                    width,
                    0,
                    crate::ui::message::MessageRenderOptions {
                        tools_collapsed: true,
                        ..Default::default()
                    },
                ),
                &mut lines,
            );
            for line in &lines {
                out.push_str(&line.spans.iter().map(|s| s.content.as_ref()).collect::<String>());
                out.push('\n');
            }
        }
        out
    }

    /// Assert against what actually PAINTS, not against fields. Every
    /// assertion in this module is about what the chat block shows, and
    /// a block can stop being renderable while every tool_use_id-keyed
    /// field on it stays correct - which is exactly how this module was
    /// blind. A field check can be fooled by that; a render check
    /// cannot, because a renamed or hidden block paints nothing.
    fn assert_block_still_renders(app: &mut App, when: &str) {
        let rendered = rendered_chat(app);
        assert!(
            rendered.contains("Monitor") && rendered.contains("tail-order probe"),
            "{when}: the Monitor block must still paint; got:\n{rendered}",
        );
    }

    /// `monitor_output_tail` feeds the live block's tree rows. Its only
    /// reader was dead code until the chat block un-hid, so this pins
    /// that the wire actually fills it - last five lines, oldest first.
    #[test]
    fn task_notification_fills_the_chat_tail_oldest_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let output_file = dir.path().join("monitor.output");
        let seeded: Vec<String> = (1..=8).map(|i| format!("FORGEPROBE line-{i}")).collect();
        std::fs::write(&output_file, seeded.join("\n") + "\n").expect("seed the output file");

        let mut app = App::test_default();
        arm_monitor(&mut app);
        handle_task_started(&mut app, task_started());
        handle_task_notification(&mut app, task_notification(&output_file));

        assert_block_still_renders(&mut app, "after task_notification");
        assert_eq!(
            with_tool_call(&app, |tc| tc.monitor_output_tail.clone()),
            (4..=8).map(|i| format!("FORGEPROBE line-{i}")).collect::<Vec<_>>(),
            "the chat tail carries the last five lines, oldest first",
        );
    }

    /// The Monitor `tool_result` is the "Monitor started (task ...)"
    /// ack and lands seconds after arming, while the watched command
    /// still runs. It drives the tool call terminal, so the block
    /// renders its completed one-liner for the whole live window.
    #[test]
    fn tool_result_ack_does_not_end_the_monitor_block() {
        let mut app = App::test_default();
        arm_monitor(&mut app);
        assert_eq!(
            with_tool_call(&app, |tc| tc.status),
            model::ToolCallStatus::InProgress,
            "the tool_use arrives in flight",
        );

        tool_updates::handle_tool_call_update_session(
            &mut app,
            &model::RenderToolCallUpdate::new(
                TOOL_USE_ID,
                model::RenderToolCallUpdateFields {
                    status: Some(model::ToolCallStatus::Completed),
                    raw_output: Some(serde_json::json!(
                        "Monitor started (task task_monitor, timeout 60000ms)."
                    )),
                    ..Default::default()
                },
            ),
        );

        assert_eq!(
            with_tool_call(&app, |tc| tc.status),
            model::ToolCallStatus::Completed,
            "the ack alone marks the tool call terminal",
        );
        assert_block_still_renders(&mut app, "after the tool_result ack");
        assert_eq!(
            with_tool_call(&app, |tc| tc.monitor_status),
            Some(crate::app::MonitorStatus::Running),
            "the monitor itself is still running, so the chat block stays live",
        );
    }

    /// The block's height swings as the tail fills and again when it
    /// collapses, so a stamp has to invalidate the viewport's cached
    /// prefix-sum too - marking the tool dirty only rebuilds the
    /// render. Same contract the backgrounded-`Bash` stream holds.
    #[test]
    fn stamping_the_block_invalidates_the_cached_message_height() {
        let mut app = App::test_default();
        arm_monitor(&mut app);
        handle_task_started(&mut app, task_started());
        let (msg_idx, _) = app.lookup_tool_call(TOOL_USE_ID).expect("indexed");

        // Settle THIS message's height so later staleness is ours.
        // `set_message_height` sizes + stores but leaves the stale bit,
        // so clear it explicitly to establish the precondition.
        app.active_viewport_mut().set_message_height(msg_idx, 4);
        app.active_viewport_mut().stale_message_heights[msg_idx] = false;
        assert!(
            !app.active_viewport_mut().stale_message_heights[msg_idx],
            "precondition: this message is not awaiting remeasure",
        );

        // Drive the stamp DIRECTLY. Going through
        // `handle_task_notification` also runs the summary update,
        // which invalidates on its own - the assertion would then pass
        // with this fix removed entirely.
        let bytes_before = app.message_retained_bytes().get(msg_idx).copied().unwrap_or(0);
        app.replace_monitor_output_tail_by_task_id(TASK_ID, &["one".to_owned(), "two".to_owned()]);
        assert!(
            app.active_viewport_mut().stale_message_heights[msg_idx],
            "the tail stamp changed the block's height and must schedule a remeasure",
        );
        // The retained-bytes cache feeds history trimming, so it has to
        // track the tail growing and shrinking too - the same set
        // `terminal.rs` does for a backgrounded Bash stream.
        assert_ne!(
            app.message_retained_bytes().get(msg_idx).copied().unwrap_or(0),
            bytes_before,
            "the tail stamp changed the message's retained size",
        );

        // Same for the liveness stamp: the block collapses from the
        // tail tree back to a single row.
        app.active_viewport_mut().stale_message_heights[msg_idx] = false;
        app.set_monitor_status_by_task_id(TASK_ID, crate::app::MonitorStatus::Stopped);
        assert!(
            app.active_viewport_mut().stale_message_heights[msg_idx],
            "collapsing to the summary row changed the height and must schedule a remeasure",
        );

        // Re-stamping the SAME status must not invalidate again. A
        // timer-polled monitor re-runs this path on every tick, and an
        // unconditional invalidate would remeasure the message forever
        // for a block whose shape never changed. The tail stamp already
        // guards this; the liveness stamp has to as well.
        app.active_viewport_mut().stale_message_heights[msg_idx] = false;
        app.set_monitor_status_by_task_id(TASK_ID, crate::app::MonitorStatus::Stopped);
        assert!(
            !app.active_viewport_mut().stale_message_heights[msg_idx],
            "an unchanged status is a no-op, not a remeasure",
        );
    }

    /// The synthesis path in `apply_tool_progress_update` fires when
    /// `turn_state` has been reset, which any frame arriving after its
    /// launching turn does. It must not push a `"Task"` tool_use over
    /// a block that already exists - that renames it and hides it, and
    /// every tool_use_id-keyed stamp keeps working regardless, so
    /// nothing downstream notices.
    #[test]
    fn an_out_of_turn_progress_frame_cannot_rename_or_hide_the_block() {
        let mut app = App::test_default();
        arm_monitor(&mut app);
        handle_task_started(&mut app, task_started());
        // Turn finalisation: the mapping the progress path consults is
        // gone, so the next frame for this id takes the synthesis branch.
        let _: () = app.with_turn_state_mut(|ts| ts.tool_calls.clear());

        handle_task_progress(&mut app, task_progress());

        assert_block_still_renders(&mut app, "after an out-of-turn task_progress");
    }

    /// `monitor_status` is deliberately absent from
    /// `update_existing_tool_call`'s sync set, and the field doc says
    /// so - but a comment is not a guard. Adding it there rebuilds from
    /// a fresh `ToolCallInfo`, and once `clear_monitors_if_all_terminal`
    /// has drained the entry that rebuild resolves `None`, which renders
    /// as still-running. This pins the outcome so the edit fails loudly.
    #[test]
    fn a_later_wire_update_cannot_reopen_a_collapsed_block() {
        let mut app = App::test_default();
        arm_monitor(&mut app);
        handle_task_started(&mut app, task_started());
        app.set_monitor_status_by_task_id(TASK_ID, crate::app::MonitorStatus::Completed);
        app.clear_monitors_if_all_terminal();
        assert!(app.monitors().is_empty(), "precondition: the entry has been drained");

        // A re-delivered tool_use for the same id goes through
        // `update_existing_tool_call`, which is where the forbidden
        // sync would live. `handle_tool_call` directly is the only way
        // to reach that path - `apply_tool_use_block` routes an
        // already-known id to the field-patch path instead.
        super::super::tool_calls::handle_tool_call(
            &mut app,
            model::RenderToolCall::new(TOOL_USE_ID, "Monitor")
                .status(model::ToolCallStatus::Completed)
                .meta(serde_json::json!({"claudeCode": {"toolName": "Monitor"}}))
                .raw_input(monitor_input()),
        );

        let rendered = rendered_chat(&mut app);
        assert!(
            rendered.contains("completed"),
            "the block stays collapsed after a later update; got:\n{rendered}",
        );
        assert!(
            !rendered.contains("$ for i in"),
            "a reopened block would paint the live command row; got:\n{rendered}",
        );
    }

    /// The live multi-row shape - header, command, tail tree - has no
    /// end-to-end coverage otherwise; the unit tests call the renderer
    /// directly and the replay snapshot only ever caught the collapsed
    /// one-liner.
    #[test]
    fn live_monitor_block_paints_its_tail_tree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let output_file = dir.path().join("monitor.output");
        let seeded: Vec<String> = (1..=3).map(|i| format!("build step {i}")).collect();
        std::fs::write(&output_file, seeded.join("\n") + "\n").expect("seed the output file");

        let mut app = App::test_default();
        arm_monitor(&mut app);
        handle_task_started(&mut app, task_started());
        app.set_monitor_output_file_by_task_id(TASK_ID, output_file.clone());
        app.refresh_monitor_output_tail_from_file(TASK_ID);

        insta::assert_snapshot!(rendered_chat(&mut app).trim_end());
    }

    /// Refusing to synthesize must not also drop the re-registration
    /// the synthesis used to do. `apply_tool_summary_update`
    /// early-returns without a `turn_state` entry, so a task whose
    /// notification arrives after its turn ended would sit visible and
    /// stuck on in_progress with no content.
    #[test]
    fn an_out_of_turn_progress_frame_re_registers_the_turn_state_entry() {
        let mut app = App::test_default();
        arm_monitor(&mut app);
        handle_task_started(&mut app, task_started());
        let _: () = app.with_turn_state_mut(|ts| ts.tool_calls.clear());

        handle_task_progress(&mut app, task_progress());

        let readopted = app.with_turn_state(|ts| ts.tool_calls.get(TOOL_USE_ID).cloned());
        let readopted = readopted.expect("the progress frame re-registers the entry");
        // The real-name property rides `meta.claudeCode.toolName`, which
        // is what `resolve_sdk_tool_name` reads. Asserting on `title`
        // would prove nothing: `tool_title` has no Monitor arm, so it
        // returns the bare name whatever the entry was built from.
        assert_eq!(
            readopted
                .meta
                .as_ref()
                .and_then(|m| m.get("claudeCode"))
                .and_then(|c| c.get("toolName"))
                .and_then(serde_json::Value::as_str),
            Some("Monitor"),
            "re-registered under its real name, never as a Task",
        );
        assert_block_still_renders(&mut app, "after re-registration");
    }

    /// The width tests all call `render_lifecycle_one_liner` directly,
    /// so none of them notices if the production call site stops
    /// passing the real width. Drive one narrow case through
    /// `render_message` so the wiring itself is covered.
    #[test]
    fn the_block_is_clipped_to_the_real_render_width() {
        let mut app = App::test_default();
        apply_tool_use_block(
            &mut app,
            TOOL_USE_ID,
            "Monitor",
            &serde_json::json!({
                "description": "a description long enough that it cannot fit a narrow pane",
                "command": "gh run watch 18234567 --exit-status --repo busytools/forge",
                "persistent": true,
                "timeout_ms": 0,
            }),
            None,
        );
        handle_task_started(&mut app, task_started());

        let rendered = rendered_chat_at(&mut app, NARROW_RENDER_WIDTH);
        for row in rendered.lines() {
            assert!(
                unicode_width::UnicodeWidthStr::width(row) <= NARROW_RENDER_WIDTH as usize,
                "every painted row fits the render width {NARROW_RENDER_WIDTH}; got {}: {row:?}",
                unicode_width::UnicodeWidthStr::width(row),
            );
        }
        // Snapshot the exact shape as well: the per-row width check only
        // catches overflow, and the layout constants can over-clip
        // without any row growing.
        insta::assert_snapshot!(rendered.trim_end());
    }

    /// The re-adopted entry must carry the block's CURRENT status. This
    /// is the normal case, not an edge: the "Monitor started" ack drives
    /// the tool call terminal seconds after arming, so by the time any
    /// out-of-turn frame lands the call is already `Completed`. A
    /// re-adopt that defaults to `Pending` walks straight past the
    /// terminal guard and reopens a finished tool call.
    #[test]
    fn re_adopting_after_the_ack_does_not_reopen_a_terminal_tool_call() {
        let mut app = App::test_default();
        arm_monitor(&mut app);
        handle_task_started(&mut app, task_started());

        // The ack: the tool call goes terminal while the monitor runs on.
        tool_updates::handle_tool_call_update_session(
            &mut app,
            &model::RenderToolCallUpdate::new(
                TOOL_USE_ID,
                model::RenderToolCallUpdateFields {
                    status: Some(model::ToolCallStatus::Completed),
                    ..Default::default()
                },
            ),
        );
        assert_eq!(with_tool_call(&app, |tc| tc.status), model::ToolCallStatus::Completed);

        // Turn finalisation, then a frame that takes the re-adopt path.
        let _: () = app.with_turn_state_mut(|ts| ts.tool_calls.clear());
        handle_task_progress(&mut app, task_progress());

        assert_eq!(
            with_tool_call(&app, |tc| tc.status),
            model::ToolCallStatus::Completed,
            "a terminal tool call stays terminal across a re-adopt",
        );
    }

    /// A resumed session must not restore a finished monitor as live.
    /// Replay never re-drives the terminal `task_updated`, so liveness
    /// here comes from `upsert_monitor_from_tool_input` seeding replayed
    /// entries `Completed` - terminal, so the block collapses, and the
    /// success variant because the seed is a placeholder rather than
    /// evidence the watched command failed. The `ICON_FAILED` assertion
    /// below is what holds that second half.
    #[test]
    fn a_replayed_monitor_is_restored_terminal_not_live() {
        let mut app = App::test_default();
        app.replay_in_progress = true;
        arm_monitor(&mut app);
        let rendered = rendered_chat(&mut app);
        assert!(
            rendered.contains("completed"),
            "a replayed monitor collapses, and the seed is not evidence of failure; got:\n{rendered}",
        );
        assert!(
            !rendered.contains(crate::ui::theme::ICON_FAILED),
            "the replay seed must not paint a failure glyph; got:\n{rendered}",
        );
        assert!(
            !rendered.contains("$ for i in"),
            "a replayed monitor never paints the live command row; got:\n{rendered}",
        );
    }

    /// The wire's terminal `task_updated` is keyed by task_id and
    /// arrives after the launching turn finalised, so it routes
    /// straight to the entry - and has to reach the chat block too.
    #[test]
    fn terminal_task_updated_flips_the_chat_block_to_stopped() {
        let mut app = App::test_default();
        arm_monitor(&mut app);
        handle_task_started(&mut app, task_started());
        // Drive the WIRE handler, not the setter: `handle_task_updated`
        // is the only production path to a terminal monitor, and it is
        // where the wire's failed / killed / stopped folding happens.
        handle_task_updated(
            &mut app,
            Message::TaskUpdated {
                task_id: TASK_ID.to_owned(),
                patch: forge_primitives::messages::TaskUpdatePatch {
                    status: Some("killed".to_owned()),
                    end_time: None,
                },
                uuid: String::new(),
                session_id: String::new(),
            },
        );
        assert_block_still_renders(&mut app, "after terminal task_updated");
        assert_eq!(
            with_tool_call(&app, |tc| tc.monitor_status),
            Some(crate::app::MonitorStatus::Stopped),
        );
    }
}
