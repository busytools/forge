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
    app.set_pending_cancel_origin(None);
    app.set_pending_auto_submit_after_cancel(false);
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
    app.pending_interaction_ids_mut().clear();
    app.clear_tool_scope_tracking();
    app.active_tool_call_index_mut().clear();
    app.todos_mut().clear();
    app.set_todo_verification_nudge(false);
    app.focus = super::super::FocusManager::default();
    app.available_commands_mut().clear();
    app.available_agents_mut().clear();
    app.config.overlay = None;
    app.config.pending_session_title_change = None;
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
        })],
        None,
    ));
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
    for msg in history_messages {
        // The raw walker (`handle_sdk_message`) processes user
        // messages by walking tool_results only — live wire user
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
            // assistant pointer when we're about to actually render — an
            // empty Text block isn't a render and shouldn't move the
            // pointer.
            let mut rendered_user_text = false;
            for block in &envelope.content {
                if let forge_primitives::ContentBlock::Text { text } = block {
                    if text.is_empty() {
                        continue;
                    }
                    // Drop Claude Code's local-command scaffolding —
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
    }
    app.finalize_turn_runtime_artifacts(model::ToolCallStatus::Failed);
    app.clear_active_turn_assistant();
    app.enforce_history_retention_tracked();
    *app.active_viewport_mut() = super::super::ChatViewport::new();
    app.active_viewport_mut().engage_auto_scroll();
}
