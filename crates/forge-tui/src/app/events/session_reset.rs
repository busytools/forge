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
    if let Some(terminals) = app.terminals() {
        crate::agent::events::kill_all_terminals(terminals);
    }

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
    app.set_fast_mode_state(model::FastModeState::Off);
    app.set_runtime_session_state(None);
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
    app.clear_terminal_tool_call_tracking();
    *app.mcp_mut() = super::super::McpState::default();
    crate::app::usage::reset_for_session_change(app);
    crate::app::plugins::reset_for_session_change(app);
    app.force_redraw = true;
    app.needs_redraw = true;
}

fn append_resume_user_message_chunk(app: &mut App, chunk: &model::ContentChunk) {
    let model::ContentBlock::Text(text) = &chunk.content else {
        return;
    };
    if text.text.is_empty() {
        return;
    }

    if let Some(last) = app.active_messages_mut().last_mut()
        && matches!(last.role, MessageRole::User)
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
        app.sync_after_message_blocks_changed(last_idx);
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
        None,
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
    let header = format!("Queued during the previous turn · {} messages", prompts.len());
    let mut blocks: Vec<MessageBlock> = Vec::with_capacity(prompts.len() + 1);
    blocks.push(MessageBlock::Text(TextBlock::from_complete(&header)));
    for prompt in prompts {
        let body = format!("▸ {prompt}");
        blocks.push(MessageBlock::Text(TextBlock::from_complete(&body)));
    }
    app.push_message_tracked(ChatMessage::new(MessageRole::User, blocks, None));
    app.enforce_history_retention_tracked();
    tracing::debug!(
        target: crate::logging::targets::APP_INPUT,
        event_name = "queued_group_replayed",
        message = "pushed grouped user bubble for replayed queued_command run",
        outcome = "success",
        message_count = prompts.len(),
    );
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
                .filter(|p| !p.trim_start().starts_with("<task-notification>"))
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
                    // Drop Claude Code's local-command scaffolding -
                    // `<local-command-caveat>…</local-command-caveat>`,
                    // `<command-name>/x</command-name>…`, and the
                    // matching `<local-command-stdout>` wrappers. These
                    // are metadata the LLM uses to distinguish "this is
                    // a slash-command invocation, not user input"; they
                    // were never meant for chat-buffer rendering. The
                    // live-session input handler never produces them
                    // (slash commands take a different path), so replay
                    // is the only surface that surfaces them.
                    let trimmed = text.trim_start();
                    if trimmed.starts_with("<local-command-caveat>")
                        || trimmed.starts_with("<command-name>")
                        || trimmed.starts_with("<local-command-stdout>")
                    {
                        continue;
                    }
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
                    let chunk = model::ContentChunk::new(model::ContentBlock::Text(
                        model::TextContent::new(text.clone()),
                    ));
                    append_resume_user_message_chunk(app, &chunk);
                }
            }
        }
        super::sdk_message::handle_sdk_message(app, msg.clone());
        i += 1;
    }
    app.replay_in_progress = false;
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
    //! `Running`. The unit-level gate lives in `events/sdk_message.rs`;
    //! this test pins the integration behaviour.
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
}
