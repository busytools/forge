use super::super::{
    App, BlockCache, ChatMessage, IncrementalMarkdown, MessageBlock, MessageRole, TextBlock,
    TextBlockSpacing,
};
use crate::agent::model;

pub(super) fn reset_for_new_session(
    app: &mut App,
    session_id: model::SessionId,
    current_model: model::CurrentModel,
    mode: Option<super::super::ModeState>,
    preserve_current_welcome_tip: bool,
) {
    reset_session_identity_state(app, session_id, current_model, mode);
    reset_messages_for_new_session(app, preserve_current_welcome_tip);
    reset_input_state_for_new_session(app);
    reset_interaction_state_for_new_session(app);
    reset_render_state_for_new_session(app);
    reset_cache_and_footer_state_for_new_session(app);
}

fn reset_session_identity_state(
    app: &mut App,
    session_id: model::SessionId,
    current_model: model::CurrentModel,
    mode: Option<super::super::ModeState>,
) {
    app.bump_session_scope_epoch();
    app.set_session_id(Some(session_id));
    app.set_current_model(Some(current_model.clone()));
    app.set_mode(mode);
    app.config_options_mut().clear();
    if let Some(requested_id) = current_model.requested_id {
        app.config_options_mut()
            .insert("model".to_owned(), serde_json::Value::String(requested_id));
    }
    *app.login_hint_mut() = None;
    super::clear_compaction_state(app, false);
    *app.session_usage_mut() = super::super::SessionUsageState::default();
    app.set_runtime_session_state(None);
    app.set_observed_permission_mode(None);
    app.set_observed_effort(None);
    app.set_observed_assistant_model(None);
    app.set_pending_mode_rollback(None);
    app.set_pending_model_rollback(None);
    app.set_prompt_suggestion(None);
    app.set_last_rate_limit_update(None);
    app.should_quit = false;
    app.set_files_accessed(0);
    app.set_cancelled_turn_pending_hint(false);
    app.set_pending_cancel(false);
    app.set_account_info(None);
}

fn reset_messages_for_new_session(app: &mut App, preserve_current_welcome_tip: bool) {
    let preserved_tip_seed =
        preserve_current_welcome_tip.then(|| app.current_welcome_tip_seed()).flatten();
    app.clear_messages_tracked();
    *app.history_retention_stats_mut() = super::super::state::HistoryRetentionStats::default();
    let mut welcome = app.build_welcome_message();
    if let Some(tip_seed) = preserved_tip_seed {
        App::apply_welcome_tip_seed(&mut welcome, tip_seed);
    }
    app.push_message_tracked(welcome);
    app.sync_welcome_snapshot();
    *app.active_viewport_mut() = super::super::ChatViewport::new();
}

fn reset_input_state_for_new_session(app: &mut App) {
    app.input_mut().clear();
    app.help_open = false;
    *app.pending_submit_mut() = None;
    app.pending_paste_text_mut().clear();
    *app.pending_paste_session_mut() = None;
    *app.active_paste_session_mut() = None;
    app.pending_images_mut().clear();
}

fn reset_interaction_state_for_new_session(app: &mut App) {
    app.clear_tool_scope_tracking();
    app.active_tool_call_index_mut().clear();
    app.clear_active_session_background_task_registry();
    app.todos_mut().clear();
    app.focus = super::super::FocusManager::default();
    app.available_commands_mut().clear();
    app.available_agents_mut().clear();
    app.config.overlay = None;
}

fn reset_render_state_for_new_session(app: &mut App) {
    *app.selection_mut() = None;
    app.scrollbar_drag = None;
    app.rendered_chat_lines.clear();
    app.rendered_chat_area = ratatui::layout::Rect::default();
    app.rendered_input_lines.clear();
    app.rendered_input_area = ratatui::layout::Rect::default();
    *app.mention_mut() = None;
    crate::app::file_index::reset(app);
    *app.slash_mut() = None;
    *app.subagent_mut() = None;
    app.help_view = super::super::HelpView::default();
    app.help_dialog = crate::app::dialog::DialogState::default();
    app.help_visible_count = 0;
}

fn reset_cache_and_footer_state_for_new_session(app: &mut App) {
    *app.mcp_mut() = super::super::McpState::default();
    crate::app::usage::reset_for_session_change(app);
    crate::app::plugins::reset_for_session_change(app);
    app.force_redraw = true;
    app.needs_redraw = true;
}

/// True when the message's tail Text block renders as an inbound
/// envelope card: `detect_inbound` matches on the text prefix, so a
/// chunk merged into that block would paint inside the card body
/// rather than as its own turn.
fn tail_renders_as_envelope_card(msg: &ChatMessage) -> bool {
    matches!(msg.blocks.last(), Some(MessageBlock::Text(block))
        if crate::ui::peer_block::detect_inbound(&block.text).is_some())
}

/// Append one replayed user text chunk. `continues_previous` carries
/// the loop's block boundary: only a later Text block of the same
/// envelope glues into the bubble before it.
fn append_resume_user_message_chunk(
    app: &mut App,
    chunk: &model::ContentChunk,
    continues_previous: bool,
) {
    let model::RenderContentBlock::Text(text) = &chunk.content else {
        return;
    };
    if text.text.is_empty() {
        return;
    }

    if continues_previous
        && let Some(last) = app.active_messages_mut().last_mut()
        && matches!(last.role, MessageRole::User)
        && !tail_renders_as_envelope_card(last)
    {
        if let Some(MessageBlock::Text(block)) = last.blocks.last_mut() {
            block.text.push_str(&text.text);
            block.markdown.append(&text.text);
            block.cache.invalidate();
        } else {
            let mut incr = IncrementalMarkdown::default();
            incr.append(&text.text);
            last.blocks.push(MessageBlock::Text(TextBlock {
                text: text.text.clone(),
                cache: BlockCache::default(),
                markdown: incr,
                trailing_spacing: TextBlockSpacing::default(),
                peer_collapsed_override: None,
                peer_last_measured_y_in_msg: 0,
                peer_last_measured_height: 0,
                peer_last_measured_width: 0,
            }));
        }
        let last_idx = app.messages().len().saturating_sub(1);
        app.sync_after_message_tail_changed(last_idx);
        return;
    }

    let mut incr = IncrementalMarkdown::default();
    incr.append(&text.text);
    app.push_message_tracked(ChatMessage::new(
        MessageRole::User,
        vec![MessageBlock::Text(TextBlock {
            text: text.text.clone(),
            cache: BlockCache::default(),
            markdown: incr,
            trailing_spacing: TextBlockSpacing::default(),
            peer_collapsed_override: None,
            peer_last_measured_y_in_msg: 0,
            peer_last_measured_height: 0,
            peer_last_measured_width: 0,
        })],
    ));
}

/// Count the leading prefix of `messages` that are synthesized
/// queued_command-only User envelopes (one `ContentBlock::QueuedCommand`
/// and nothing else). Returns `Some(n)` when at least 2 such
/// envelopes sit adjacent; `None` otherwise so the normal
/// per-message dispatch handles singletons through the usual
/// `handle_queued_command_echo` path (which keeps the
/// `<task-notification>` filter co-located).
fn queued_only_user_run_len(messages: &[forge_primitives::Message]) -> Option<usize> {
    let count = messages.iter().take_while(|m| is_queued_only_user_envelope(m)).count();
    (count >= 2).then_some(count)
}

fn is_queued_only_user_envelope(msg: &forge_primitives::Message) -> bool {
    let forge_primitives::Message::User { message: envelope, .. } = msg else {
        return false;
    };
    envelope.content.len() == 1
        && matches!(envelope.content[0], forge_primitives::ContentBlock::QueuedCommand { .. })
}

/// Render N (≥ 2) queued-command prompts that share a JSONL flush
/// timestamp as a single user bubble. The first text block is a DIM
/// header (`Queued during the previous turn · N messages`), followed
/// by one `▸ <prompt>` text block per message. All blocks live
/// inside one [`MessageRole::User`] [`ChatMessage`] so the existing
/// `USER_MSG_BG` background stretches over the whole group - visually
/// a single bordered area, which is what option B's mockup showed.
fn push_queued_group(app: &mut App, prompts: &[String]) {
    app.clear_active_turn_assistant();
    let mut survivors: Vec<&String> = Vec::with_capacity(prompts.len());
    for prompt in prompts {
        if super::sdk_message::append_resume_envelope_if_present(app, prompt) {
            continue;
        }
        survivors.push(prompt);
    }
    match survivors.len() {
        0 => return,
        1 => {
            super::sdk_message::handle_queued_command_echo(app, survivors[0]);
            return;
        }
        _ => {}
    }
    let header = format!("Queued during the previous turn · {} messages", survivors.len());
    let mut blocks: Vec<MessageBlock> = Vec::with_capacity(survivors.len() + 1);
    blocks.push(MessageBlock::Text(TextBlock::from_complete(&header)));
    for prompt in &survivors {
        let body = format!("▸ {prompt}");
        blocks.push(MessageBlock::Text(TextBlock::from_complete(&body)));
    }
    app.push_message_tracked(ChatMessage::new(MessageRole::User, blocks));
    app.enforce_history_retention_tracked();
    tracing::debug!(
        target: crate::logging::targets::APP_INPUT,
        event_name = "queued_group_replayed",
        message = "pushed grouped user bubble for replayed queued_command run",
        outcome = "success",
        message_count = survivors.len(),
    );
}

/// True when a user-role turn's text is harness-injected scaffolding
/// never meant for the chat buffer: `<task-notification>` completion
/// notices and Claude Code's slash-command wrappers. Live sessions
/// drop these as input echoes, so only resume re-renders them; both
/// replay filter sites route through here to stay in sync.
fn is_suppressed_user_scaffolding(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with("<task-notification>")
        || t.starts_with("<local-command-caveat>")
        || t.starts_with("<local-command-stdout>")
        || t.starts_with("<command-name>")
        || t.starts_with("<command-message>")
}

/// The content blocks a replayed envelope carries, empty for the
/// variants that carry none.
fn message_content_blocks(msg: &forge_primitives::Message) -> &[forge_primitives::ContentBlock] {
    match msg {
        forge_primitives::Message::Assistant { message, .. } => &message.content,
        forge_primitives::Message::User { message, .. } => &message.content,
        _ => &[],
    }
}

/// The tool call a result block reports on. Deliberately looser than
/// the walkers: a result the walker happens to drop still means the
/// result arrived, and reading it as absent would name a healthy call
/// as unfinished. The two shapes past `ToolResult` are absent from the
/// local corpus but reachable, so they are covered forward.
fn tool_result_target(block: &forge_primitives::ContentBlock) -> Option<&str> {
    use forge_primitives::ContentBlock;

    let id = match block {
        ContentBlock::ToolResult { tool_use_id, .. }
        | ContentBlock::ServerToolResult { tool_use_id, .. } => tool_use_id.as_str(),
        ContentBlock::Unknown { type_str, raw }
            if forge_workspace::tooling::is_tool_result_block_type(type_str) =>
        {
            raw.as_object()?.get("tool_use_id")?.as_str()?
        }
        _ => return None,
    };
    (!id.is_empty()).then_some(id)
}

/// A replayed `tool_use` whose result never arrived.
struct UnterminatedCall {
    id: String,
    name: String,
    /// Whether a user turn appears later in the history. Most of these
    /// calls are a user pressing Esc and carrying on rather than a
    /// session dying, and this separates the two. Named for what it
    /// measures rather than for the conclusion, so it cannot outlive the
    /// reasoning.
    user_turn_followed: bool,
}

/// Whether `msg` is the user taking a turn, as opposed to the wire
/// carrying tool output back. Both are `Message::User`, so a plain
/// variant match counts a sibling call's result as the conversation
/// continuing and reads `user_turn_followed` as true on a session that
/// died mid-batch.
fn is_user_prompt_turn(msg: &forge_primitives::Message) -> bool {
    matches!(msg, forge_primitives::Message::User { .. })
        && message_content_blocks(msg).iter().any(|b| tool_result_target(b).is_none())
}

/// Tool calls in `history` whose result never arrived, in first-seen
/// order and deduplicated by id so a re-stated `tool_use` counts once.
///
/// Read off the replayed envelopes, NOT off the post-walk sweep. The
/// sweep force-fails every call still in progress, which includes calls
/// that completed and were then re-stated (#558), so a count taken from
/// it fires on healthy resumes. Building `resolved` over the whole
/// history before looking at any `tool_use` is what makes that hold in
/// both orders: a call whose result arrived and was then re-stated is
/// excluded by its original result.
fn unterminated_tool_calls(history: &[forge_primitives::Message]) -> Vec<UnterminatedCall> {
    use forge_primitives::ContentBlock;

    let mut resolved: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for msg in history {
        for block in message_content_blocks(msg) {
            if let Some(id) = tool_result_target(block) {
                resolved.insert(id);
            }
        }
    }
    let last_user_idx = history.iter().rposition(is_user_prompt_turn).unwrap_or(0);

    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut unterminated = Vec::new();
    for (idx, msg) in history.iter().enumerate() {
        for block in message_content_blocks(msg) {
            let (ContentBlock::ToolUse { id, name, .. }
            | ContentBlock::ServerToolUse { id, name, .. }) = block
            else {
                continue;
            };
            if !id.is_empty() && !resolved.contains(id.as_str()) && seen.insert(id.as_str()) {
                unterminated.push(UnterminatedCall {
                    id: id.clone(),
                    name: name.clone(),
                    user_turn_followed: idx < last_user_idx,
                });
            }
        }
    }
    unterminated
}

/// Name every call the replayed history left without a result.
fn report_unterminated_tool_calls(app: &App, history: &[forge_primitives::Message]) {
    let unterminated = unterminated_tool_calls(history);
    if unterminated.is_empty() {
        return;
    }
    let session_id = super::tool_calls::current_session_id(app);
    for call in unterminated {
        tracing::warn!(
            target: crate::logging::targets::APP_TOOL,
            event_name = "resume_tool_call_unterminated",
            message = "resumed a tool call whose result never arrived",
            outcome = "failure",
            session_id = %session_id,
            tool_call_id = %call.id,
            tool_name = %call.name,
            user_turn_followed = call.user_turn_followed,
        );
    }
}

pub(super) fn load_resume_history(app: &mut App, history_messages: &[forge_primitives::Message]) {
    let preserved_tip_seed = app.current_welcome_tip_seed();
    app.clear_messages_tracked();
    *app.history_retention_stats_mut() = super::super::state::HistoryRetentionStats::default();
    let mut welcome = app.build_welcome_message();
    if let Some(tip_seed) = preserved_tip_seed {
        App::apply_welcome_tip_seed(&mut welcome, tip_seed);
    }
    app.push_message_tracked(welcome);
    app.sync_welcome_snapshot();
    // Replay marker: see `App.replay_in_progress` rustdoc + the
    // `handle_assistant` gate in `events/sdk_message.rs`. The flag
    // suppresses the lifecycle = Running write for historical
    // assistant envelopes so a resumed auto_start project doesn't
    // land in the Projects pane stuck on the spinner glyph.
    app.replay_in_progress = true;
    let mut i = 0;
    while i < history_messages.len() {
        // Mid-turn queued messages (claude CLI's `queuedCommands`
        // local buffer, flushed at the next user→model boundary)
        // get persisted to JSONL as `type:attachment,
        // attachment.type:queued_command` rows, one per submission.
        // The catalog scanner hoists each row into a synthetic
        // `{role:user, content:[{queued_command}]}` envelope - see
        // `userdata::catalog::scan::synthesize_queued_command_message`.
        //
        // Live forge already renders each submission at the time
        // the user typed it (via the input_submit path). Resume can
        // only place them at their JSONL position, which is the
        // flush time - every queued submission in a batch shares
        // the same millisecond timestamp. Rendered as individual
        // user bubbles, they cluster at the flush point and read
        // like a wall of unrelated messages.
        //
        // Group them: walk a run of consecutive synthesized
        // queued-only User envelopes, filter `<task-notification>`
        // harness plumbing, and emit ONE user bubble containing
        // all the surviving prompts. See GitHub issue #127.
        if let Some(run_len) = queued_only_user_run_len(&history_messages[i..]) {
            let prompts: Vec<String> = (i..i + run_len)
                .filter_map(|j| match &history_messages[j] {
                    forge_primitives::Message::User { message: envelope, .. } => {
                        envelope.content.iter().find_map(|b| match b {
                            forge_primitives::ContentBlock::QueuedCommand { prompt, .. } => {
                                Some(super::sdk_message::extract_queued_command_text(prompt))
                            }
                            _ => None,
                        })
                    }
                    _ => None,
                })
                .filter(|p| !is_suppressed_user_scaffolding(p))
                .collect();
            i += run_len;
            match prompts.len() {
                0 => continue,
                1 => super::sdk_message::handle_queued_command_echo(app, &prompts[0]),
                _ => push_queued_group(app, &prompts),
            }
            continue;
        }

        let msg = &history_messages[i];
        // The raw walker (`handle_sdk_message`) processes user
        // messages by walking tool_results only - live wire user
        // text content blocks are echoes of the user's input that
        // the input handler already rendered, so the walker
        // correctly drops them. Replay has no input handler
        // contribution, so render the user text content blocks here
        // before dispatch.
        if let forge_primitives::Message::User { message: envelope, .. } = msg {
            // Render replay-time user text content blocks. The live raw
            // walker drops user text (those are echoes of input the input
            // handler already rendered); replay has no input handler
            // contribution, so render here. Only clear the active-turn
            // assistant pointer when we're about to actually render - an
            // empty Text block isn't a render and shouldn't move the
            // pointer.
            let mut rendered_user_text = false;
            for block in &envelope.content {
                if let forge_primitives::ContentBlock::Text { text } = block {
                    if text.is_empty() {
                        continue;
                    }
                    if is_suppressed_user_scaffolding(text) {
                        continue;
                    }
                    let continues_previous = rendered_user_text;
                    if !rendered_user_text {
                        app.clear_active_turn_assistant();
                        tracing::debug!(
                            target: crate::logging::targets::APP_SESSION,
                            event_name = "resume_user_text_rendered",
                            message = "rendering user-text content block from session resume",
                            outcome = "success",
                        );
                        rendered_user_text = true;
                    }
                    // The dispatcher below paints an inbound envelope stamped.
                    if crate::ui::peer_block::detect_inbound(text).is_some() {
                        continue;
                    }
                    let chunk = model::ContentChunk::new(model::RenderContentBlock::Text(
                        model::TextContent::new(text.clone()),
                    ));
                    append_resume_user_message_chunk(app, &chunk, continues_previous);
                }
            }
        }
        super::sdk_message::handle_sdk_message(app, msg.clone());
        i += 1;
    }
    app.replay_in_progress = false;
    app.rebuild_render_cache_accounting();
    // #289: replay's orphan-marking path (#277 Bug 3b) leaves the
    // section populated with all-terminal entries. The live wire
    // path drains via `clear_*_if_all_terminal` at each status
    // flip; replay never makes that call. Mirror it here so the
    // post-resume Inspector matches the live behaviour: all-terminal
    // -> section disappears; mixed -> stays visible with the in-flight
    // entries.
    app.clear_monitors_if_all_terminal();
    app.clear_workflows_if_all_terminal();
    app.finalize_turn_runtime_artifacts(model::ToolCallStatus::Failed);
    report_unterminated_tool_calls(app, history_messages);
    app.clear_active_turn_assistant();
    app.enforce_history_retention_tracked();
    *app.active_viewport_mut() = super::super::ChatViewport::new();
    app.active_viewport_mut().engage_auto_scroll();
}

#[cfg(test)]
mod tests {
    //! Regression coverage for the launchpad-spinner-stuck bug.
    //! End-to-end: walking on-disk history through the shared SDK
    //! dispatcher must NOT leave the bucket's lifecycle stuck on
    //! `Running`. The unit-level gate lives in `events/sdk_message.rs`.
    //! The tests below cover a second contract on the same walk: which
    //! records the replay may emit, and which it must not.
    use super::load_resume_history;
    use crate::app::session::SessionLifecycleState;
    use crate::app::{App, MessageBlock, MessageRole};
    use forge_primitives::{AssistantEnvelope, ContentBlock, Message, UserEnvelope};
    use serde_json::Value;

    fn historical_assistant(text: &str) -> Message {
        Message::Assistant {
            message: AssistantEnvelope {
                id: "msg_history".to_owned(),
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

    fn historical_tool_use_named(id: &str, name: &str, input: Value) -> Message {
        Message::Assistant {
            message: AssistantEnvelope {
                id: "msg_history".to_owned(),
                role: "assistant".to_owned(),
                model: "claude-test".to_owned(),
                content: vec![ContentBlock::ToolUse {
                    id: id.to_owned(),
                    name: name.to_owned(),
                    input,
                }],
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

    fn historical_tool_use(id: &str) -> Message {
        historical_tool_use_named(id, "Bash", serde_json::json!({"command": "echo hi"}))
    }

    fn historical_tool_result(id: &str, is_error: bool) -> Message {
        historical_tool_result_text(id, is_error, if is_error { "boom" } else { "ok" })
    }

    fn historical_tool_result_text(id: &str, is_error: bool, text: &str) -> Message {
        Message::User {
            message: UserEnvelope {
                role: "user".to_owned(),
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: id.to_owned(),
                    content: Value::String(text.to_owned()),
                    is_error,
                }],
            },
            session_id: String::new(),
            parent_tool_use_id: None,
            uuid: None,
            tool_use_result: None,
        }
    }

    fn historical_server_tool_use(id: &str, name: &str) -> Message {
        Message::Assistant {
            message: AssistantEnvelope {
                id: "msg_history".to_owned(),
                role: "assistant".to_owned(),
                model: "claude-test".to_owned(),
                content: vec![ContentBlock::ServerToolUse {
                    id: id.to_owned(),
                    name: name.to_owned(),
                    input: Value::Null,
                }],
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

    fn user_with_content(content: Vec<ContentBlock>) -> Message {
        Message::User {
            message: UserEnvelope { role: "user".to_owned(), content },
            session_id: String::new(),
            parent_tool_use_id: None,
            uuid: None,
            tool_use_result: None,
        }
    }

    fn historical_server_tool_result(id: &str) -> Message {
        user_with_content(vec![ContentBlock::ServerToolResult {
            tool_use_id: id.to_owned(),
            content: Value::String("ok".to_owned()),
        }])
    }

    /// A wire tool-result variant the typed enum does not enumerate, so
    /// it lands as `Unknown` and is recognised by its `type_str`.
    fn historical_raw_tool_result(id: &str, type_str: &str) -> Message {
        user_with_content(vec![ContentBlock::Unknown {
            type_str: type_str.to_owned(),
            raw: serde_json::json!({
                "type": type_str,
                "tool_use_id": id,
                "content": "ok",
            }),
        }])
    }

    /// One entry per emitted record, with every field it carried, so a
    /// test can pin values and emission count rather than existence.
    #[derive(Clone)]
    struct CapturedEvent {
        level: tracing::Level,
        name: String,
        fields: Vec<(String, String)>,
    }

    impl CapturedEvent {
        fn field(&self, name: &str) -> Option<&str> {
            self.fields.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
        }
    }

    #[derive(Clone, Default)]
    struct EventCapture(std::sync::Arc<std::sync::Mutex<Vec<CapturedEvent>>>);

    impl EventCapture {
        fn names_at(&self, level: tracing::Level) -> Vec<String> {
            let seen = self.0.lock().expect("capture");
            seen.iter().filter(|e| e.level == level).map(|e| e.name.clone()).collect()
        }

        /// Every emission of `event_name`, in order.
        fn records_named(&self, event_name: &str) -> Vec<CapturedEvent> {
            let seen = self.0.lock().expect("capture");
            seen.iter().filter(|e| e.name == event_name).cloned().collect()
        }
    }

    #[derive(Default)]
    struct EventFieldVisitor(Vec<(String, String)>);

    impl tracing::field::Visit for EventFieldVisitor {
        // Every field arrives here - plain, `%` and `?` alike - because
        // the typed `record_*` arms forward to this one.
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0
                .push((field.name().to_owned(), format!("{value:?}").trim_matches('"').to_owned()));
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for EventCapture {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut visitor = EventFieldVisitor::default();
            event.record(&mut visitor);
            let fields = visitor.0;
            let name = fields
                .iter()
                .find(|(key, _)| key == "event_name")
                .map_or_else(|| event.metadata().name().to_owned(), |(_, value)| value.clone());
            self.0.lock().expect("capture").push(CapturedEvent {
                level: *event.metadata().level(),
                name,
                fields,
            });
        }
    }

    fn tool_call_status(app: &App, id: &str) -> Option<crate::agent::model::ToolCallStatus> {
        let (mi, bi) = app.lookup_tool_call(id)?;
        match app.messages().get(mi)?.blocks.get(bi)? {
            MessageBlock::ToolCall(tc) => Some(tc.status),
            _ => None,
        }
    }

    /// Returns the capture AND the walked `App`, so liveness can be
    /// asserted from state rather than from records.
    fn capture_replay_of(history: &[Message]) -> (EventCapture, App) {
        capture_replay_of_session(None, history)
    }

    /// [`capture_replay_of`] with the session id set, so a record's
    /// `session_id` can be asserted as populated rather than present.
    fn capture_replay_of_session(
        session_id: Option<&str>,
        history: &[Message],
    ) -> (EventCapture, App) {
        use tracing_subscriber::layer::SubscriberExt;

        let capture = EventCapture::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        let mut app = App::test_default();
        if let Some(id) = session_id {
            app.set_session_id(Some(crate::agent::model::SessionId::new(id.to_owned())));
        }
        tracing::subscriber::with_default(subscriber, || {
            load_resume_history(&mut app, history);
        });
        (capture, app)
    }

    /// A `TaskUpdate` naming an id the replay never saw warns, and the
    /// warning has to carry the session, the tool call and the task:
    /// without those it names a failure nobody can trace back.
    #[test]
    fn replay_warning_for_an_unknown_task_id_names_its_session_and_call() {
        let (capture, _app) = capture_replay_of_session(
            Some("sess-unknown-task"),
            &[
                historical_tool_use_named(
                    "toolu_orphan",
                    "TaskUpdate",
                    serde_json::json!({"taskId": "404", "status": "completed"}),
                ),
                historical_tool_result_text("toolu_orphan", false, "Task #404 updated"),
            ],
        );

        let record = capture
            .records_named("task_update_unknown_id")
            .into_iter()
            .find(|record| record.level == tracing::Level::WARN)
            .expect("a TaskUpdate against an absent id must warn");
        assert_eq!(record.field("session_id"), Some("sess-unknown-task"));
        assert_eq!(record.field("tool_call_id"), Some("toolu_orphan"));
        assert_eq!(record.field("task_id"), Some("404"));
    }

    /// The replayed shapes a tool call can reach: completed, failed,
    /// refused, plus the `TaskCreate` / `TaskUpdate` pair.
    /// `Killed` is absent because the JSONL parser admits only user,
    /// assistant and queued-command attachment rows, so no
    /// status-bearing wire message reaches the walk at all.
    fn replay_fixture() -> Vec<Message> {
        vec![
            historical_user_text("run something"),
            historical_tool_use("toolu_ok"),
            // Re-stated in flight - reaches the shared emitter's DEBUG arm.
            historical_tool_use_named(
                "toolu_ok",
                "Bash",
                serde_json::json!({"command": "echo hi again"}),
            ),
            historical_tool_result("toolu_ok", false),
            historical_tool_use("toolu_err"),
            historical_tool_result("toolu_err", true),
            historical_tool_use("toolu_slow"),
            historical_tool_result_text("toolu_slow", true, "the command timed out after 120s"),
            historical_tool_use("toolu_refused"),
            historical_tool_result_text("toolu_refused", true, "permission denied by the user"),
            historical_tool_use_named(
                "toolu_task",
                "TaskCreate",
                serde_json::json!({"subject": "ship it", "activeForm": "shipping it"}),
            ),
            historical_tool_result_text(
                "toolu_task",
                false,
                "Task #7 created successfully: ship it",
            ),
            historical_tool_use_named(
                "toolu_task_done",
                "TaskUpdate",
                serde_json::json!({"taskId": "7", "status": "completed"}),
            ),
            historical_tool_result_text("toolu_task_done", false, "Task #7 updated"),
            // A second task, reworded by its update: the first keeps its
            // create-time `active_form`, this one gets a new one, so a
            // gate blanking the field on either side has somewhere to
            // show. `in_progress` is the status the inspector renders
            // `active_form` for.
            historical_tool_use_named(
                "toolu_task2",
                "TaskCreate",
                serde_json::json!({"subject": "draft it", "activeForm": "drafting it"}),
            ),
            historical_tool_result_text(
                "toolu_task2",
                false,
                "Task #8 created successfully: draft it",
            ),
            historical_tool_use_named(
                "toolu_task2_upd",
                "TaskUpdate",
                serde_json::json!({
                    "taskId": "8",
                    "subject": "revise it",
                    "activeForm": "revising it",
                    "status": "in_progress"
                }),
            ),
            historical_tool_result_text("toolu_task2_upd", false, "Task #8 updated"),
            // An unrecognised status: the only shape that reaches the
            // unknown-status warning, and the reason that warning has a
            // test at all.
            historical_tool_use_named(
                "toolu_task2_odd",
                "TaskUpdate",
                serde_json::json!({"taskId": "8", "status": "cancelled"}),
            ),
            historical_tool_result_text("toolu_task2_odd", false, "Task #8 untouched"),
            historical_assistant("done"),
        ]
    }

    /// Every record the gate is meant to silence, one per emitting site.
    const SILENCED_ON_REPLAY: [&str; 6] = [
        "tool_call_received",
        "command_started",
        "command_completed",
        "tool_call_completed",
        "task_create_applied",
        "task_update_applied",
    ];

    /// `Visit::record_str` forwards to `record_debug`, so every way the
    /// macros write `event_name` arrives through that one arm. Breaking
    /// it fails this test and every test that reads a name through the
    /// capture, which is what deleting the redundant override bought.
    #[test]
    fn capture_reads_event_name_however_it_was_recorded() {
        use tracing_subscriber::layer::SubscriberExt;

        let capture = EventCapture::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(event_name = "plain", message = "str-recorded");
            tracing::info!(event_name = %"displayed", message = "display-recorded");
            tracing::warn!(event_name = ?"debugged", message = "debug-recorded");
        });

        assert_eq!(capture.names_at(tracing::Level::INFO), vec!["plain", "displayed"]);
        assert_eq!(capture.names_at(tracing::Level::WARN), vec!["debugged"]);
    }

    /// Replaying history re-renders envelopes that were already logged
    /// when they first happened, so the walk must not re-emit them as
    /// operational records.
    #[test]
    fn replay_walk_emits_no_success_records() {
        let (capture, app) = capture_replay_of(&replay_fixture());

        assert_eq!(
            tool_call_status(&app, "toolu_ok"),
            Some(crate::agent::model::ToolCallStatus::Completed),
            "a completed call must end the walk terminal, not merely exist",
        );
        assert!(app.lookup_tool_call("toolu_err").is_some(), "the failed call never landed");
        assert!(app.lookup_tool_call("toolu_refused").is_some(), "the refused call never landed");
        assert!(!app.todos().is_empty(), "the Task pair never reached the inspector");

        // Closed world rather than a deny-list.
        let mut info = capture.names_at(tracing::Level::INFO);
        info.sort_unstable();
        info.dedup();
        assert_eq!(
            info,
            vec!["tool_call_refused".to_owned()],
            "a replay may emit exactly one distinct INFO name, the refusal. Adding another is a \
             deliberate decision - record it in SILENCED_ON_REPLAY or here, not by widening this \
             assertion in passing",
        );
        // At every level, not only INFO: re-levelling a gated record
        // still writes it, and `app.command` is at debug in the default
        // directives, so a record re-levelled there reaches the ring.
        // ERROR matters most - it sits above every default directive,
        // so nothing downstream can filter it. TRACE is a forward
        // guard; the walk emits nothing there today.
        for level in [
            tracing::Level::ERROR,
            tracing::Level::WARN,
            tracing::Level::DEBUG,
            tracing::Level::TRACE,
        ] {
            let names = capture.names_at(level);
            for silenced in SILENCED_ON_REPLAY {
                assert!(
                    !names.iter().any(|name| name == silenced),
                    "replay emitted `{silenced}` at {level}, saw {names:?}",
                );
            }
        }
    }

    /// The other half of the same contract: quieting the replay must
    /// not quiet what went wrong in it. `tool_call_refused` is INFO
    /// upstream but is a failure, which is why the gate keys on the
    /// record's outcome rather than on its level.
    #[test]
    fn replay_walk_still_reports_what_went_wrong() {
        let (capture, _app) = capture_replay_of(&replay_fixture());

        let warnings = capture.names_at(tracing::Level::WARN);
        for expected in [
            "command_failed",
            "tool_call_failed",
            "tool_call_timeout",
            "task_update_unknown_status",
        ] {
            assert!(
                warnings.iter().any(|name| name == expected),
                "replay lost `{expected}`, saw {warnings:?}",
            );
        }
        let info = capture.names_at(tracing::Level::INFO);
        assert!(
            info.iter().any(|name| name == "tool_call_refused"),
            "a refusal is a failure wearing an INFO level - it must survive, saw {info:?}",
        );
        let debug = capture.names_at(tracing::Level::DEBUG);
        assert!(
            debug.iter().any(|name| name == "tool_call_updated"),
            "the shared emitter's DEBUG arm must survive, saw {debug:?}",
        );
        assert!(
            debug.iter().any(|name| name == "resume_user_text_rendered"),
            "the replay path's own debug trail is deliberately kept, saw {debug:?}",
        );
    }

    const UNTERMINATED: &str = "resume_tool_call_unterminated";

    /// Resuming the session you would open to diagnose a mid-tool death
    /// has to leave a record naming what was running. One per call that
    /// never saw a result, carrying the session and the call, because a
    /// log ring multiplexes every session and "died mid-tool" is only
    /// half the question - the other half is "doing what".
    #[test]
    fn replay_names_each_tool_call_that_never_saw_a_result() {
        let history = vec![
            historical_user_text("run something"),
            historical_tool_use("toolu_done"),
            historical_tool_result("toolu_done", false),
            historical_tool_use_named(
                "toolu_dead",
                "Bash",
                serde_json::json!({"command": "sleep 900"}),
            ),
            historical_tool_use_named(
                "toolu_dead_two",
                "Read",
                serde_json::json!({"file_path": "/tmp/x"}),
            ),
        ];
        let (capture, _app) = capture_replay_of_session(Some("sess-resume-1"), &history);

        let records = capture.records_named(UNTERMINATED);
        let ids: Vec<Option<&str>> = records.iter().map(|r| r.field("tool_call_id")).collect();
        assert_eq!(
            records.len(),
            2,
            "one record per unterminated call, no more and no fewer: {ids:?}",
        );
        assert_eq!(
            ids,
            vec![Some("toolu_dead"), Some("toolu_dead_two")],
            "each record names its own call, in the order the walk met them",
        );
        assert_eq!(
            records.iter().map(|r| r.field("tool_name")).collect::<Vec<_>>(),
            vec![Some("Bash"), Some("Read")],
            "the record answers `doing what`, not only `how many`",
        );
        for record in &records {
            assert_eq!(record.level, tracing::Level::WARN, "an unfinished call is not a success");
            assert_eq!(record.field("outcome"), Some("failure"));
            assert_eq!(
                record.field("session_id"),
                Some("sess-resume-1"),
                "unattributable in a multiplexed ring without this",
            );
            assert_eq!(
                record.field("user_turn_followed"),
                Some("false"),
                "nothing follows these calls, so the session did end on them",
            );
        }
    }

    /// Most of these calls are a user pressing Esc and carrying on, not
    /// a session dying - 32 of 35 across the local transcripts had a
    /// user turn after them. Without the distinction, someone reading
    /// the log for why a session died wades through interruptions, so
    /// the field has to separate the two rather than filter one away.
    #[test]
    fn replay_separates_an_interrupted_call_from_one_the_session_ended_on() {
        let history = vec![
            historical_user_text("run something"),
            historical_tool_use_named("toolu_esc", "Bash", serde_json::json!({"command": "sleep"})),
            historical_user_text("never mind, do this instead"),
            historical_tool_use_named("toolu_died", "Bash", serde_json::json!({"command": "make"})),
        ];
        let (capture, _app) = capture_replay_of(&history);

        let records = capture.records_named(UNTERMINATED);
        let by_id: Vec<(Option<&str>, Option<&str>)> = records
            .iter()
            .map(|r| (r.field("tool_call_id"), r.field("user_turn_followed")))
            .collect();
        assert_eq!(
            by_id,
            vec![(Some("toolu_esc"), Some("true")), (Some("toolu_died"), Some("false"))],
            "the interrupted call stays visible, and only the last one reads as a session end",
        );
    }

    /// The count has to come from calls that genuinely never saw a
    /// result, not from the post-walk sweep. A resume whose results all
    /// arrived emits nothing, in each shape a result arrives in.
    #[test]
    fn replay_names_nothing_when_every_result_arrived() {
        let (capture, _app) = capture_replay_of(&replay_fixture());
        assert!(
            capture.records_named(UNTERMINATED).is_empty(),
            "the standard fixture terminates every call",
        );

        let exotic = vec![
            historical_user_text("run something"),
            historical_tool_use_named("toolu_mcp", "mcp__x__y", serde_json::json!({})),
            historical_raw_tool_result("toolu_mcp", "mcp_tool_result"),
            historical_server_tool_use("toolu_srv", "web_search"),
            historical_server_tool_result("toolu_srv"),
        ];
        let (capture, app) = capture_replay_of(&exotic);
        assert!(
            app.lookup_tool_call("toolu_mcp").is_some(),
            "the fixture has to reach the walk for its silence to mean anything",
        );
        assert!(
            capture.records_named(UNTERMINATED).is_empty(),
            "a result counts however it is shaped on the wire, saw {:?}",
            capture
                .records_named(UNTERMINATED)
                .iter()
                .map(|r| r.field("tool_call_id"))
                .collect::<Vec<_>>(),
        );
    }

    /// A tool result is a `Message::User` too, so "did a user turn
    /// follow" cannot be answered by matching that variant: a sibling
    /// call's result arriving would read as the conversation continuing.
    /// This is the shape that separates them - a batch of two where one
    /// result lands and the other never does, and nothing after it. The
    /// session died mid-batch, so neither call was interrupted.
    #[test]
    fn a_sibling_result_is_not_the_user_taking_a_turn() {
        let batch = Message::Assistant {
            message: AssistantEnvelope {
                id: "msg_history".to_owned(),
                role: "assistant".to_owned(),
                model: "claude-test".to_owned(),
                content: vec![
                    ContentBlock::ToolUse {
                        id: "toolu_landed".to_owned(),
                        name: "Bash".to_owned(),
                        input: serde_json::json!({"command": "echo a"}),
                    },
                    ContentBlock::ToolUse {
                        id: "toolu_lost".to_owned(),
                        name: "Bash".to_owned(),
                        input: serde_json::json!({"command": "echo b"}),
                    },
                ],
                stop_reason: None,
                stop_sequence: None,
                usage: None,
            },
            session_id: String::new(),
            parent_tool_use_id: None,
            error: None,
            uuid: None,
        };
        let history = vec![
            historical_user_text("run both"),
            batch,
            historical_tool_result("toolu_landed", false),
        ];
        let (capture, _app) = capture_replay_of(&history);

        let records = capture.records_named(UNTERMINATED);
        assert_eq!(
            records
                .iter()
                .map(|r| (r.field("tool_call_id"), r.field("user_turn_followed")))
                .collect::<Vec<_>>(),
            vec![(Some("toolu_lost"), Some("false"))],
            "the surviving sibling's result is not the user carrying on",
        );
    }

    /// #558's shape is result THEN re-statement, and that order is what
    /// discriminates: resolving per-id by last-event-wins - which is
    /// what the sweep effectively sees - leaves the completed call
    /// looking unfinished. Building the resolved set over the whole
    /// history first is what makes both orders agree. Its own fixture,
    /// because it needs a second id that never resolves to exercise the
    /// dedup.
    #[test]
    fn a_completed_call_re_stated_afterwards_is_not_named() {
        let history = vec![
            historical_user_text("run something"),
            historical_tool_use("toolu_settled"),
            historical_tool_result("toolu_settled", false),
            // The re-statement lands AFTER its own result.
            historical_tool_use_named(
                "toolu_settled",
                "Bash",
                serde_json::json!({"command": "echo hi again"}),
            ),
            // A second re-statement of a call that never resolved: the
            // dedup has to fold these into one record, not two.
            historical_tool_use_named("toolu_open", "Read", serde_json::json!({"file_path": "/a"})),
            historical_tool_use_named("toolu_open", "Read", serde_json::json!({"file_path": "/a"})),
        ];
        let (capture, _app) = capture_replay_of(&history);

        assert_eq!(
            capture
                .records_named(UNTERMINATED)
                .iter()
                .map(|r| r.field("tool_call_id"))
                .collect::<Vec<_>>(),
            vec![Some("toolu_open")],
            "a re-stated completed call is settled by its own result, and a re-stated \
             unresolved one is named once",
        );
    }

    /// The neighbour above pins the WARN record; this pins what the user
    /// opens the resumed chat to see. A `tool_use` re-stated after its
    /// own result used to stamp the call back to `InProgress`, and the
    /// post-walk sweep force-fails whatever is still in progress - so a
    /// call that succeeded rendered as failed. Asserted against its own
    /// control so the fixture cannot drift into passing vacuously.
    #[test]
    fn a_re_stated_tool_use_leaves_a_completed_call_completed() {
        let settled = vec![
            historical_user_text("run something"),
            historical_tool_use("toolu_ok"),
            historical_tool_result("toolu_ok", false),
        ];
        let mut re_stated = settled.clone();
        re_stated.push(historical_tool_use("toolu_ok"));

        let status_of = |history: &[Message]| {
            let (_capture, app) = capture_replay_of(history);
            tool_call_status(&app, "toolu_ok")
        };

        assert_eq!(
            (status_of(&settled), status_of(&re_stated)),
            (
                Some(crate::agent::model::ToolCallStatus::Completed),
                Some(crate::agent::model::ToolCallStatus::Completed)
            ),
            "a completed call stays completed whether or not its tool_use is re-stated",
        );
    }

    /// The gate decides which records are written. It must never decide
    /// what the user ends up looking at - so a replayed walk and a live
    /// walk over the same envelopes have to land the same state. This
    /// covers the state the task-delta block can reach, which is where
    /// a gate can suppress one.
    ///
    /// Deliberately not a general state diff. Below the gate, the four
    /// `&App` sites only read fields and hand them to a tracing macro,
    /// so there is no state for them to suppress. A wider fingerprint
    /// would also fail on clean code, since a replay legitimately adds
    /// a welcome message, renders user text the live walker drops, and
    /// rebuilds render-cache accounting.
    ///
    /// "Live" here means the reducer: `replay_in_progress` is set only
    /// inside `load_resume_history` and every reader sits below
    /// `apply_session_update`, so a divergence introduced above that
    /// line is a routing defect and a different test's job.
    #[test]
    fn replay_and_live_walks_land_identical_state() {
        let fixture = replay_fixture();
        let (_capture, replayed) = capture_replay_of(&fixture);

        let mut live = App::test_default();
        for msg in fixture {
            super::super::sdk_message::handle_sdk_message(&mut live, msg);
        }

        for id in [
            "toolu_ok",
            "toolu_err",
            "toolu_slow",
            "toolu_refused",
            "toolu_task",
            "toolu_task_done",
            "toolu_task2",
            "toolu_task2_upd",
            "toolu_task2_odd",
        ] {
            assert_eq!(
                tool_call_status(&replayed, id),
                tool_call_status(&live, id),
                "replay and live disagree on the status of `{id}`",
            );
        }

        let todos_of = |app: &App| {
            app.todos()
                .iter()
                // Destructured so a new `TodoItem` field is a compile
                // error here rather than a silently uncompared one.
                .map(|crate::app::TodoItem { id, content, status, active_form }| {
                    (id.clone(), content.clone(), status.clone(), active_form.clone())
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            todos_of(&replayed),
            todos_of(&live),
            "replay and live disagree on the inspector's task list",
        );
    }

    /// The gate has to be OFF everywhere else: a live tool call is an
    /// event, and these records are how the log says so.
    #[test]
    fn live_tool_calls_still_emit_their_operational_records() {
        use tracing_subscriber::layer::SubscriberExt;

        let capture = EventCapture::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        tracing::subscriber::with_default(subscriber, || {
            let mut app = App::test_default();
            assert!(!app.replay_in_progress, "live path: the gate must be off");
            for msg in replay_fixture() {
                super::super::sdk_message::handle_sdk_message(&mut app, msg);
            }
        });

        let info = capture.names_at(tracing::Level::INFO);
        for expected in SILENCED_ON_REPLAY {
            assert!(
                info.iter().any(|name| name == expected),
                "live path lost `{expected}`, saw {info:?}",
            );
        }
    }

    fn synthesized_queued(prompt: &str) -> Message {
        Message::User {
            message: UserEnvelope {
                role: "user".to_owned(),
                content: vec![ContentBlock::QueuedCommand {
                    prompt: Value::String(prompt.to_owned()),
                    command_mode: None,
                    source_uuid: None,
                }],
            },
            session_id: String::new(),
            parent_tool_use_id: None,
            uuid: None,
            tool_use_result: None,
        }
    }

    fn historical_user_text(text: &str) -> Message {
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

    fn user_bubble_texts(app: &App) -> Vec<String> {
        app.messages()
            .iter()
            .filter(|m| matches!(m.role, MessageRole::User))
            .flat_map(|m| m.blocks.iter())
            .filter_map(|b| match b {
                MessageBlock::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .collect()
    }

    /// The walk suspends render-cache accounting and rebuilds it once
    /// at the end, so the accounting it leaves behind has to be
    /// indistinguishable from one maintained per message. Rebuilding a
    /// second time must therefore be a no-op on every total the
    /// eviction path reads.
    #[test]
    fn replay_leaves_render_cache_accounting_in_rebuilt_state() {
        let mut app = App::test_default();
        let history: Vec<Message> = (0..24)
            .map(|i| {
                if i % 3 == 0 {
                    historical_user_text(&format!("prompt {i}"))
                } else {
                    historical_assistant(&format!("reply {i}"))
                }
            })
            .collect();

        load_resume_history(&mut app, &history);

        let slots = app.render_cache_slots().to_vec();
        let total = app.render_cache_total_bytes();
        let protected = app.render_cache_protected_bytes();
        let evictable = app.render_cache_evictable().cloned();
        assert_eq!(
            slots.len(),
            app.messages().len(),
            "a suspended walk must not leave the slot rows short of the message list",
        );

        app.rebuild_render_cache_accounting();

        assert_eq!(app.render_cache_slots(), slots.as_slice());
        assert_eq!(app.render_cache_total_bytes(), total);
        assert_eq!(app.render_cache_protected_bytes(), protected);
        assert_eq!(app.render_cache_evictable(), evictable.as_ref());
    }

    #[test]
    fn replay_walk_does_not_leave_lifecycle_on_running() {
        let mut app = App::test_default();
        // Model an Idle, freshly-Connected bucket - what
        // `apply_session_update_connected`'s background path produces
        // before kicking off `load_resume_history`.
        let key = app.active_session_key.clone().expect("active key");
        app.sessions.get_mut(&key).expect("bucket").lifecycle_state = SessionLifecycleState::Idle;

        // A modest replay tail - multiple assistant messages, as a
        // long-lived session would have. Replay must leave the bucket
        // at Idle, not stuck at Running, regardless of how many
        // assistant messages the history carried.
        let history = vec![
            historical_assistant("prior turn 1"),
            historical_assistant("prior turn 2"),
            historical_assistant("prior turn 3"),
        ];
        load_resume_history(&mut app, &history);

        let bucket = app.sessions.get(&key).expect("bucket");
        assert_eq!(
            bucket.lifecycle_state,
            SessionLifecycleState::Idle,
            "post-replay lifecycle must still be Idle - Running here is what \
             pinned the Projects pane spinner on after launchpad auto_start",
        );
        assert!(
            !app.replay_in_progress,
            "replay_in_progress must be cleared at end of load_resume_history",
        );
    }

    /// Three consecutive queued_command attachments (the classic
    /// mid-turn submit batch - claude CLI buffers them locally and
    /// flushes at the next user→model boundary so the JSONL stamps
    /// them with one millisecond timestamp) collapse into a single
    /// user bubble whose blocks are `[header, ▸p1, ▸p2, ▸p3]`. The
    /// per-message bubble cluster the live-vs-resume timing
    /// mismatch produces is what the previous render had.
    #[test]
    fn run_of_queued_commands_collapses_into_one_user_bubble() {
        let mut app = App::test_default();

        let history = vec![
            synthesized_queued("What is this launchpad auto stop?"),
            synthesized_queued("start*"),
            synthesized_queued("And why do you still carry focus equals to false?"),
        ];
        load_resume_history(&mut app, &history);

        // Find the queued group - the only User message in the
        // resulting chat that has 4 blocks (header + 3 items). The
        // welcome message lives above it as a Welcome-role entry.
        let group = app
            .messages()
            .iter()
            .find(|m| matches!(m.role, MessageRole::User) && m.blocks.len() == 4)
            .expect("queued group bubble");
        let header_text = match &group.blocks[0] {
            MessageBlock::Text(b) => b.text.clone(),
            _ => panic!("first block should be the header text"),
        };
        assert!(
            header_text.contains("Queued during the previous turn")
                && header_text.contains("3 messages"),
            "header should announce the count: {header_text:?}",
        );
        for (idx, expected) in [
            "▸ What is this launchpad auto stop?",
            "▸ start*",
            "▸ And why do you still carry focus equals to false?",
        ]
        .iter()
        .enumerate()
        {
            let block_idx = idx + 1; // skip header
            match &group.blocks[block_idx] {
                MessageBlock::Text(b) => assert_eq!(b.text, *expected),
                _ => panic!("block {block_idx} should be a Text block"),
            }
        }
    }

    /// A solo queued_command (no run) falls through to the regular
    /// single-bubble path so the live-rendering shape is preserved.
    /// Without this carve-out, every mid-turn submit would render
    /// as a "Queued during the previous turn · 1 messages" bubble
    /// which is overkill for the common case.
    #[test]
    fn single_queued_command_renders_as_regular_user_bubble() {
        let mut app = App::test_default();
        let history = vec![synthesized_queued("solo mid-turn message")];
        load_resume_history(&mut app, &history);

        let user_msgs: Vec<&_> =
            app.messages().iter().filter(|m| matches!(m.role, MessageRole::User)).collect();
        assert_eq!(user_msgs.len(), 1, "exactly one user bubble for the solo queued message");
        assert_eq!(user_msgs[0].blocks.len(), 1, "no group header for a singleton");
        let MessageBlock::Text(text_block) = &user_msgs[0].blocks[0] else {
            panic!("solo queued bubble should be a Text block");
        };
        assert_eq!(text_block.text, "solo mid-turn message");
    }

    /// Task-notification queued_commands (harness plumbing, not
    /// user input) drop out of a group so the surviving prompts
    /// stay clean. With one survivor, the group collapses back to
    /// the singleton path.
    #[test]
    fn task_notifications_are_filtered_inside_a_run() {
        let mut app = App::test_default();
        let history = vec![
            synthesized_queued("<task-notification>\n<task-id>abc</task-id>\n</task-notification>"),
            synthesized_queued("real user prompt"),
            synthesized_queued("<task-notification>\n<task-id>def</task-id>\n</task-notification>"),
        ];
        load_resume_history(&mut app, &history);

        let user_msgs: Vec<&_> =
            app.messages().iter().filter(|m| matches!(m.role, MessageRole::User)).collect();
        assert_eq!(user_msgs.len(), 1, "two task-notifications drop, leaving one prompt");
        let MessageBlock::Text(text_block) = &user_msgs[0].blocks[0] else {
            panic!("should be a Text block");
        };
        assert_eq!(text_block.text, "real user prompt");
    }

    /// A historical `<task-notification>` user turn (the harness's
    /// background-task / subagent completion notice) must not render as
    /// a chat bubble on resume - it leaks the task id, output path, and
    /// status. The genuine user turn beside it still renders.
    #[test]
    fn resume_suppresses_task_notification_user_turns() {
        let mut app = App::test_default();
        let notification = "<task-notification>\n\
             <task-id>task-abc123</task-id>\n\
             <tool-use-id>toolu_xyz</tool-use-id>\n\
             <output-path>/tmp/forge/tasks/task-abc123.output</output-path>\n\
             <status>completed</status>\n\
             </task-notification>";
        let history = vec![
            historical_user_text("do the thing"),
            historical_user_text(notification),
            historical_assistant("done"),
        ];
        load_resume_history(&mut app, &history);

        let texts = user_bubble_texts(&app);
        assert!(
            texts.iter().any(|t| t.contains("do the thing")),
            "the genuine user turn must still render: {texts:?}",
        );
        assert!(
            !texts.iter().any(|t| t.contains("task-notification")),
            "task-notification scaffolding must not render: {texts:?}",
        );
        assert!(
            !texts.iter().any(|t| t.contains("task-abc123")),
            "task id must not leak into chat: {texts:?}",
        );
        assert!(
            !texts.iter().any(|t| t.contains("/tmp/forge/tasks")),
            "output path must not leak into chat: {texts:?}",
        );
    }

    /// A historical `<command-message>` slash-command wrapper is
    /// likewise suppressed on resume.
    #[test]
    fn resume_suppresses_command_message_wrapper() {
        let mut app = App::test_default();
        let history = vec![
            historical_user_text("<command-message>compact</command-message>"),
            historical_assistant("ack"),
        ];
        load_resume_history(&mut app, &history);

        let texts = user_bubble_texts(&app);
        assert!(
            !texts.iter().any(|t| t.contains("command-message")),
            "command-message wrapper must not render: {texts:?}",
        );
        assert!(
            app.messages().iter().all(|m| !matches!(m.role, MessageRole::User)),
            "no user bubble should survive the lone wrapper turn",
        );
    }

    /// The three wrappers already filtered before this predicate
    /// existed must keep dropping on resume - the shared predicate
    /// cannot regress them.
    #[test]
    fn resume_still_suppresses_local_command_wrappers() {
        let mut app = App::test_default();
        let history = vec![
            historical_user_text("<local-command-caveat>caveat text</local-command-caveat>"),
            historical_assistant("a1"),
            historical_user_text("<command-name>/clear</command-name>"),
            historical_assistant("a2"),
            historical_user_text("<local-command-stdout>stdout text</local-command-stdout>"),
            historical_assistant("a3"),
        ];
        load_resume_history(&mut app, &history);

        let texts = user_bubble_texts(&app);
        assert!(
            !texts.iter().any(|t| t.contains("local-command-caveat")),
            "local-command-caveat must stay suppressed: {texts:?}",
        );
        assert!(
            !texts.iter().any(|t| t.contains("command-name")),
            "command-name must stay suppressed: {texts:?}",
        );
        assert!(
            !texts.iter().any(|t| t.contains("local-command-stdout")),
            "local-command-stdout must stay suppressed: {texts:?}",
        );
        assert!(
            app.messages().iter().all(|m| !matches!(m.role, MessageRole::User)),
            "every user turn was a wrapper; none should render",
        );
    }

    /// The predicate anchors on `trim_start().starts_with(...)`, so a
    /// genuine user message that merely mentions a wrapper mid-sentence
    /// still renders.
    #[test]
    fn resume_renders_user_text_that_merely_mentions_wrappers() {
        let mut app = App::test_default();
        let history = vec![
            historical_user_text("please explain the <task-notification> tag to me"),
            historical_assistant("sure"),
            historical_user_text("what does <command-name> mean mid-sentence"),
            historical_assistant("it means a slash-command"),
        ];
        load_resume_history(&mut app, &history);

        let texts = user_bubble_texts(&app);
        assert!(
            texts.iter().any(|t| t.contains("please explain the <task-notification> tag")),
            "a message mentioning the tag mid-text must render: {texts:?}",
        );
        assert!(
            texts.iter().any(|t| t.contains("what does <command-name> mean")),
            "a message mentioning the wrapper mid-text must render: {texts:?}",
        );
    }

    // ---------------------------------------------------------------
    // A resumed inbound envelope must render exactly as its live
    // delivery did: one stamped card, never a plain user bubble
    // beside it.
    // ---------------------------------------------------------------

    fn peer_envelope_text(id: &str) -> String {
        format!("[Message id={id} from agent 'forge' (org 'Personal')]\n\ninbound body")
    }

    #[test]
    fn resumed_peer_envelope_renders_as_one_stamped_card() {
        let mut app = App::test_default();
        load_resume_history(&mut app, &[historical_user_text(&peer_envelope_text("t-1"))]);

        let user_msgs: Vec<&_> =
            app.messages().iter().filter(|m| matches!(m.role, MessageRole::User)).collect();
        assert_eq!(user_msgs.len(), 1, "the resume loop must not pre-paint the envelope text");
        assert!(
            user_msgs[0].is_peer_envelope,
            "the one user message is the stamped card, not a plain bubble",
        );
    }

    #[test]
    fn resumed_adjacent_envelopes_form_one_stamped_streak() {
        let mut app = App::test_default();
        load_resume_history(
            &mut app,
            &[
                historical_user_text(&peer_envelope_text("t-1")),
                historical_user_text(&peer_envelope_text("t-2")),
            ],
        );

        let user_msgs: Vec<&_> =
            app.messages().iter().filter(|m| matches!(m.role, MessageRole::User)).collect();
        assert_eq!(user_msgs.len(), 1, "adjacent envelopes merge into one stamped streak");
        assert!(user_msgs[0].is_peer_envelope);
        assert_eq!(user_msgs[0].blocks.len(), 2, "one block per delivered envelope");
    }

    /// Two adjacent plain user envelopes are two distinct turns: the
    /// chunk merge glues the Text blocks of one envelope, never a
    /// chunk from the next one, or the second turn vanishes as its
    /// own bubble.
    #[test]
    fn adjacent_plain_user_turns_stay_distinct_bubbles() {
        let mut app = App::test_default();
        load_resume_history(
            &mut app,
            &[historical_user_text("first prompt"), historical_user_text("second prompt")],
        );

        let user_msgs: Vec<&_> =
            app.messages().iter().filter(|m| matches!(m.role, MessageRole::User)).collect();
        assert_eq!(user_msgs.len(), 2, "each plain user envelope is its own bubble");
        let texts: Vec<&str> = user_msgs
            .iter()
            .filter_map(|m| match &m.blocks[0] {
                MessageBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["first prompt", "second prompt"], "no text crosses envelopes");
    }

    /// A queued-command row and the plain turn after it are two
    /// distinct turns: the echo bubble and the replayed text must not
    /// glue into one bubble.
    #[test]
    fn queued_command_then_plain_turn_stay_distinct_bubbles() {
        let mut app = App::test_default();
        load_resume_history(
            &mut app,
            &[synthesized_queued("mid-turn queue"), historical_user_text("the next prompt")],
        );

        let user_msgs: Vec<&_> =
            app.messages().iter().filter(|m| matches!(m.role, MessageRole::User)).collect();
        assert_eq!(user_msgs.len(), 2, "queued echo and the next turn are two bubbles");
        let texts: Vec<&str> = user_msgs
            .iter()
            .filter_map(|m| match &m.blocks[0] {
                MessageBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["mid-turn queue", "the next prompt"]);
    }

    /// An envelope entry followed by a separate plain user turn must
    /// stay two messages: the chunk merge pushes into the stamped
    /// card's text block, whose prefix the renderer still detects as
    /// an envelope, so the merged plain turn would paint inside the
    /// card body and vanish as its own turn.
    #[test]
    fn resumed_plain_turn_after_an_envelope_stays_a_distinct_message() {
        let mut app = App::test_default();
        load_resume_history(
            &mut app,
            &[
                historical_user_text(&peer_envelope_text("t-1")),
                historical_user_text("retype after the interruption"),
            ],
        );

        let user_msgs: Vec<&_> =
            app.messages().iter().filter(|m| matches!(m.role, MessageRole::User)).collect();
        assert_eq!(user_msgs.len(), 2, "envelope card and plain turn stay distinct messages");
        assert!(user_msgs[0].is_peer_envelope, "the envelope entry is the stamped card");
        let MessageBlock::Text(plain) = &user_msgs[1].blocks[0] else {
            panic!("the plain turn is a text block");
        };
        assert_eq!(plain.text, "retype after the interruption", "the retype keeps its own text");
    }

    #[test]
    fn resumed_gotify_notification_keeps_its_source_label() {
        let mut app = App::test_default();
        let notification =
            "[Gotify - app 'Backups', priority 3]\nNightly backup complete\nAll volumes backed up";
        load_resume_history(&mut app, &[historical_user_text(notification)]);

        let user_msgs: Vec<&_> =
            app.messages().iter().filter(|m| matches!(m.role, MessageRole::User)).collect();
        assert_eq!(user_msgs.len(), 1, "one card for the notification");
        assert!(user_msgs[0].is_gotify_envelope, "gotify stamps its own kind");
        assert!(!user_msgs[0].is_peer_envelope, "and it is not peer traffic");
    }

    #[test]
    fn resumed_queued_envelope_prompt_renders_as_envelope_card() {
        let mut app = App::test_default();
        let history = vec![synthesized_queued(
            "[Question id=q-1 from agent 'forge' (org 'Personal')]\n\nwhat gives?",
        )];
        load_resume_history(&mut app, &history);

        let user_msgs: Vec<&_> =
            app.messages().iter().filter(|m| matches!(m.role, MessageRole::User)).collect();
        assert_eq!(user_msgs.len(), 1, "the echo must not push a second bubble");
        assert!(user_msgs[0].is_peer_envelope, "a queued envelope prompt renders as a card");
    }

    #[test]
    fn resumed_queued_run_renders_envelope_prompts_as_cards() {
        let mut app = App::test_default();
        let history = vec![
            synthesized_queued("plain prompt one"),
            synthesized_queued(&peer_envelope_text("t-9")),
            synthesized_queued("plain prompt two"),
        ];
        load_resume_history(&mut app, &history);

        let user_msgs: Vec<&_> =
            app.messages().iter().filter(|m| matches!(m.role, MessageRole::User)).collect();
        assert_eq!(
            user_msgs.iter().filter(|m| m.is_peer_envelope).count(),
            1,
            "exactly one stamped card for the envelope prompt",
        );
        let group = user_msgs
            .iter()
            .find(|m| m.blocks.len() == 3)
            .expect("the two surviving plain prompts collapse into a group");
        let MessageBlock::Text(header_block) = &group.blocks[0] else {
            panic!("first block should be the header text");
        };
        assert!(
            header_block.text.contains("2 messages"),
            "only the survivors count: {:?}",
            header_block.text,
        );
        let blocks_text: Vec<&str> = group
            .blocks
            .iter()
            .filter_map(|b| match b {
                MessageBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            !blocks_text.iter().any(|t| t.contains("Message id=")),
            "the envelope prompt must not land in the group bubble: {blocks_text:?}",
        );
    }

    /// The survivor split can shrink a run below the group threshold:
    /// an envelope prompt leaves the run as a stamped card and a lone
    /// plain survivor must take the singleton echo, never a
    /// "1 messages" group header.
    #[test]
    fn resumed_queued_run_shrunk_to_one_survivor_takes_the_singleton_echo() {
        let mut app = App::test_default();
        let history = vec![
            synthesized_queued(&peer_envelope_text("t-5")),
            synthesized_queued("plain prompt"),
        ];
        load_resume_history(&mut app, &history);

        let user_msgs: Vec<&_> =
            app.messages().iter().filter(|m| matches!(m.role, MessageRole::User)).collect();
        assert_eq!(user_msgs.len(), 2, "the stamped card and the plain bubble both render");
        assert_eq!(
            user_msgs.iter().filter(|m| m.is_peer_envelope).count(),
            1,
            "the envelope prompt renders as its stamped card",
        );
        let texts: Vec<&str> = user_msgs
            .iter()
            .flat_map(|m| m.blocks.iter())
            .filter_map(|b| match b {
                MessageBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            !texts.iter().any(|t| t.contains("Queued during the previous turn")),
            "a 1-survivor run must not render a group header: {texts:?}",
        );
    }

    // ---------------------------------------------------------------
    // resume-replay drains all-terminal MONITORS +
    // WORKFLOWS sections post-replay (mirrors the live wire path's
    // `clear_*_if_all_terminal` calls). Mixed-state sections stay
    // visible with the in-flight entries.
    // ---------------------------------------------------------------

    use crate::app::state::types::{MonitorEntry, MonitorStatus, WorkflowEntry, WorkflowStatus};

    fn stub_monitor(id: &str, status: MonitorStatus) -> MonitorEntry {
        MonitorEntry {
            tool_use_id: id.to_owned(),
            task_id: Some(format!("task_{id}")),
            description: format!("desc_{id}"),
            command: "tail -F app.log".to_owned(),
            persistent: false,
            timeout_ms: 0,
            status,
            output_file: None,
            output_tail: std::collections::VecDeque::new(),
            expanded_in_inspector: false,
        }
    }

    fn stub_workflow(id: &str, status: WorkflowStatus) -> WorkflowEntry {
        WorkflowEntry {
            tool_use_id: id.to_owned(),
            task_id: Some(format!("task_{id}")),
            meta_name: format!("wf_{id}"),
            meta_description: None,
            phases: Vec::new(),
            status,
            final_result_summary: None,
            expanded_in_inspector: false,
        }
    }

    #[test]
    fn resume_replay_clears_all_terminal_monitors() {
        let mut app = App::test_default();
        // Seed the active bucket with replay-orphan monitors (the
        // shape #277 Bug 3b's resume-marking loop produces).
        *app.monitors_mut() = vec![
            stub_monitor("a", MonitorStatus::Stopped),
            stub_monitor("b", MonitorStatus::Completed),
        ];
        assert_eq!(app.monitors().len(), 2);

        load_resume_history(&mut app, &[]);

        assert!(
            app.monitors().is_empty(),
            "all-terminal MONITORS must drain post-replay (mirrors the live wire \
             clear_monitors_if_all_terminal path)",
        );
    }

    #[test]
    fn resume_replay_clears_all_terminal_workflows() {
        let mut app = App::test_default();
        *app.workflows_mut() = vec![
            stub_workflow("a", WorkflowStatus::Completed),
            stub_workflow("b", WorkflowStatus::Completed),
        ];
        assert_eq!(app.workflows().len(), 2);

        load_resume_history(&mut app, &[]);

        assert!(app.workflows().is_empty(), "all-terminal WORKFLOWS must drain post-replay");
    }

    #[test]
    fn resume_replay_keeps_monitors_section_when_some_still_running() {
        let mut app = App::test_default();
        *app.monitors_mut() = vec![
            stub_monitor("running", MonitorStatus::Running),
            stub_monitor("stopped", MonitorStatus::Stopped),
        ];

        load_resume_history(&mut app, &[]);

        assert_eq!(
            app.monitors().len(),
            2,
            "mixed-state MONITORS must survive: clear_if_all_terminal only fires when \
             every entry is terminal",
        );
    }

    #[test]
    fn resume_replay_keeps_workflows_section_when_some_still_in_progress() {
        let mut app = App::test_default();
        *app.workflows_mut() = vec![
            stub_workflow("in_progress", WorkflowStatus::InProgress),
            stub_workflow("done", WorkflowStatus::Completed),
        ];

        load_resume_history(&mut app, &[]);

        assert_eq!(app.workflows().len(), 2, "mixed-state WORKFLOWS must survive");
    }

    /// The tests above seed the entries directly, so none of them
    /// exercises the walk that actually builds one. A historical
    /// `Workflow` tool_use reaches `upsert_workflow_from_tool_input`,
    /// and no terminal event can follow it - the resume walk is fed by
    /// `synthesize_replay_messages`, which emits only User / Assistant
    /// envelopes, so no `TaskUpdated` / `TaskProgress` exists during
    /// replay. Seeded in-progress it would linger forever AND hold the
    /// section open for its completed siblings.
    #[test]
    fn resume_replay_drains_a_replayed_workflow_and_its_completed_sibling() {
        let mut app = App::test_default();
        *app.workflows_mut() = vec![stub_workflow("sibling", WorkflowStatus::Completed)];
        let history = vec![historical_tool_use_named(
            "toolu_wf",
            "Workflow",
            serde_json::json!({"script": "export const meta = { name: 'nightly-sweep' }"}),
        )];

        load_resume_history(&mut app, &history);

        assert!(
            app.workflows().is_empty(),
            "a replayed Workflow must restore terminal so the WORKFLOWS section drains \
             instead of showing it as in progress forever and blocking the clear for its \
             completed siblings; got: {:?}",
            app.workflows().iter().map(|w| (&w.meta_name, w.status)).collect::<Vec<_>>(),
        );

        // Drained is also what an entry that was never BUILT looks like,
        // so hold the sibling non-terminal and check the replayed entry
        // is really there and really terminal.
        let mut app = App::test_default();
        *app.workflows_mut() = vec![stub_workflow("sibling", WorkflowStatus::InProgress)];
        load_resume_history(&mut app, &history);

        assert_eq!(
            app.workflows().iter().find(|w| w.meta_name == "nightly-sweep").map(|w| w.status),
            Some(WorkflowStatus::Completed),
            "the walk builds the replayed entry and seeds it terminal; got: {:?}",
            app.workflows().iter().map(|w| (&w.meta_name, w.status)).collect::<Vec<_>>(),
        );
    }

    /// Starting a fresh session must drop the previous session's
    /// background-task registry - it is session-scoped and the incoming
    /// session has its own live set.
    #[test]
    fn reset_for_new_session_clears_background_registry() {
        use super::reset_for_new_session;
        use crate::agent::model;

        let mut app = App::test_default();
        let key = app.active_session_key.clone().expect("active key");
        {
            let bucket = app.sessions.get_mut(&key).expect("active bucket");
            bucket.background_tasks.push(crate::app::BackgroundTask {
                task_id: "t1".to_owned(),
                task_type: "local_bash".to_owned(),
                description: "gh run watch".to_owned(),
            });
            bucket.session_task_tool_use_ids.insert("t1".to_owned(), "tc-1".to_owned());
        }

        reset_for_new_session(
            &mut app,
            model::SessionId::new("reset-target"),
            model::CurrentModel::new("m", "m", "m"),
            None,
            false,
        );

        let active = app.active_session_key.clone().expect("active key after reset");
        let bucket = app.sessions.get(&active).expect("active bucket after reset");
        assert!(!bucket.has_live_background_work(), "reset drops the background-task registry");
        assert!(bucket.session_task_tool_use_ids.is_empty(), "task-id mirror cleared on reset too");
    }
}
