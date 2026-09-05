use super::super::{
    App, AppStatus, BlockCache, ChatMessage, InvalidationLevel, MessageBlock, MessageRole,
    ToolCallInfo, ToolCallScope,
};
use super::tool_updates::raw_output_to_terminal_text;
use crate::agent::model;

pub(super) fn handle_tool_call(app: &mut App, tc: model::RenderToolCall) {
    let id_str = tc.tool_call_id.clone();
    let sdk_tool_name = resolve_sdk_tool_name(tc.kind, tc.meta.as_ref());
    let parent_tool_use_id = parent_tool_use_id_from_meta(tc.meta.as_ref());
    let scope = register_tool_call_scope(app, &id_str, &sdk_tool_name, parent_tool_use_id);
    log_tool_call_received(app, &tc, &scope, &sdk_tool_name);
    update_subagent_scope_state(app, &scope, tc.status, &id_str);

    // Monitor tool_use → push a UiSession.monitors entry
    // (idempotent on tool_use_id). Parses the typed input via the
    // forge-agent parser so the description / command / persistent /
    // timeout_ms fields share one validation point. Malformed input
    // (description or command missing) silently no-ops; the standard
    // tool-card render path still surfaces the call.
    if sdk_tool_name == "Monitor"
        && let Some(input) = tc.raw_input.as_ref()
        && let Some(parsed) = forge_workspace::user_interaction::parse_monitor_input(input)
    {
        app.upsert_monitor_from_tool_input(
            &id_str,
            parsed.description,
            parsed.command,
            parsed.persistent,
            parsed.timeout_ms,
        );
    }

    // Workflow tool_use → push a UiSession.workflows
    // entry. `meta_name` / `meta_description` are extracted from
    // the script's `export const meta = {...}` block via the
    // substring parser; malformed scripts still get an entry with
    // the literal "Workflow" fallback so the Inspector row always
    // renders.
    if sdk_tool_name == "Workflow"
        && let Some(input) = tc.raw_input.as_ref()
        && let Some(parsed) = forge_workspace::user_interaction::parse_workflow_input(input)
    {
        let (meta_name, meta_description) = crate::ui::workflow_meta_fields(&parsed.script);
        app.upsert_workflow_from_tool_input(&id_str, meta_name, meta_description);
    }

    // ScheduleWakeup tool_use - one pending wakeup per session
    // (the /loop dynamic-pacing re-arm). fire_at = now + delaySeconds;
    // `reason` is the headline shown in the SCHEDULES section.
    if sdk_tool_name == "ScheduleWakeup"
        && let Some(input) = tc.raw_input.as_ref()
    {
        let delay = input.get("delaySeconds").and_then(serde_json::Value::as_u64).unwrap_or(0);
        let reason = input.get("reason").and_then(serde_json::Value::as_str).unwrap_or("");
        let fire_at = std::time::SystemTime::now() + std::time::Duration::from_secs(delay);
        app.upsert_wakeup_from_tool_input(&id_str, reason, fire_at);
    }

    // CronCreate tool_use - upsert a cron entry keyed by tool_use_id.
    // The CLI's CronCreate result carries the job id (stamped later
    // via `stamp_cron_id_from_result` in the tool_use_result handler).
    if sdk_tool_name == "CronCreate"
        && let Some(input) = tc.raw_input.as_ref()
    {
        let expr = input.get("cron").and_then(serde_json::Value::as_str).unwrap_or("");
        let prompt = input.get("prompt").and_then(serde_json::Value::as_str).unwrap_or("");
        let recurring = input.get("recurring").and_then(serde_json::Value::as_bool).unwrap_or(true);
        app.upsert_cron_from_tool_input(
            &id_str,
            expr,
            prompt,
            recurring,
            std::time::SystemTime::now(),
        );
    }

    // CronDelete tool_use - remove the matching cron entry by job id.
    // No-op when the job id is missing (malformed input).
    if sdk_tool_name == "CronDelete"
        && let Some(input) = tc.raw_input.as_ref()
        && let Some(job_id) = input.get("id").and_then(serde_json::Value::as_str)
    {
        app.remove_cron_by_id(job_id);
    }

    let tool_info = build_tool_info_from_tool_call(app, tc, sdk_tool_name, &scope);
    log_command_started(app, &tool_info);
    log_terminal_spawned(app, &tool_info, "initial");
    if should_jump_on_large_write(&tool_info) {
        app.active_viewport_mut().engage_auto_scroll();
    }
    // The root is the MAIN agent invoking Task, so it owns its turn like
    // any other call; only a child is another agent's work.
    let subagent_parent = match &scope {
        ToolCallScope::SubagentChild { parent_tool_use_id } => Some(parent_tool_use_id.clone()),
        ToolCallScope::SubagentRoot | ToolCallScope::MainAgent => None,
    };
    let subagent_scoped = subagent_parent.is_some();
    upsert_tool_call_into_assistant_message(app, tool_info, subagent_parent.as_deref());

    // A subagent's own call is not this session working, and
    // files-accessed is this turn's footer figure.
    if !subagent_scoped {
        app.status = AppStatus::Running;
        app.increment_files_accessed();
    }
}

fn log_tool_call_received(
    app: &App,
    tc: &model::RenderToolCall,
    scope: &ToolCallScope,
    sdk_tool_name: &str,
) {
    // INFO-only helper, so function entry IS the INFO arm.
    if super::skip_operational_log_during_replay(app) {
        return;
    }
    let session_id = current_session_id(app);
    tracing::info!(
        target: crate::logging::targets::APP_TOOL,
        event_name = "tool_call_received",
        message = "tool call received",
        outcome = "success",
        session_id = %session_id,
        tool_call_id = %tc.tool_call_id,
        count = tc.content.len(),
        size_bytes = json_value_size(tc.raw_input.as_ref()).unwrap_or_default(),
        tool_name = sdk_tool_name,
        tool_title = %tc.title,
        tool_kind = %tool_kind_name(tc.kind),
        tool_status = ?tc.status,
        tool_scope = %tool_scope_name(scope),
        content_block_count = tc.content.len(),
        location_count = tc.locations.len(),
        has_raw_output = tc.raw_output.is_some(),
        has_output_metadata = tc.output_metadata.is_some(),
    );
}

pub(super) fn register_tool_call_scope(
    app: &mut App,
    id: &str,
    sdk_tool_name: &str,
    parent_tool_use_id: Option<&str>,
) -> ToolCallScope {
    let scope = if let Some(parent_tool_use_id) =
        parent_tool_use_id.filter(|parent| !parent.trim().is_empty())
    {
        ToolCallScope::SubagentChild { parent_tool_use_id: parent_tool_use_id.to_owned() }
    } else if matches!(sdk_tool_name, "Task" | "Agent") {
        ToolCallScope::SubagentRoot
    } else {
        ToolCallScope::MainAgent
    };
    app.register_tool_call_scope(id.to_owned(), scope.clone());
    scope
}

pub(super) fn update_subagent_scope_state(
    app: &mut App,
    scope: &ToolCallScope,
    status: model::ToolCallStatus,
    id: &str,
) {
    match scope {
        ToolCallScope::SubagentChild { .. } | ToolCallScope::MainAgent => {}
        ToolCallScope::SubagentRoot => match status {
            model::ToolCallStatus::InProgress | model::ToolCallStatus::Pending => {
                app.insert_active_task(id.to_owned());
            }
            model::ToolCallStatus::Completed
            | model::ToolCallStatus::Failed
            | model::ToolCallStatus::Killed => {
                app.remove_active_task(id);
            }
        },
    }
}

fn build_tool_info_from_tool_call(
    app: &App,
    tc: model::RenderToolCall,
    sdk_tool_name: String,
    scope: &ToolCallScope,
) -> ToolCallInfo {
    let terminal_id = tc.content.iter().find_map(|content| match content {
        model::RenderToolCallContent::Terminal(term) => Some(term.terminal_id.clone()),
        _ => None,
    });
    let initial_execute_output = if super::super::is_execute_tool_name(&sdk_tool_name) {
        tc.raw_output.as_ref().and_then(raw_output_to_terminal_text)
    } else {
        None
    };

    // CLI 2.1.156 chat-suppressed tools (#268 + #273):
    // - Task* family (TaskCreate / TaskUpdate / TaskList / TaskGet) -
    //   Inspector TASKS section is the authoritative surface.
    // - TaskOutput / TaskStop - paired with Monitor / Workflow; their
    //   side-effects surface on those tools' own blocks.
    // - AskUserQuestion - dock-morph widget renders instead of a card.
    //
    // - Workflow - the Inspector WORKFLOWS section is the surface. A
    //   chat block was tried and reverted; keeping the Inspector as the
    //   only surface is the standing choice, not pending work.
    //
    // Monitor is NOT here: the lifecycle block in
    // `ui::message::render_lifecycle_one_liner` is its only surface,
    // and `append_assistant_tool_block` reaches that render only for a
    // visible tool call.
    let is_chat_suppressed = matches!(
        sdk_tool_name.as_str(),
        "TaskCreate"
            | "TaskUpdate"
            | "TaskList"
            | "TaskGet"
            | "TaskOutput"
            | "TaskStop"
            | "AskUserQuestion"
            | "Workflow"
            | "ScheduleWakeup"
            | "CronCreate"
            | "CronDelete",
    );
    let monitor_status = app.monitor_status_for_tool_use(&tc.tool_call_id);
    let mut tool_info = ToolCallInfo {
        id: tc.tool_call_id,
        title: shorten_tool_title(&tc.title, &app.cwd_raw()),
        sdk_tool_name,
        raw_input: tc.raw_input,
        raw_input_bytes: 0,
        output_metadata: tc.output_metadata,
        task_metadata: tc.task_metadata,
        status: tc.status,
        content: tc.content,
        // Subagent scopes are Inspector-only. The root + every child
        // tool call lives in the message list (so `App::subagents_view`
        // can walk both via the registered scope), but the chat render
        // skips them - a Task/Agent dispatch shows nothing in the chat
        // stream and surfaces only in the Inspector SUBAGENTS section.
        hidden: is_chat_suppressed
            || matches!(scope, ToolCallScope::SubagentRoot | ToolCallScope::SubagentChild { .. },),
        terminal_id,
        terminal_output: None,
        monitor_output_tail: Vec::default(),
        monitor_status,
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
    };
    tool_info.raw_input_bytes =
        tool_info.raw_input.as_ref().map_or(0, ToolCallInfo::estimate_json_value_bytes);
    if let Some(output) = initial_execute_output {
        tool_info.terminal_output = Some(output);
    }
    tool_info
}

pub(super) fn upsert_tool_call_into_assistant_message(
    app: &mut App,
    tool_info: ToolCallInfo,
    subagent_parent: Option<&str>,
) {
    let existing_pos = app.lookup_tool_call(&tool_info.id);

    if let Some((mi, bi)) = existing_pos {
        update_existing_tool_call(app, mi, bi, &tool_info);
        return;
    }

    // A subagent's card belongs with its root: that is where the
    // Inspector's parent-id correlation looks for it, and it is the only
    // placement independent of whatever sits at the tail.
    if let Some(parent) = subagent_parent
        && let Some((root_msg_idx, _)) = app.lookup_tool_call(parent)
        && let Some(owner) = app.active_messages_mut().get_mut(root_msg_idx)
    {
        let block_idx = owner.blocks.len();
        let tc_id = tool_info.id.clone();
        owner.blocks.push(MessageBlock::ToolCall(Box::new(tool_info)));
        app.sync_after_message_tail_changed(root_msg_idx);
        app.index_tool_call(tc_id, root_msg_idx, block_idx);
        return;
    }

    // Retention can evict the root: it drops from the front, and a
    // backgrounded root's card goes terminal as soon as the CLI
    // acknowledges the launch, so it is not retention-protected. Reuse
    // the last assistant rather than the tail - after one push a last
    // assistant always exists, so this stops pushing.
    if subagent_parent.is_some() {
        let target = app.messages().iter().rposition(|m| matches!(m.role, MessageRole::Assistant));
        let tc_id = tool_info.id.clone();
        let (msg_idx, block_idx) = if let Some(msg_idx) = target {
            let Some(owner) = app.active_messages_mut().get_mut(msg_idx) else {
                return;
            };
            let block_idx = owner.blocks.len();
            owner.blocks.push(MessageBlock::ToolCall(Box::new(tool_info)));
            app.sync_after_message_tail_changed(msg_idx);
            (msg_idx, block_idx)
        } else {
            let new_idx = app.messages().len();
            app.push_message_tracked(ChatMessage::new(
                MessageRole::Assistant,
                vec![MessageBlock::ToolCall(Box::new(tool_info))],
            ));
            (new_idx, 0)
        };
        app.index_tool_call(tc_id, msg_idx, block_idx);
        return;
    }

    if let Some(msg_idx) = app.active_turn_assistant_idx()
        && let Some(owner) = app.active_messages_mut().get_mut(msg_idx)
    {
        let block_idx = owner.blocks.len();
        let tc_id = tool_info.id.clone();
        owner.blocks.push(MessageBlock::ToolCall(Box::new(tool_info)));
        app.sync_after_message_tail_changed(msg_idx);
        app.index_tool_call(tc_id, msg_idx, block_idx);
        return;
    }

    // A main-agent call here is opening a new turn, so it may not land
    // on a message whose turn already ended; "unsettled" alone is not
    // that test, since a resumed or failed turn leaves a tail that
    // never Resulted.
    let append_to_tail = app.messages().last().is_some_and(|m| {
        matches!(m.role, MessageRole::Assistant)
            && !m.turn_info.is_settled()
            && !m.turn_info.is_empty()
    });

    if append_to_tail {
        let msg_idx = app.messages().len().saturating_sub(1);
        let Some(last) = app.active_messages_mut().last_mut() else {
            return;
        };
        let block_idx = last.blocks.len();
        let tc_id = tool_info.id.clone();
        last.blocks.push(MessageBlock::ToolCall(Box::new(tool_info)));
        app.bind_active_turn_assistant(msg_idx);
        app.sync_after_message_tail_changed(msg_idx);
        app.index_tool_call(tc_id, msg_idx, block_idx);
        return;
    }

    let tc_id = tool_info.id.clone();
    let new_idx = app.messages().len();
    app.push_message_tracked(ChatMessage::new(
        MessageRole::Assistant,
        vec![MessageBlock::ToolCall(Box::new(tool_info))],
    ));
    app.bind_active_turn_assistant(new_idx);
    app.index_tool_call(tc_id, new_idx, 0);
}

/// A tool's input is fixed at dispatch; a later update that carries no
/// input (`None`) or an empty object `{}` must not clobber it.
pub(super) fn raw_input_carries_content(v: &serde_json::Value) -> bool {
    !v.as_object().is_some_and(serde_json::Map::is_empty)
}

fn update_existing_tool_call(app: &mut App, mi: usize, bi: usize, tool_info: &ToolCallInfo) {
    let mut layout_dirty = false;
    if let Some(MessageBlock::ToolCall(existing)) =
        app.active_messages_mut().get_mut(mi).and_then(|m| m.blocks.get_mut(bi))
    {
        let existing = existing.as_mut();
        let mut changed = false;
        changed |= sync_if_changed(&mut existing.title, &tool_info.title);
        changed |= sync_if_changed(&mut existing.status, &tool_info.status);
        changed |= sync_if_changed(&mut existing.content, &tool_info.content);
        changed |= sync_if_changed(&mut existing.sdk_tool_name, &tool_info.sdk_tool_name);
        changed |= sync_if_changed(&mut existing.hidden, &tool_info.hidden);
        // Input is fixed at dispatch; a later input-less OR empty-`{}`
        // update must preserve it.
        if tool_info.raw_input.as_ref().is_some_and(raw_input_carries_content) {
            changed |= existing.set_raw_input(tool_info.raw_input.clone());
        }
        changed |= sync_if_changed(&mut existing.output_metadata, &tool_info.output_metadata);
        changed |= sync_if_changed(&mut existing.task_metadata, &tool_info.task_metadata);
        if tool_info.terminal_id.is_some() {
            changed |= sync_if_changed(&mut existing.terminal_id, &tool_info.terminal_id);
        }
        if tool_info.terminal_output.is_some() {
            changed |= sync_if_changed(&mut existing.terminal_output, &tool_info.terminal_output);
        }
        if changed {
            existing.mark_tool_call_layout_dirty();
            layout_dirty = true;
        } else {
            crate::perf::mark("tool_update_noop_skips");
        }
    }
    if layout_dirty {
        app.sync_render_cache_slot(mi, bi);
        app.recompute_message_retained_bytes(mi);
        app.invalidate_layout(InvalidationLevel::MessageChanged(mi));
    }
}

pub(super) fn sync_if_changed<T: PartialEq + Clone>(dst: &mut T, src: &T) -> bool {
    if dst == src {
        return false;
    }
    dst.clone_from(src);
    true
}

pub(super) fn sdk_tool_name_from_meta(meta: Option<&serde_json::Value>) -> Option<&str> {
    meta.and_then(|m| m.get("claudeCode")).and_then(|v| v.get("toolName")).and_then(|v| v.as_str())
}

pub(super) fn parent_tool_use_id_from_meta(meta: Option<&serde_json::Value>) -> Option<&str> {
    meta.and_then(|m| m.get("claudeCode"))
        .and_then(|v| v.get("parentToolUseId"))
        .and_then(|v| v.as_str())
        .filter(|value| !value.trim().is_empty())
}

fn fallback_sdk_tool_name(kind: model::ToolKind) -> &'static str {
    match kind {
        model::ToolKind::Read => "Read",
        model::ToolKind::Edit => "Edit",
        model::ToolKind::Delete => "Delete",
        model::ToolKind::Move => "Move",
        model::ToolKind::Search => "Search",
        model::ToolKind::Execute => "Bash",
        model::ToolKind::Think => "Think",
        model::ToolKind::Fetch => "Fetch",
        model::ToolKind::SwitchMode => "ExitPlanMode",
        model::ToolKind::Other => "Tool",
    }
}

pub(super) fn resolve_sdk_tool_name(
    kind: model::ToolKind,
    meta: Option<&serde_json::Value>,
) -> String {
    if let Some(name) = sdk_tool_name_from_meta(meta).filter(|name| !name.trim().is_empty()) {
        name.to_owned()
    } else {
        let fallback = fallback_sdk_tool_name(kind);
        if matches!(kind, model::ToolKind::Think) {
            tracing::warn!(
                target: crate::logging::targets::APP_TOOL,
                event_name = "tool_name_fallback_used",
                message = "tool name fallback used for tool call",
                outcome = "degraded",
                tool_kind = %tool_kind_name(kind),
                fallback_tool_name = fallback,
            );
        }
        fallback.to_owned()
    }
}

/// Shorten absolute paths in tool titles to relative paths based on cwd.
/// e.g. "Read C:\\Users\\me\\project\\src\\main.rs" -> "Read src/main.rs"
/// Handles both `/` and `\\` separators on all platforms since the bridge adapter
/// may use either regardless of the host OS.
pub(super) fn shorten_tool_title(title: &str, cwd_raw: &str) -> String {
    if cwd_raw.is_empty() {
        return title.to_owned();
    }

    // Quick check: if title doesn't contain any part of cwd, skip normalization
    // Use the first path component of cwd as a heuristic
    let cwd_start = cwd_raw.split(['/', '\\']).find(|s| !s.is_empty()).unwrap_or(cwd_raw);
    if !title.contains(cwd_start) {
        return title.to_owned();
    }

    // Normalize both to forward slashes for matching
    let cwd_norm = cwd_raw.replace('\\', "/");
    let title_norm = title.replace('\\', "/");

    // Ensure cwd ends with slash so we strip the separator too
    let with_sep = if cwd_norm.ends_with('/') { cwd_norm } else { format!("{cwd_norm}/") };

    if title_norm.contains(&with_sep) {
        return title_norm.replace(&with_sep, "");
    }
    title_norm
}

pub(super) const WRITE_DIFF_JUMP_THRESHOLD_LINES: usize = 40;

pub(super) fn should_jump_on_large_write(tc: &ToolCallInfo) -> bool {
    if tc.sdk_tool_name != "Write" {
        return false;
    }
    tc.content.iter().any(|c| match c {
        model::RenderToolCallContent::Diff(diff) => {
            let new_lines = diff.new_text.lines().count();
            let old_lines = diff.old_text.as_deref().map_or(0, |t| t.lines().count());
            new_lines.max(old_lines) >= WRITE_DIFF_JUMP_THRESHOLD_LINES
        }
        _ => false,
    })
}

/// Check if any tool call in the current assistant message is still in-progress.
pub(super) fn has_in_progress_tool_calls(app: &App) -> bool {
    if let Some(owner_idx) = app.active_turn_assistant_idx()
        && let Some(owner) = app.messages().get(owner_idx)
    {
        return owner.blocks.iter().any(|block| {
            matches!(
                block,
                MessageBlock::ToolCall(tc)
                    if matches!(tc.status, model::ToolCallStatus::InProgress | model::ToolCallStatus::Pending)
            )
        });
    }
    false
}

pub(super) fn log_command_started(app: &App, tc: &ToolCallInfo) {
    if !tc.is_execute_tool() {
        return;
    }

    match tc.status {
        model::ToolCallStatus::Pending | model::ToolCallStatus::InProgress => {
            if super::skip_operational_log_during_replay(app) {
                return;
            }
            tracing::info!(
                target: crate::logging::targets::APP_COMMAND,
                event_name = "command_started",
                message = "command execution started",
                outcome = "start",
                session_id = %current_session_id(app),
                tool_call_id = %tc.id,
                terminal_id = %tc.terminal_id.as_deref().unwrap_or(""),
                size_bytes = u64::try_from(tc.raw_input_bytes).unwrap_or_default(),
                tool_name = %tc.sdk_tool_name,
                tool_status = ?tc.status,
                has_terminal = tc.terminal_id.is_some(),
                terminal_output_bytes =
                    u64::try_from(tc.terminal_output.as_deref().map_or(0, str::len))
                        .unwrap_or_default(),
                assistant_auto_backgrounded = tc.assistant_auto_backgrounded(),
            );
        }
        model::ToolCallStatus::Completed => tracing::info!(
            target: crate::logging::targets::APP_COMMAND,
            event_name = "command_completed",
            message = "command execution completed",
            outcome = "success",
            session_id = %current_session_id(app),
            tool_call_id = %tc.id,
            terminal_id = %tc.terminal_id.as_deref().unwrap_or(""),
            size_bytes = u64::try_from(tc.raw_input_bytes).unwrap_or_default(),
            tool_name = %tc.sdk_tool_name,
            tool_status = ?tc.status,
            has_terminal = tc.terminal_id.is_some(),
            terminal_output_bytes = u64::try_from(tc.terminal_output.as_deref().map_or(0, str::len))
                .unwrap_or_default(),
            assistant_auto_backgrounded = tc.assistant_auto_backgrounded(),
        ),
        model::ToolCallStatus::Failed | model::ToolCallStatus::Killed => tracing::warn!(
            target: crate::logging::targets::APP_COMMAND,
            event_name = if matches!(tc.status, model::ToolCallStatus::Killed) {
                "command_killed"
            } else {
                "command_failed"
            },
            message = if matches!(tc.status, model::ToolCallStatus::Killed) {
                "command execution killed"
            } else {
                "command execution failed"
            },
            outcome = "failure",
            session_id = %current_session_id(app),
            tool_call_id = %tc.id,
            terminal_id = %tc.terminal_id.as_deref().unwrap_or(""),
            size_bytes = u64::try_from(tc.raw_input_bytes).unwrap_or_default(),
            tool_name = %tc.sdk_tool_name,
            tool_status = ?tc.status,
            error_kind = "command_error",
            has_terminal = tc.terminal_id.is_some(),
            terminal_output_bytes = u64::try_from(tc.terminal_output.as_deref().map_or(0, str::len))
                .unwrap_or_default(),
            assistant_auto_backgrounded = tc.assistant_auto_backgrounded(),
        ),
    }
}

pub(super) fn log_terminal_spawned(app: &App, tc: &ToolCallInfo, source: &str) {
    if !tc.is_execute_tool() || tc.terminal_id.is_none() {
        return;
    }

    tracing::info!(
        target: crate::logging::targets::APP_COMMAND,
        event_name = "terminal_spawned",
        message = "terminal attached to command execution",
        outcome = "success",
        session_id = %current_session_id(app),
        tool_call_id = %tc.id,
        terminal_id = %tc.terminal_id.as_deref().unwrap_or(""),
        tool_name = %tc.sdk_tool_name,
        spawn_source = source,
        assistant_auto_backgrounded = tc.assistant_auto_backgrounded(),
    );
}

pub(super) fn current_session_id(app: &App) -> String {
    app.session_id().map_or_else(String::new, |s| s.to_string())
}

pub(super) fn json_value_size(value: Option<&serde_json::Value>) -> Option<u64> {
    value
        .and_then(|value| serde_json::to_vec(value).ok())
        .and_then(|bytes| u64::try_from(bytes.len()).ok())
}

pub(super) fn tool_scope_name(scope: &ToolCallScope) -> &'static str {
    match scope {
        ToolCallScope::SubagentRoot => "subagent_root",
        ToolCallScope::MainAgent => "main_agent",
        ToolCallScope::SubagentChild { .. } => "subagent_child",
    }
}

pub(super) fn tool_kind_name(kind: model::ToolKind) -> &'static str {
    match kind {
        model::ToolKind::Read => "read",
        model::ToolKind::Edit => "edit",
        model::ToolKind::Delete => "delete",
        model::ToolKind::Move => "move",
        model::ToolKind::Search => "search",
        model::ToolKind::Execute => "execute",
        model::ToolKind::Think => "think",
        model::ToolKind::Fetch => "fetch",
        model::ToolKind::SwitchMode => "switch_mode",
        model::ToolKind::Other => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::super::TextBlock;
    use super::*;

    fn subagent_root(id: &str, raw_input: Option<serde_json::Value>) -> ToolCallInfo {
        ToolCallInfo {
            id: id.to_owned(),
            title: "Task".to_owned(),
            sdk_tool_name: "Task".to_owned(),
            raw_input,
            raw_input_bytes: 0,
            output_metadata: None,
            task_metadata: None,
            status: model::ToolCallStatus::InProgress,
            content: Vec::new(),
            hidden: false,
            terminal_id: None,
            terminal_output: None,
            monitor_output_tail: Vec::new(),
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

    // A subagent root's descriptive raw_input feeds its SUBAGENTS label; a
    // later input-less wire update must preserve it, not collapse to "Task".
    #[test]
    fn subagent_root_raw_input_survives_later_input_less_update() {
        let mut app = App::test_default();
        let id = "toolu_subagent_root";
        let descriptive = serde_json::json!({
            "subagent_type": "Explore",
            "description": "map hidden tool calls",
        });

        app.register_tool_call_scope(id.to_owned(), ToolCallScope::SubagentRoot);
        upsert_tool_call_into_assistant_message(
            &mut app,
            subagent_root(id, Some(descriptive.clone())),
            None,
        );
        assert_eq!(
            app.subagents_view()[0].label,
            "Explore · map hidden tool calls",
            "label is descriptive after the initial dispatch",
        );

        // The CLI's follow-up update refines status only and carries no input.
        upsert_tool_call_into_assistant_message(&mut app, subagent_root(id, None), None);

        let (mi, bi) = app.lookup_tool_call(id).expect("root stays indexed");
        let MessageBlock::ToolCall(root) = &app.messages()[mi].blocks[bi] else {
            panic!("expected ToolCall block");
        };
        assert_eq!(
            root.raw_input.as_ref(),
            Some(&descriptive),
            "input-less update preserves the dispatch raw_input",
        );
        assert_eq!(
            app.subagents_view()[0].label,
            "Explore · map hidden tool calls",
            "label stays descriptive, never collapses to the bare tool name",
        );
    }

    // An empty-object `{}` update is `is_some()` yet carries no content;
    // it must preserve the dispatch input just like an input-less one.
    #[test]
    fn subagent_root_raw_input_survives_later_empty_object_update() {
        let mut app = App::test_default();
        let id = "toolu_subagent_root_empty";
        let descriptive = serde_json::json!({
            "subagent_type": "Explore",
            "description": "map hidden tool calls",
        });

        app.register_tool_call_scope(id.to_owned(), ToolCallScope::SubagentRoot);
        upsert_tool_call_into_assistant_message(
            &mut app,
            subagent_root(id, Some(descriptive.clone())),
            None,
        );

        upsert_tool_call_into_assistant_message(
            &mut app,
            subagent_root(id, Some(serde_json::json!({}))),
            None,
        );

        let (mi, bi) = app.lookup_tool_call(id).expect("root stays indexed");
        let MessageBlock::ToolCall(root) = &app.messages()[mi].blocks[bi] else {
            panic!("expected ToolCall block");
        };
        assert_eq!(
            root.raw_input.as_ref(),
            Some(&descriptive),
            "empty-object update preserves the dispatch raw_input",
        );
        assert_eq!(app.subagents_view()[0].label, "Explore · map hidden tool calls");
    }

    fn render_assistant_block(block: MessageBlock) -> String {
        let mut msg = ChatMessage::new(MessageRole::Assistant, vec![block]);
        let spinner = crate::ui::message::SpinnerState {
            glyph: '\u{280B}',
            is_active_turn_assistant: false,
            show_empty_thinking: false,
            show_thinking: false,
            show_compacting: false,
            running_subagents: None,
            live_turn_running: false,
        };
        let mut lines = Vec::new();
        crate::ui::message::render_message(
            &mut msg,
            &spinner,
            crate::ui::message::MessageRenderContext::new(
                None,
                80,
                0,
                crate::ui::message::MessageRenderOptions {
                    tools_collapsed: true,
                    ..Default::default()
                },
            ),
            &mut lines,
        );
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Monitor taken through the production build path must reach the
    /// chat renderer. The lifecycle block is its only chat surface, and
    /// the Inspector MONITORS section is gone, so a `hidden` stamp
    /// erases it everywhere - `append_assistant_tool_block` returns
    /// before the render fires.
    #[test]
    fn monitor_reaches_the_chat_renderer() {
        let app = App::test_default();
        let tc =
            model::RenderToolCall::new("toolu_lifecycle", "Monitor").raw_input(serde_json::json!({
                "description": "ci-watch",
                "command": "gh run watch 1",
                "persistent": true,
                "timeout_ms": 0,
            }));
        let info = build_tool_info_from_tool_call(
            &app,
            tc,
            "Monitor".to_owned(),
            &ToolCallScope::MainAgent,
        );
        assert!(!info.hidden, "Monitor must not be chat-suppressed");
        let rendered = render_assistant_block(MessageBlock::ToolCall(Box::new(info)));
        assert!(
            rendered.contains("ci-watch"),
            "Monitor lifecycle block missing from the chat render; got:\n{rendered}",
        );
    }

    /// Workflow is chat-suppressed: the Inspector WORKFLOWS section is
    /// its surface.
    #[test]
    fn workflow_stays_chat_suppressed() {
        let app = App::test_default();
        let tc = model::RenderToolCall::new("toolu_wf", "Workflow")
            .raw_input(serde_json::json!({"script": "export const meta = { name: 'x' }"}));
        let info = build_tool_info_from_tool_call(
            &app,
            tc,
            "Workflow".to_owned(),
            &ToolCallScope::MainAgent,
        );
        assert!(info.hidden, "Workflow renders in the Inspector, not the chat stream");
        let rendered = render_assistant_block(MessageBlock::ToolCall(Box::new(info)));
        assert!(
            !rendered.contains("Workflow"),
            "a suppressed Workflow paints no block of its own; got:\n{rendered}",
        );
    }

    // The guard is UPDATE-only: a genuinely no-arg tool still captures
    // its `{}` input at first sight (the creation path is unguarded).
    #[test]
    fn initial_empty_object_raw_input_is_captured_at_creation() {
        let app = App::test_default();
        let tc =
            model::RenderToolCall::new("toolu_no_arg", "NoArg").raw_input(serde_json::json!({}));
        let info =
            build_tool_info_from_tool_call(&app, tc, "NoArg".to_owned(), &ToolCallScope::MainAgent);
        assert_eq!(info.raw_input, Some(serde_json::json!({})));
    }

    fn assistant_tail(text: &str) -> ChatMessage {
        ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::Text(TextBlock::from_complete(text))],
        )
    }

    // Mirror of the streaming half's resumed-completed regression (events.rs):
    // replay persists no Result records, so a resumed-completed tail is
    // content-bearing with a default `turn_info`. A main-agent call opening
    // the next turn must open a fresh bubble past it, never glue into it.
    #[test]
    fn main_agent_call_pushes_fresh_for_resumed_completed_tail() {
        let mut app = App::test_default();
        app.active_messages_mut().push(ChatMessage::new(
            MessageRole::User,
            vec![MessageBlock::Text(TextBlock::from_complete("q1"))],
        ));
        app.active_messages_mut().push(assistant_tail("prior answer"));
        app.clear_active_turn_assistant();
        app.status = AppStatus::Running;
        let completed_idx = app.messages().len() - 1;

        upsert_tool_call_into_assistant_message(&mut app, subagent_root("toolu_next", None), None);

        assert_eq!(
            app.messages().len(),
            completed_idx + 2,
            "a fresh bubble opened past the historical one",
        );
        assert_eq!(
            app.messages()[completed_idx].blocks.len(),
            1,
            "the historical bubble must not receive the next turn's tool call",
        );
        assert_eq!(
            app.active_turn_assistant_idx(),
            Some(completed_idx + 1),
            "the pointer binds to the fresh bubble",
        );
    }

    // The #783 case the append branch exists for: a live turn's tail whose
    // pointer was lost keeps receiving. `started_at` is what separates it
    // from a resumed or failed tail.
    #[test]
    fn main_agent_call_still_appends_to_a_live_turn_tail() {
        let mut app = App::test_default();
        app.active_messages_mut().push(assistant_tail("streaming"));
        app.active_messages_mut()[0].turn_info.started_at = Some(std::time::Instant::now());
        app.clear_active_turn_assistant();
        app.status = AppStatus::Running;
        let tail_idx = app.messages().len() - 1;

        upsert_tool_call_into_assistant_message(&mut app, subagent_root("toolu_live", None), None);

        assert_eq!(app.messages().len(), tail_idx + 1, "no new bubble for a live tail");
        assert_eq!(app.messages()[tail_idx].blocks.len(), 2, "the call lands on the live tail");
        assert_eq!(
            app.active_turn_assistant_idx(),
            Some(tail_idx),
            "the pointer binds to the live tail",
        );
    }
}
