use super::super::{App, AppStatus, InvalidationLevel, MessageBlock, ToolCallInfo, ToolCallScope};
use super::tool_calls::{
    current_session_id, has_in_progress_tool_calls, json_value_size, log_terminal_spawned,
    parent_tool_use_id_from_meta, raw_input_carries_content, sdk_tool_name_from_meta,
    should_jump_on_large_write, tool_scope_name,
};
use crate::agent::model;
use crate::app::todos::{
    TaskCreateInput, TaskUpdateInput, apply_task_create, apply_task_update,
    parse_task_create_input, parse_task_create_result_id, parse_task_update_input,
};

pub(super) fn handle_tool_call_update_session(app: &mut App, tcu: &model::RenderToolCallUpdate) {
    let id_str = tcu.tool_call_id.clone();
    let Some((mi, bi)) = app.lookup_tool_call(&id_str) else {
        tracing::warn!(
            target: crate::logging::targets::APP_TOOL,
            event_name = "tool_call_update_missing",
            message = "tool call update dropped because tool call was not found",
            outcome = "dropped",
            session_id = %current_session_id(app),
            tool_call_id = %id_str,
            tool_status = ?tcu.fields.status,
        );
        return;
    };
    if let Some(parent_tool_use_id) = parent_tool_use_id_from_meta(tcu.meta.as_ref()) {
        app.register_tool_call_scope(
            id_str.clone(),
            ToolCallScope::SubagentChild { parent_tool_use_id: parent_tool_use_id.to_owned() },
        );
    }
    let tool_scope = app.tool_call_scope(&id_str);
    let previous_status =
        app.messages().get(mi).and_then(|message| message.blocks.get(bi)).and_then(|block| {
            match block {
                MessageBlock::ToolCall(tc) => Some(tc.status),
                _ => None,
            }
        });
    let previous_terminal_id =
        app.messages().get(mi).and_then(|message| message.blocks.get(bi)).and_then(|block| {
            match block {
                MessageBlock::ToolCall(tc) => tc.terminal_id.clone(),
                _ => None,
            }
        });
    apply_tool_scope_status_update(app, &id_str, tool_scope.as_ref(), tcu.fields.status);

    let update_outcome = apply_tool_call_update_to_indexed_block(app, mi, bi, tcu);
    if let Some(mi) = update_outcome.layout_dirty_idx {
        app.recompute_message_retained_bytes(mi);
        app.invalidate_layout(InvalidationLevel::MessageChanged(mi));
    }
    log_tool_call_update_applied(
        app,
        &id_str,
        tcu,
        tool_scope.as_ref(),
        previous_status,
        &update_outcome,
    );
    log_command_update_applied(app, &id_str, previous_status, previous_terminal_id.as_deref());
    // #268: TaskCreate / TaskUpdate apply directly to `app.todos_mut()`
    // - they're append / mutate / remove deltas, not full-list
    // replacements, so they bypass any "all-completed clears" cascade.
    if let Some(delta) = update_outcome.pending_task_delta {
        let quiet = super::skip_operational_log_during_replay(app);
        match delta {
            TaskDelta::Create { input, id } => {
                if !quiet {
                    let session_id = current_session_id(app);
                    tracing::info!(
                        target: crate::logging::targets::APP_TOOL,
                        event_name = "task_create_applied",
                        message = "TaskCreate item added to inspector list",
                        outcome = "success",
                        session_id = %session_id,
                        tool_call_id = %id_str,
                        task_id = %id,
                        tool_name = "TaskCreate",
                    );
                }
                apply_task_create(app, input, id);
            }
            TaskDelta::Update(update) => {
                if !quiet {
                    let session_id = current_session_id(app);
                    tracing::info!(
                        target: crate::logging::targets::APP_TOOL,
                        event_name = "task_update_applied",
                        message = "TaskUpdate applied to inspector list",
                        outcome = "success",
                        session_id = %session_id,
                        tool_call_id = %id_str,
                        task_id = %update.task_id,
                        tool_name = "TaskUpdate",
                    );
                }
                if let Some(unknown) = update.unknown_status.as_deref() {
                    // Reported here rather than in the parser: the two
                    // records that used to carry this one's session and
                    // tool call are silenced during a replay, and a
                    // warning nothing can be attributed to is close to
                    // no warning at all.
                    tracing::warn!(
                        target: crate::logging::targets::APP_TOOL,
                        event_name = "task_update_unknown_status",
                        message = "TaskUpdate carried an unrecognised status value; no status mutation applied",
                        outcome = "skipped",
                        session_id = %current_session_id(app),
                        tool_call_id = %id_str,
                        task_id = %update.task_id,
                        status = %unknown,
                    );
                }
                apply_task_update(app, update, &id_str);
            }
        }
    }
    if matches!(app.status, AppStatus::Running) && !has_in_progress_tool_calls(app) {
        app.status = AppStatus::Thinking;
    }
}

fn apply_tool_scope_status_update(
    app: &mut App,
    id_str: &str,
    tool_scope: Option<&ToolCallScope>,
    status: Option<model::ToolCallStatus>,
) {
    let Some(status) = status else {
        return;
    };
    match tool_scope {
        Some(ToolCallScope::SubagentRoot) => match status {
            model::ToolCallStatus::Pending | model::ToolCallStatus::InProgress => {
                app.insert_active_task(id_str.to_owned());
            }
            model::ToolCallStatus::Completed
            | model::ToolCallStatus::Failed
            | model::ToolCallStatus::Killed => {
                app.remove_active_task(id_str);
            }
        },
        Some(ToolCallScope::SubagentChild { .. } | ToolCallScope::MainAgent) | None => {}
    }
}

struct ToolCallUpdateApplyOutcome {
    changed: bool,
    layout_dirty_idx: Option<usize>,
    /// `Some(delta)` when this update completed a `TaskCreate` or
    /// applied a `TaskUpdate` (#268). The outer handler dispatches
    /// to `apply_task_create` / `apply_task_update` because both
    /// reducers mutate `app.todos_mut()` and the inner block's
    /// mutable borrow of `app.active_messages_mut()` would conflict.
    /// `None` for every other tool call.
    pending_task_delta: Option<TaskDelta>,
}

/// #268: Per-call mutation extracted from a `TaskCreate` /
/// `TaskUpdate` tool call result. Carried out of
/// `apply_tool_call_update_to_indexed_block` (which holds a mut
/// borrow on `active_messages_mut`) so the outer handler can apply
/// it via the `app.todos_mut()` reducer free of borrow conflicts.
enum TaskDelta {
    /// `TaskCreate` completed with a parseable `Task #N created
    /// successfully:` result text. The reducer appends a TodoItem
    /// keyed by `id`.
    Create { input: TaskCreateInput, id: String },
    /// `TaskUpdate` carried a valid `taskId`. The reducer mutates
    /// the matching item or removes it (status == "deleted").
    Update(TaskUpdateInput),
}

/// One row per field update the reducer folds. The array below is typed with
/// it, so adding a row is a compile error until this moves; the per-field test
/// asserts its own table covers every row, so forgetting a case fails too.
const FIELD_UPDATE_COUNT: usize = 9;

/// One field update's outcome. `body` says whether the field it writes can
/// change the bytes `render_tool_call_body` produces; `changed` says whether
/// it wrote anything at all.
///
/// Both aggregates are folded from a list of these, so a tenth field is
/// added once and the two cannot drift apart.
struct FieldUpdate {
    body: bool,
    changed: bool,
}

impl FieldUpdate {
    /// Writes a field the rendered body reads.
    fn body(changed: bool) -> Self {
        Self { body: true, changed }
    }

    /// Writes a field the body ignores but the message still renders.
    fn presentation(changed: bool) -> Self {
        Self { body: false, changed }
    }
}

fn apply_tool_call_update_to_indexed_block(
    app: &mut App,
    mi: usize,
    bi: usize,
    tcu: &model::RenderToolCallUpdate,
) -> ToolCallUpdateApplyOutcome {
    let mut out = ToolCallUpdateApplyOutcome {
        changed: false,
        layout_dirty_idx: None,
        pending_task_delta: None,
    };
    // Snapshot upfront so the per-tool mutable-borrow of `app.active_messages_mut()`
    // doesn't conflict with `&app.cwd_raw`.
    let cwd_raw = app.cwd_raw();
    let mut should_engage_auto_scroll = false;

    if let Some(MessageBlock::ToolCall(tc)) =
        app.active_messages_mut().get_mut(mi).and_then(|m| m.blocks.get_mut(bi))
    {
        let tc = tc.as_mut();
        // What `render_tool_call_body` reads on `ToolCallInfo`, and therefore
        // which rows below say `body`: `sdk_tool_name` (the Execute branch,
        // and `"Write"` picks the diff highlight window), `status` (the
        // Execute gate plus the Failed/Killed/InProgress branches),
        // `terminal_output`, `content`, and `title` (`is_markdown_file` /
        // `lang_from_title` pick markdown over code highlighting). Nothing
        // else.
        //
        // Two rows write under a different name than they read: `raw_output`
        // feeds `terminal_output`, and `name` feeds `sdk_tool_name`.
        //
        // The presentation rows still reach the message, so the layout epoch
        // has to keep bumping or the message keeps its previous render:
        // `hash_message_block_into` hashes that epoch, `raw_input` picks the
        // one-liner target (`read_target`, `search_target`, `web_target`) and
        // the peer-block shape, the two metadata fields add title badges, and
        // `hidden` is hashed directly.
        let updates: [FieldUpdate; FIELD_UPDATE_COUNT] = [
            FieldUpdate::body(apply_tool_call_status_update(tc, tcu.fields.status)),
            FieldUpdate::body(apply_tool_call_title_update(
                tc,
                tcu.fields.title.as_deref(),
                &cwd_raw,
            )),
            FieldUpdate::body(apply_tool_call_content_update(tc, tcu.fields.content.as_deref())),
            FieldUpdate::presentation(apply_tool_call_raw_input_update(
                tc,
                tcu.fields.raw_input.as_ref(),
            )),
            FieldUpdate::presentation(apply_tool_call_output_metadata_update(
                tc,
                tcu.fields.output_metadata.as_ref(),
            )),
            FieldUpdate::presentation(apply_tool_call_task_metadata_update(
                tc,
                tcu.fields.task_metadata.as_ref(),
            )),
            FieldUpdate::body(apply_tool_call_raw_output_update(
                tc,
                tcu.fields.raw_output.as_ref(),
            )),
            FieldUpdate::body(apply_tool_call_name_update(tc, tcu.meta.as_ref())),
            FieldUpdate::presentation(apply_tool_call_hidden_update(tc, tcu.meta.as_ref())),
        ];

        let body_changed = updates.iter().any(|u| u.body && u.changed);
        let changed = updates.iter().any(|u| u.changed);
        // #268: Task* family delta. Read post-apply so `tc.status`
        // reflects the fields just merged from this update; the
        // delta fires exactly once per tool_call when the call
        // reaches `Completed` AND its raw_input + (for TaskCreate)
        // raw_output are present. TaskGet / TaskList carry no
        // delta - they're chat-suppressed but produce no state.
        out.pending_task_delta = extract_task_delta_from_tool_call_update(tc);

        if changed {
            out.changed = true;
            should_engage_auto_scroll = should_jump_on_large_write(tc);
            // A body-preserving change still has to re-measure (the title
            // is rendered live), but it must not cost a body rebuild.
            if body_changed {
                tc.mark_tool_call_layout_dirty();
            } else {
                tc.mark_tool_call_layout_dirty_only();
            }
            out.layout_dirty_idx = Some(mi);
        } else {
            crate::perf::mark("tool_update_noop_skips");
        }
    }
    if should_engage_auto_scroll {
        app.active_viewport_mut().engage_auto_scroll();
    }
    if out.changed {
        app.sync_render_cache_slot(mi, bi);
    }

    out
}

fn apply_tool_call_status_update(
    tc: &mut ToolCallInfo,
    status: Option<model::ToolCallStatus>,
) -> bool {
    if let Some(status) = status
        && tc.status != status
    {
        tc.status = status;
        return true;
    }
    false
}

fn apply_tool_call_title_update(tc: &mut ToolCallInfo, title: Option<&str>, cwd_raw: &str) -> bool {
    let Some(title) = title else {
        return false;
    };
    let shortened = super::tool_calls::shorten_tool_title(title, cwd_raw);
    if tc.title == shortened {
        return false;
    }
    tc.title = shortened;
    true
}

fn apply_tool_call_content_update(
    tc: &mut ToolCallInfo,
    content: Option<&[model::RenderToolCallContent]>,
) -> bool {
    let Some(content) = content else {
        return false;
    };
    let mut changed = false;
    for cb in content {
        if let model::RenderToolCallContent::Terminal(t) = cb {
            let tid = t.terminal_id.clone();
            if tc.terminal_id.as_deref() != Some(tid.as_str()) {
                tc.terminal_id = Some(tid);
                changed = true;
            }
        }
    }
    // Preserve the original Diff for Edit / Write tools when the
    // incoming update doesn't include one. The original Diff was
    // synthesized from the tool_use input (old_string / new_string
    // for Edit, content for Write) at tool-call creation time; a
    // decline or error result arrives as a plain Text content block,
    // and the naive `tc.content = content.to_vec()` below would
    // overwrite the diff with the error text - losing the green/red
    // diff coloring the user expects to still see after declining.
    let preserve_diff = matches!(tc.sdk_tool_name.as_str(), "Edit" | "Write")
        && !content.iter().any(|c| matches!(c, model::RenderToolCallContent::Diff(_)))
        && tc.content.iter().any(|c| matches!(c, model::RenderToolCallContent::Diff(_)));
    let new_content: Vec<model::RenderToolCallContent> = if preserve_diff {
        let mut combined: Vec<model::RenderToolCallContent> = tc
            .content
            .iter()
            .filter(|c| matches!(c, model::RenderToolCallContent::Diff(_)))
            .cloned()
            .collect();
        combined.extend_from_slice(content);
        combined
    } else {
        content.to_vec()
    };
    if tc.content != new_content {
        tc.content = new_content;
        changed = true;
    }
    changed
}

fn apply_tool_call_raw_input_update(
    tc: &mut ToolCallInfo,
    raw_input: Option<&serde_json::Value>,
) -> bool {
    let Some(raw_input) = raw_input else {
        return false;
    };
    if !raw_input_carries_content(raw_input) {
        return false;
    }
    tc.set_raw_input(Some(raw_input.clone()))
}

fn apply_tool_call_output_metadata_update(
    tc: &mut ToolCallInfo,
    output_metadata: Option<&model::ToolOutputMetadata>,
) -> bool {
    let Some(output_metadata) = output_metadata else {
        return false;
    };
    if tc.output_metadata.as_ref() == Some(output_metadata) {
        return false;
    }
    tc.output_metadata = Some(output_metadata.clone());
    true
}

fn apply_tool_call_task_metadata_update(
    tc: &mut ToolCallInfo,
    task_metadata: Option<&model::TaskMetadata>,
) -> bool {
    let Some(task_metadata) = task_metadata else {
        return false;
    };
    let mut merged = tc.task_metadata.clone().unwrap_or_default();
    if task_metadata.end_time.is_some() {
        merged.end_time = task_metadata.end_time;
    }
    if task_metadata.total_paused_ms.is_some() {
        merged.total_paused_ms = task_metadata.total_paused_ms;
    }
    if task_metadata.error.is_some() {
        merged.error.clone_from(&task_metadata.error);
    }
    if task_metadata.is_backgrounded.is_some() {
        merged.is_backgrounded = task_metadata.is_backgrounded;
    }
    if tc.task_metadata.as_ref() == Some(&merged) {
        return false;
    }
    tc.task_metadata = Some(merged);
    true
}

fn apply_tool_call_raw_output_update(
    tc: &mut ToolCallInfo,
    raw_output: Option<&serde_json::Value>,
) -> bool {
    if !tc.is_execute_tool() {
        return false;
    }
    let Some(raw_output) = raw_output else {
        return false;
    };
    let Some(output) = raw_output_to_terminal_text(raw_output) else {
        return false;
    };
    if tc.terminal_output.as_deref() == Some(output.as_str()) {
        return false;
    }
    tc.terminal_output = Some(output);
    true
}

fn apply_tool_call_name_update(tc: &mut ToolCallInfo, meta: Option<&serde_json::Value>) -> bool {
    let Some(name) = sdk_tool_name_from_meta(meta) else {
        return false;
    };
    if name.trim().is_empty() || tc.sdk_tool_name == name {
        return false;
    }
    name.clone_into(&mut tc.sdk_tool_name);
    true
}

fn apply_tool_call_hidden_update(tc: &mut ToolCallInfo, meta: Option<&serde_json::Value>) -> bool {
    if parent_tool_use_id_from_meta(meta).is_none() || tc.hidden {
        return false;
    }
    tc.hidden = true;
    true
}

/// #268: Extract a `TaskDelta` for the `TaskCreate` / `TaskUpdate`
/// family. `TaskList` and `TaskGet` have no state delta; they share
/// the gate but always return `None` here (they're chat-suppressed
/// at the tool_call construction site).
///
/// `TaskCreate` requires a parseable `id` from the tool's result
/// text; if the tool hasn't completed or the result text doesn't
/// match `^Task #N created successfully:`, returns `None`. The
/// caller is idempotent against `None`, so repeat updates that
/// arrive before the result lands cost nothing.
///
/// `TaskUpdate` requires a parseable `taskId` in `raw_input`; if
/// the input is missing or malformed (no taskId field, non-string),
/// returns `None`.
fn extract_task_delta_from_tool_call_update(tc: &ToolCallInfo) -> Option<TaskDelta> {
    match tc.sdk_tool_name.as_str() {
        "TaskCreate" => {
            if tc.status != model::ToolCallStatus::Completed {
                // Wait for completion - the id only appears in the
                // result text the CLI emits at completion time.
                return None;
            }
            let raw_input = tc.raw_input.as_ref()?;
            let input = parse_task_create_input(raw_input)?;
            let result_text = tool_call_text_output(tc)?;
            let id = parse_task_create_result_id(&result_text)?;
            Some(TaskDelta::Create { input, id })
        }
        "TaskUpdate" => {
            let raw_input = tc.raw_input.as_ref()?;
            let update = parse_task_update_input(raw_input)?;
            Some(TaskDelta::Update(update))
        }
        _ => None,
    }
}

/// Concatenate every `RenderToolCallContent::Content(Text)` block on the
/// tool call into a single `String`. Non-text blocks (diff, terminal,
/// mcp resource, image) are skipped - `TaskCreate`'s assigned id
/// always lives in a Text block per core-v1's wire capture.
fn tool_call_text_output(tc: &ToolCallInfo) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for content in &tc.content {
        if let model::RenderToolCallContent::Content(inner) = content
            && let model::RenderContentBlock::Text(text) = &inner.content
        {
            parts.push(text.text.as_str());
        }
    }
    if parts.is_empty() { None } else { Some(parts.join("\n")) }
}

pub(super) fn raw_output_to_terminal_text(raw_output: &serde_json::Value) -> Option<String> {
    match raw_output {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => (!s.is_empty()).then(|| s.clone()),
        serde_json::Value::Array(items) => {
            let chunks: Vec<&str> = items.iter().filter_map(extract_text_field).collect();
            if chunks.is_empty() {
                serde_json::to_string_pretty(raw_output).ok().filter(|s| !s.is_empty())
            } else {
                Some(chunks.join("\n"))
            }
        }
        value => extract_text_field(value)
            .map(str::to_owned)
            .or_else(|| serde_json::to_string_pretty(value).ok().filter(|s| !s.is_empty())),
    }
}

fn extract_text_field(value: &serde_json::Value) -> Option<&str> {
    value.get("text").and_then(serde_json::Value::as_str)
}

fn log_tool_call_update_applied(
    app: &App,
    id_str: &str,
    tcu: &model::RenderToolCallUpdate,
    tool_scope: Option<&ToolCallScope>,
    previous_status: Option<model::ToolCallStatus>,
    update_outcome: &ToolCallUpdateApplyOutcome,
) {
    if !update_outcome.changed {
        return;
    }

    let Some(tc) = app
        .lookup_tool_call(id_str)
        .and_then(|(mi, bi)| app.messages().get(mi).and_then(|message| message.blocks.get(bi)))
        .and_then(|block| match block {
            MessageBlock::ToolCall(tc) => Some(tc.as_ref()),
            _ => None,
        })
    else {
        return;
    };

    let log_spec = tool_update_log_spec(tc, tcu, previous_status);
    // Level as well as outcome: `tool_call_updated` is DEBUG and also
    // carries `success`, so a condition without the level would silence
    // it too. Hoisted above the field work rather than sitting in the
    // arm, since none of it is written when this returns.
    if matches!(log_spec.level, ToolUpdateLogLevel::Info)
        && log_spec.outcome == "success"
        && super::skip_operational_log_during_replay(app)
    {
        return;
    }
    let session_id = current_session_id(app);
    let scope_name = tool_scope.map_or("unknown", tool_scope_name);
    let raw_output_chars = tcu.fields.raw_output.as_ref().and_then(|value| match value {
        serde_json::Value::String(text) => Some(text.chars().count()),
        _ => serde_json::to_string(value).ok().map(|text| text.chars().count()),
    });
    let content_block_count = tcu.fields.content.as_ref().map_or(tc.content.len(), Vec::len);
    let raw_input_bytes = json_value_size(tcu.fields.raw_input.as_ref()).unwrap_or_default();
    let location_count = tcu.fields.locations.as_ref().map_or(0, Vec::len);

    match log_spec.level {
        ToolUpdateLogLevel::Info => {
            tracing::info!(
                target: crate::logging::targets::APP_TOOL,
                event_name = log_spec.event_name,
                message = log_spec.message,
                outcome = log_spec.outcome,
                session_id = %session_id,
                tool_call_id = %id_str,
                tool_name = %tc.sdk_tool_name,
                tool_title = %tc.title,
                tool_scope = scope_name,
                previous_status = ?previous_status,
                tool_status = ?tc.status,
                content_block_count,
                raw_output_chars = raw_output_chars.unwrap_or_default(),
                has_output_metadata = tc.output_metadata.is_some(),
                has_task_metadata = tc.task_metadata.is_some(),
            );
        }
        ToolUpdateLogLevel::Warn => tracing::warn!(
            target: crate::logging::targets::APP_TOOL,
            event_name = log_spec.event_name,
            message = log_spec.message,
            outcome = log_spec.outcome,
            session_id = %session_id,
            tool_call_id = %id_str,
            tool_name = %tc.sdk_tool_name,
            tool_title = %tc.title,
            tool_scope = scope_name,
            previous_status = ?previous_status,
            tool_status = ?tc.status,
            content_block_count,
            raw_output_chars = raw_output_chars.unwrap_or_default(),
            has_output_metadata = tc.output_metadata.is_some(),
            has_task_metadata = tc.task_metadata.is_some(),
        ),
        ToolUpdateLogLevel::Debug => tracing::debug!(
            target: crate::logging::targets::APP_TOOL,
            event_name = log_spec.event_name,
            message = log_spec.message,
            outcome = log_spec.outcome,
            session_id = %session_id,
            tool_call_id = %id_str,
            tool_name = %tc.sdk_tool_name,
            tool_title = %tc.title,
            tool_scope = scope_name,
            previous_status = ?previous_status,
            tool_status = ?tc.status,
            content_block_count,
            raw_output_chars = raw_output_chars.unwrap_or_default(),
            has_output_metadata = tc.output_metadata.is_some(),
            has_task_metadata = tc.task_metadata.is_some(),
            title_changed = tcu.fields.title.is_some(),
            status_changed = tcu.fields.status != previous_status,
            raw_input_bytes,
            location_count,
        ),
    }
}

#[derive(Clone)]
enum ToolUpdateLogLevel {
    Info,
    Warn,
    Debug,
}

#[derive(Clone)]
struct ToolUpdateLogSpec {
    level: ToolUpdateLogLevel,
    event_name: &'static str,
    message: &'static str,
    outcome: &'static str,
}

fn tool_update_log_spec(
    tc: &ToolCallInfo,
    tcu: &model::RenderToolCallUpdate,
    previous_status: Option<model::ToolCallStatus>,
) -> ToolUpdateLogSpec {
    match tc.status {
        model::ToolCallStatus::Completed => ToolUpdateLogSpec {
            level: if entered_final_status(previous_status, tc.status) {
                ToolUpdateLogLevel::Info
            } else {
                ToolUpdateLogLevel::Debug
            },
            event_name: if entered_final_status(previous_status, tc.status) {
                "tool_call_completed"
            } else {
                "tool_call_updated"
            },
            message: if entered_final_status(previous_status, tc.status) {
                "tool call completed"
            } else {
                "tool call updated after completion"
            },
            outcome: "success",
        },
        model::ToolCallStatus::Failed | model::ToolCallStatus::Killed => {
            if !entered_final_status(previous_status, tc.status) {
                return ToolUpdateLogSpec {
                    level: ToolUpdateLogLevel::Debug,
                    event_name: "tool_call_updated",
                    message: "tool call updated after failure",
                    outcome: "failure",
                };
            }
            if let Some(raw_output) = tcu.fields.raw_output.as_ref() {
                let text = match raw_output {
                    serde_json::Value::String(text) => text.to_ascii_lowercase(),
                    value => serde_json::to_string(value).unwrap_or_default().to_ascii_lowercase(),
                };
                if text.contains("permission denied")
                    || text.contains("cancelled by user")
                    || text.contains("plan rejected")
                    || text.contains("question cancelled")
                {
                    return ToolUpdateLogSpec {
                        level: ToolUpdateLogLevel::Info,
                        event_name: "tool_call_refused",
                        message: "tool call refused",
                        outcome: "cancelled",
                    };
                }
                if text.contains("timed out") || text.contains("timeout") {
                    return ToolUpdateLogSpec {
                        level: ToolUpdateLogLevel::Warn,
                        event_name: "tool_call_timeout",
                        message: "tool call timed out",
                        outcome: "timeout",
                    };
                }
            }
            ToolUpdateLogSpec {
                level: ToolUpdateLogLevel::Warn,
                event_name: if matches!(tc.status, model::ToolCallStatus::Killed) {
                    "tool_call_killed"
                } else {
                    "tool_call_failed"
                },
                message: if matches!(tc.status, model::ToolCallStatus::Killed) {
                    "tool call killed"
                } else {
                    "tool call failed"
                },
                outcome: "failure",
            }
        }
        model::ToolCallStatus::Pending | model::ToolCallStatus::InProgress => ToolUpdateLogSpec {
            level: ToolUpdateLogLevel::Debug,
            event_name: "tool_call_updated",
            message: "tool call updated",
            outcome: "success",
        },
    }
}

fn entered_final_status(
    previous_status: Option<model::ToolCallStatus>,
    current_status: model::ToolCallStatus,
) -> bool {
    matches!(
        current_status,
        model::ToolCallStatus::Completed
            | model::ToolCallStatus::Failed
            | model::ToolCallStatus::Killed
    ) && !matches!(previous_status, Some(status) if status == current_status)
}

fn log_command_update_applied(
    app: &App,
    id_str: &str,
    previous_status: Option<model::ToolCallStatus>,
    previous_terminal_id: Option<&str>,
) {
    let Some(tc) = app
        .lookup_tool_call(id_str)
        .and_then(|(mi, bi)| app.messages().get(mi).and_then(|message| message.blocks.get(bi)))
        .and_then(|block| match block {
            MessageBlock::ToolCall(tc) => Some(tc.as_ref()),
            _ => None,
        })
    else {
        return;
    };

    if !tc.is_execute_tool() {
        return;
    }

    if previous_terminal_id.is_none() && tc.terminal_id.is_some() {
        log_terminal_spawned(app, tc, "update");
    }

    let transitioned_to_final = matches!(
        previous_status,
        Some(model::ToolCallStatus::Pending | model::ToolCallStatus::InProgress)
    ) && matches!(
        tc.status,
        model::ToolCallStatus::Completed
            | model::ToolCallStatus::Failed
            | model::ToolCallStatus::Killed
    );
    if !transitioned_to_final {
        return;
    }

    // Status in the condition: the warning arm below must survive a
    // replay, and `command_failure_kind` lowercases the whole terminal
    // output, which is wasted for a record that will not be written.
    if matches!(tc.status, model::ToolCallStatus::Completed)
        && super::skip_operational_log_during_replay(app)
    {
        return;
    }
    let failure_kind = command_failure_kind(tc);
    match tc.status {
        model::ToolCallStatus::Completed => {
            tracing::info!(
                target: crate::logging::targets::APP_COMMAND,
                event_name = "command_completed",
                message = "command execution completed",
                outcome = "success",
                session_id = %current_session_id(app),
                tool_call_id = %tc.id,
                terminal_id = %tc.terminal_id.as_deref().unwrap_or(""),
                tool_name = %tc.sdk_tool_name,
                terminal_output_bytes =
                    u64::try_from(tc.terminal_output.as_deref().map_or(0, str::len))
                        .unwrap_or_default(),
                has_terminal = tc.terminal_id.is_some(),
                assistant_auto_backgrounded = tc.assistant_auto_backgrounded(),
            );
        }
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
            tool_name = %tc.sdk_tool_name,
            error_kind = failure_kind,
            terminal_output_bytes = u64::try_from(tc.terminal_output.as_deref().map_or(0, str::len))
                .unwrap_or_default(),
            has_terminal = tc.terminal_id.is_some(),
            assistant_auto_backgrounded = tc.assistant_auto_backgrounded(),
        ),
        model::ToolCallStatus::Pending | model::ToolCallStatus::InProgress => {}
    }
}

fn command_failure_kind(tc: &ToolCallInfo) -> &'static str {
    let text = tc.terminal_output.as_deref().unwrap_or("").to_ascii_lowercase();
    if text.contains("permission denied")
        || text.contains("cancelled by user")
        || text.contains("plan rejected")
        || text.contains("question cancelled")
    {
        return "refused";
    }
    if text.contains("timed out") || text.contains("timeout") {
        return "timeout";
    }
    "command_error"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, BlockCache, ChatMessage, MessageBlock, MessageRole};
    use crate::ui::tool_call::measure_tool_call_height_cached_with_tools_collapsed;
    use pretty_assertions::assert_eq;

    fn make_bash_tool_call(
        id: &str,
        status: model::ToolCallStatus,
        terminal_id: Option<&str>,
    ) -> ToolCallInfo {
        ToolCallInfo {
            id: id.to_owned(),
            title: format!("tool {id}"),
            sdk_tool_name: "Bash".to_owned(),
            raw_input: None,
            raw_input_bytes: 0,
            output_metadata: None,
            task_metadata: None,
            status,
            content: Vec::new(),
            hidden: false,
            terminal_id: terminal_id.map(str::to_owned),
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

    fn make_task_tool_call(id: &str, status: model::ToolCallStatus) -> ToolCallInfo {
        ToolCallInfo {
            id: id.to_owned(),
            title: format!("task {id}"),
            sdk_tool_name: "Agent".to_owned(),
            raw_input: None,
            raw_input_bytes: 0,
            output_metadata: None,
            task_metadata: None,
            status,
            content: Vec::new(),
            hidden: false,
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

    /// Populate the body cache through the cached render path.
    fn warm_body_cache(tc: &mut ToolCallInfo) {
        let mut out = Vec::new();
        crate::ui::tool_call::render_tool_call_cached_with_tools_collapsed(
            tc,
            crate::ui::tool_call::ToolCallRenderContext::default(),
            80,
            '\u{280B}',
            false,
            &mut out,
        );
    }

    /// The cached body rows. Empty when nothing is cached.
    ///
    /// Compares `Line`s rather than flattened text so a style-only change
    /// still counts as a difference.
    fn cached_body(tc: &ToolCallInfo) -> Vec<ratatui::text::Line<'static>> {
        tc.cache.get().cloned().unwrap_or_default()
    }

    /// Which tool the per-field cases run against. `render_tool_content`
    /// returns early on the Execute branch, so the two reach different code,
    /// and only the content fixture reaches the loop that reads `title`.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Fixture {
        /// Bash with terminal output.
        Execute,
        /// Read with a text block.
        Content,
    }

    /// Push one tool call with a live body into a fresh app.
    fn app_with_tool(id: &str, fixture: Fixture, status: model::ToolCallStatus) -> App {
        let mut app = App::test_default();
        let mut tc = make_bash_tool_call(id, status, Some("term-1"));
        match fixture {
            Fixture::Execute => tc.terminal_output = Some("alpha\nbeta\ngamma\n".to_owned()),
            Fixture::Content => {
                tc.sdk_tool_name = "Read".to_owned();
                tc.terminal_id = None;
                tc.content = vec![model::RenderToolCallContent::Content(model::ContentChunk::new(
                    model::RenderContentBlock::Text(model::TextContent::new("fn main() {}\n")),
                ))];
            }
        }
        app.active_messages_mut().push(ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::ToolCall(Box::new(tc))],
        ));
        app.index_tool_call(id.to_owned(), 0, 0);
        app
    }

    fn tool_call_block(app: &mut App) -> &mut ToolCallInfo {
        match &mut app.active_messages_mut()[0].blocks[0] {
            MessageBlock::ToolCall(tc) => tc.as_mut(),
            _ => panic!("expected a tool call block"),
        }
    }

    /// One field-update case: a name for the failure message, whether the
    /// body cache may be kept, and an update that writes ONLY that field.
    struct FieldCase {
        field: &'static str,
        body_preserving: bool,
        /// Set for `raw_output`, which the reducer drops for a non-Execute
        /// tool, so the content fixture has no case to make for it.
        execute_only: bool,
        update: model::RenderToolCallUpdate,
    }

    /// Every field `apply_tool_call_update_to_indexed_block` writes, with
    /// the path its classification should take. The expectations here are
    /// read off `render_tool_call_body`, not off the production table, so
    /// this is an independent oracle rather than a restatement of it.
    fn field_cases(id: &str) -> Vec<FieldCase> {
        let meta = |value: serde_json::Value| {
            model::RenderToolCallUpdate::new(id, model::RenderToolCallUpdateFields::new())
                .meta(value)
        };
        vec![
            // Execute gate, plus the Failed/Killed/InProgress branches.
            FieldCase {
                field: "status",
                body_preserving: false,
                execute_only: false,
                update: model::RenderToolCallUpdate::new(
                    id,
                    model::RenderToolCallUpdateFields::new().status(model::ToolCallStatus::Failed),
                ),
            },
            // `is_markdown_file` and `lang_from_title` read it.
            FieldCase {
                field: "title",
                body_preserving: false,
                execute_only: false,
                update: model::RenderToolCallUpdate::new(
                    id,
                    model::RenderToolCallUpdateFields::new().title("renamed tool"),
                ),
            },
            // The body source itself.
            FieldCase {
                field: "content",
                body_preserving: false,
                execute_only: false,
                update: model::RenderToolCallUpdate::new(
                    id,
                    model::RenderToolCallUpdateFields::new().content(vec![
                        model::RenderToolCallContent::Content(model::ContentChunk::new(
                            model::RenderContentBlock::Text(model::TextContent::new("new body")),
                        )),
                    ]),
                ),
            },
            // Picks the one-liner target, not the body.
            FieldCase {
                field: "raw_input",
                body_preserving: true,
                execute_only: false,
                update: model::RenderToolCallUpdate::new(
                    id,
                    model::RenderToolCallUpdateFields::new()
                        .raw_input(serde_json::json!({ "file_path": "/tmp/x.rs" })),
                ),
            },
            // Title badge.
            FieldCase {
                field: "output_metadata",
                body_preserving: true,
                execute_only: false,
                update: model::RenderToolCallUpdate::new(
                    id,
                    model::RenderToolCallUpdateFields::new().output_metadata(
                        model::ToolOutputMetadata::new().bash(Some(
                            model::BashOutputMetadata::new()
                                .assistant_auto_backgrounded(Some(true)),
                        )),
                    ),
                ),
            },
            // Title badge.
            FieldCase {
                field: "task_metadata",
                body_preserving: true,
                execute_only: false,
                update: model::RenderToolCallUpdate::new(
                    id,
                    model::RenderToolCallUpdateFields::new()
                        .task_metadata(model::TaskMetadata::new().backgrounded(Some(true))),
                ),
            },
            // The Execute body's terminal output.
            FieldCase {
                field: "raw_output",
                body_preserving: false,
                execute_only: true,
                update: model::RenderToolCallUpdate::new(
                    id,
                    model::RenderToolCallUpdateFields::new()
                        .raw_output(serde_json::json!("delta\nepsilon")),
                ),
            },
            // The Execute branch, and `"Write"` picks the diff window. The
            // name has to differ from BOTH fixtures' starting names, or the
            // update is a no-op on the one that already carries it.
            FieldCase {
                field: "name",
                body_preserving: false,
                execute_only: false,
                update: meta(serde_json::json!({ "claudeCode": { "toolName": "Glob" } })),
            },
            // Hashed into the message signature directly.
            FieldCase {
                field: "hidden",
                body_preserving: true,
                execute_only: false,
                update: meta(
                    serde_json::json!({ "claudeCode": { "parentToolUseId": "parent-1" } }),
                ),
            },
        ]
    }

    #[test]
    fn field_updates_take_the_path_the_render_function_implies() {
        // One case per field, over both tool shapes, so mis-classifying ANY
        // field fails here rather than silently serving a stale body. Both
        // directions are covered: a body-affecting field taking the cheap
        // path corrupts the render, and a presentation field taking the
        // expensive path is the regression this work exists to prevent.
        //
        // Two fixtures rather than one because `render_tool_content` returns
        // early on the Execute branch: a leak that only lands on the content
        // loop is invisible to the execute fixture.
        let id = "tu-field-path";
        let cases = field_cases(id);
        // Ties the oracle to the production array: a tenth row moves
        // `FIELD_UPDATE_COUNT` and lands here until it has a case.
        assert_eq!(cases.len(), FIELD_UPDATE_COUNT, "every field update needs a case here");

        for fixture in [Fixture::Execute, Fixture::Content] {
            for case in &cases {
                if case.execute_only && fixture == Fixture::Content {
                    // Prove the skip instead of trusting the flag: an
                    // execute-only field must genuinely not apply here, or a
                    // future case marked this way would silently lose its
                    // content coverage.
                    let mut app = app_with_tool(id, fixture, model::ToolCallStatus::InProgress);
                    let layout_before = tool_call_block(&mut app).layout_epoch;
                    handle_tool_call_update_session(&mut app, &case.update);
                    assert_eq!(
                        tool_call_block(&mut app).layout_epoch,
                        layout_before,
                        "{}: marked execute-only, but it applied on the content fixture",
                        case.field
                    );
                    continue;
                }
                let mut app = app_with_tool(id, fixture, model::ToolCallStatus::InProgress);
                let layout_generation = 7;

                let (before_body, epoch_before, layout_before) = {
                    let tc = tool_call_block(&mut app);
                    warm_body_cache(tc);
                    // Warm the measured-height key so its invalidation below is
                    // an observation rather than an artefact of never measuring.
                    measure_tool_call_height_cached_with_tools_collapsed(
                        tc,
                        crate::ui::tool_call::ToolCallRenderContext::default(),
                        80,
                        '\u{280B}',
                        layout_generation,
                        false,
                    );
                    assert!(
                        tc.cache_measurement_key_matches(80, layout_generation, false),
                        "{}: precondition, the measurement key must be warm",
                        case.field
                    );
                    let body = cached_body(tc);
                    assert!(
                        !body.is_empty(),
                        "{}: precondition, there must be a cached body to lose",
                        case.field
                    );
                    (body, tc.render_epoch, tc.layout_epoch)
                };

                app.last_invalidation_level.set(None);
                handle_tool_call_update_session(&mut app, &case.update);
                let invalidation = app.last_invalidation_level.get();

                let tc = tool_call_block(&mut app);
                // Without this a case whose update never applied would still
                // pass, because nothing would have moved.
                assert!(
                    tc.layout_epoch > layout_before,
                    "{}: the update did not apply, so this case proves nothing",
                    case.field
                );
                // The message is invalidated either way: a presentation field
                // moves the badges or the one-liner target, a body field moves
                // the body itself. Reporting none leaves the visible call stale.
                assert_eq!(
                    invalidation,
                    Some(InvalidationLevel::MessageChanged(0)),
                    "{}: the update must invalidate the message",
                    case.field
                );

                if case.body_preserving {
                    assert_eq!(
                        tc.render_epoch, epoch_before,
                        "{}: expected the body-preserving path",
                        case.field
                    );
                    assert!(
                        tc.cache.get().is_some(),
                        "{}: the rendered body must survive",
                        case.field
                    );
                    // The height is recomputed even though the body is reused,
                    // or a badge that moved the row count would keep its old
                    // height.
                    assert!(
                        !tc.cache_measurement_key_matches(80, layout_generation, false),
                        "{}: the height must still be recomputed",
                        case.field
                    );
                    // Independent proof that reusing the body was safe, rather
                    // than a comparison of the cache against itself: rebuild from
                    // scratch and compare against the pre-update render.
                    tc.cache.invalidate();
                    warm_body_cache(tc);
                    assert_eq!(
                        cached_body(tc),
                        before_body,
                        "{}: this field must not reach the body, so a fresh render must match",
                        case.field
                    );
                } else {
                    assert!(
                        tc.render_epoch > epoch_before,
                        "{}: expected the body-affecting path",
                        case.field
                    );
                    assert!(
                        tc.cache.get().is_none(),
                        "{}: the cached body must be discarded",
                        case.field
                    );
                }
            }
        }
    }

    #[test]
    fn empty_object_raw_input_update_preserves_subagent_label() {
        let mut app = App::test_default();
        let root_id = "tu-root-empty-input";
        let mut root = make_task_tool_call(root_id, model::ToolCallStatus::InProgress);
        root.sdk_tool_name = "Task".to_owned();
        root.raw_input = Some(serde_json::json!({
            "subagent_type": "Explore",
            "description": "Map the pipeline",
            "prompt": "map the render pipeline",
        }));
        app.active_messages_mut().push(ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::ToolCall(Box::new(root))],
        ));
        app.index_tool_call(root_id.to_owned(), 0, 0);
        app.register_tool_call_scope(root_id.to_owned(), ToolCallScope::SubagentRoot);

        assert_eq!(app.subagents_view()[0].label, "Explore \u{b7} Map the pipeline");

        let update = model::RenderToolCallUpdate::new(
            root_id,
            model::RenderToolCallUpdateFields::new().raw_input(serde_json::json!({})),
        );
        handle_tool_call_update_session(&mut app, &update);

        assert_eq!(
            app.subagents_view()[0].label,
            "Explore \u{b7} Map the pipeline",
            "empty-object update must not clobber the subagent dispatch input",
        );
    }

    #[test]
    fn repeated_completed_status_update_does_not_log_a_second_completion() {
        let tc = make_bash_tool_call("tool-1", model::ToolCallStatus::Completed, None);
        let update =
            model::RenderToolCallUpdate::new("tool-1", model::RenderToolCallUpdateFields::new());

        let spec = tool_update_log_spec(&tc, &update, Some(model::ToolCallStatus::Completed));

        assert!(matches!(spec.level, ToolUpdateLogLevel::Debug));
        assert_eq!(spec.event_name, "tool_call_updated");
        assert_eq!(spec.outcome, "success");
    }

    #[test]
    fn first_completed_status_update_logs_completion() {
        let tc = make_bash_tool_call("tool-1", model::ToolCallStatus::Completed, None);
        let update = model::RenderToolCallUpdate::new(
            "tool-1",
            model::RenderToolCallUpdateFields::new().status(model::ToolCallStatus::Completed),
        );

        let spec = tool_update_log_spec(&tc, &update, Some(model::ToolCallStatus::InProgress));

        assert!(matches!(spec.level, ToolUpdateLogLevel::Info));
        assert_eq!(spec.event_name, "tool_call_completed");
        assert_eq!(spec.outcome, "success");
    }

    #[test]
    fn task_metadata_update_is_applied_to_tool_call() {
        let mut app = App::test_default();
        let tool_id = "task-1";
        app.active_messages_mut().push(ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::ToolCall(Box::new(make_task_tool_call(
                tool_id,
                model::ToolCallStatus::InProgress,
            )))],
        ));
        app.index_tool_call(tool_id.to_owned(), 0, 0);

        let update = model::RenderToolCallUpdate::new(
            tool_id,
            model::RenderToolCallUpdateFields::new().task_metadata(
                model::TaskMetadata::new()
                    .error(Some("Task paused".to_owned()))
                    .backgrounded(Some(true)),
            ),
        );

        handle_tool_call_update_session(&mut app, &update);

        let MessageBlock::ToolCall(tc) = &app.messages()[0].blocks[0] else {
            panic!("expected tool call block");
        };
        assert_eq!(
            tc.task_metadata,
            Some(
                model::TaskMetadata::new()
                    .error(Some("Task paused".to_owned()))
                    .backgrounded(Some(true)),
            )
        );
    }

    #[test]
    fn task_metadata_update_merges_partial_patches() {
        let mut app = App::test_default();
        let tool_id = "task-1";
        app.active_messages_mut().push(ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::ToolCall(Box::new(make_task_tool_call(
                tool_id,
                model::ToolCallStatus::InProgress,
            )))],
        ));
        app.index_tool_call(tool_id.to_owned(), 0, 0);

        let backgrounded_update = model::RenderToolCallUpdate::new(
            tool_id,
            model::RenderToolCallUpdateFields::new()
                .task_metadata(model::TaskMetadata::new().backgrounded(Some(true))),
        );
        handle_tool_call_update_session(&mut app, &backgrounded_update);

        let timing_update = model::RenderToolCallUpdate::new(
            tool_id,
            model::RenderToolCallUpdateFields::new().task_metadata(
                model::TaskMetadata::new()
                    .error(Some("Task stopped by parent agent".to_owned()))
                    .end_time(Some(1234))
                    .total_paused_ms(Some(250)),
            ),
        );
        handle_tool_call_update_session(&mut app, &timing_update);

        let MessageBlock::ToolCall(tc) = &app.messages()[0].blocks[0] else {
            panic!("expected tool call block");
        };
        assert_eq!(
            tc.task_metadata,
            Some(
                model::TaskMetadata::new()
                    .error(Some("Task stopped by parent agent".to_owned()))
                    .end_time(Some(1234))
                    .total_paused_ms(Some(250))
                    .backgrounded(Some(true)),
            )
        );
    }

    #[test]
    fn killed_task_update_clears_active_task_scope() {
        let mut app = App::test_default();
        let tool_id = "task-1";
        app.active_messages_mut().push(ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::ToolCall(Box::new(make_task_tool_call(
                tool_id,
                model::ToolCallStatus::InProgress,
            )))],
        ));
        app.index_tool_call(tool_id.to_owned(), 0, 0);
        app.register_tool_call_scope(tool_id.to_owned(), ToolCallScope::SubagentRoot);
        app.insert_active_task(tool_id.to_owned());

        let update = model::RenderToolCallUpdate::new(
            tool_id,
            model::RenderToolCallUpdateFields::new().status(model::ToolCallStatus::Killed),
        );

        handle_tool_call_update_session(&mut app, &update);

        let MessageBlock::ToolCall(tc) = &app.messages()[0].blocks[0] else {
            panic!("expected tool call block");
        };
        assert_eq!(tc.status, model::ToolCallStatus::Killed);
        assert!(!app.active_task_ids().contains(tool_id));
    }
}
