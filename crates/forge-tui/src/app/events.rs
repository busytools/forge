mod api_retry;
mod client;
pub(crate) mod mouse;
mod notices;
pub(super) mod rate_limit;
mod sdk_message;
mod session;
mod session_reset;
mod streaming;
mod tool_calls;
mod tool_updates;
pub(crate) mod turn;

#[cfg(test)]
use super::TurnNoticeLocation;
use super::{
    ActiveView, App, AppStatus, ChatMessage, InvalidationLevel, MessageBlock, MessageRole,
    PendingCommandAck, SystemSeverity, TextBlock,
};
use crate::agent::model;
#[cfg(test)]
use crate::app::keys::{CMD_MOD, WORD_NAV_MOD};
#[cfg(test)]
use crossterm::event::KeyEvent;
use crossterm::event::{Event, KeyEventKind};

pub use client::apply_session_update;

/// Set the bucket's `lifecycle_state` for `key`. Reducer-side helper
/// used by the per-event handlers in this module tree (`session`,
/// `client`, `turn`) plus `app::input_submit`. No-op when no bucket
/// is registered for `key`.
///
/// Emits a `tracing::debug!` on every transition (including no-op
/// same-state writes) so the "Projects-pane spinner stops mid-turn"
/// flake has a trail when it next reproduces.
pub(crate) fn set_bucket_lifecycle_state(
    app: &mut App,
    key: &forge_workspace::SessionKey,
    state: crate::app::session::SessionLifecycleState,
) {
    if let Some(bucket) = app.sessions.get_mut(key) {
        let from = bucket.lifecycle_state;
        bucket.lifecycle_state = state;
        tracing::debug!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "session_lifecycle_transition",
            message = "session lifecycle state changed",
            outcome = "success",
            key = %key.as_str(),
            from = ?from,
            to = ?state,
        );
    }
}
pub fn handle_terminal_event(app: &mut App, event: Event) {
    let changed = match event {
        Event::Key(key) if should_dispatch_key_event(key) => dispatch_key_by_view(app, key),
        Event::Mouse(mouse) => {
            dispatch_mouse_by_view(app, mouse);
            true
        }
        Event::Paste(text) => dispatch_paste_by_view(app, &text),
        Event::FocusGained => {
            app.notifications.on_focus_gained();
            true
        }
        Event::FocusLost => {
            app.notifications.on_focus_lost();
            true
        }
        Event::Resize(width, height) => {
            handle_resize(app, width, height);
            true
        }
        // Non-press key events (Release, Repeat) -- ignored.
        Event::Key(_) => false,
    };
    app.needs_redraw |= changed;
}

fn should_dispatch_key_event(key: crossterm::event::KeyEvent) -> bool {
    key.kind == KeyEventKind::Press
        || (key.kind == KeyEventKind::Release && super::keys::is_clipboard_paste_shortcut(key))
}

fn handle_resize(app: &mut App, width: u16, height: u16) {
    // Force a full terminal clear on resize. Without this, terminal
    // emulators (especially on Windows) corrupt their scrollback buffer
    // when the alternate screen is resized, causing the visible area to
    // shift even though ratatui paints the correct content. The clear
    // resets the terminal's internal state.
    app.force_redraw = true;

    // Interaction-facing geometry is stale until the next frame computes the
    // new layout. Invalidate it immediately so mouse/selection logic cannot
    // keep using old hitboxes after a resize event.
    app.cached_frame_area = ratatui::layout::Rect::new(0, 0, width, height);
    app.rendered_chat_area = ratatui::layout::Rect::default();
    app.rendered_input_area = ratatui::layout::Rect::default();
    app.rendered_chat_lines.clear();
    app.rendered_input_lines.clear();
    *app.selection_mut() = None;
    app.scrollbar_drag = None;

    // The Narrow-tier Projects overlay is transient - its design
    // contract is "each launch starts closed" - so resetting on
    // resize matches the documented model. Without this, an overlay
    // opened at Narrow tier persists across a resize to Wide; later
    // Esc keypresses get consumed by the overlay-close handler
    // instead of cancelling a turn, and chat row clicks bleed into
    // overlay session-row hit targets stamped by a Narrow render.
    app.projects_pane_overlay_open = false;
    // Hit targets and the cached layout describe last-frame geometry;
    // both are stale across a resize and must be cleared so post-
    // resize hit-testing finds nothing until the next render fills
    // them in.
    app.pane_hit_targets.clear();
    app.layout = crate::ui::layout::AppLayout::default();

    crate::ui::help::sync_geometry_state(app, width);
}

fn dispatch_key_by_view(app: &mut App, key: crossterm::event::KeyEvent) -> bool {
    match app.active_view {
        ActiveView::Chat => {
            *app.active_paste_session_mut() = None;
            super::keys::dispatch_key_by_focus(app, key)
        }
        ActiveView::Plugins => {
            super::config::handle_plugins_key(app, key);
            true
        }
        ActiveView::Mcp => {
            super::config::handle_mcp_key(app, key);
            true
        }
        ActiveView::Launchpad => super::keys::dispatch_key_by_focus(app, key),
        ActiveView::Diff => {
            super::diff_overlay::handle_key(app, key);
            true
        }
    }
}

fn dispatch_mouse_by_view(app: &mut App, mouse: crossterm::event::MouseEvent) {
    match app.active_view {
        ActiveView::Chat => {
            *app.active_paste_session_mut() = None;
            mouse::handle_mouse_event(app, mouse);
        }
        ActiveView::Diff => {
            super::diff_overlay::handle_mouse(app, mouse);
        }
        // Plugins / MCP / Launchpad are keyboard-only - mouse
        // events are intentionally dropped.
        ActiveView::Plugins | ActiveView::Mcp | ActiveView::Launchpad => {}
    }
}

fn dispatch_paste_by_view(app: &mut App, text: &str) -> bool {
    match app.active_view {
        ActiveView::Chat => {
            if !matches!(
                app.status,
                AppStatus::Connecting | AppStatus::CommandPending | AppStatus::Error
            ) && !app.is_compacting()
            {
                app.queue_paste_text(text);
                return true;
            }
            false
        }
        ActiveView::Plugins => super::config::handle_plugins_paste(app, text),
        ActiveView::Mcp => super::config::handle_mcp_paste(app, text),
        ActiveView::Diff => super::diff_overlay::handle_paste(app, text),
        ActiveView::Launchpad => false,
    }
}

pub(super) fn apply_available_commands_update(app: &mut App, cmds: model::AvailableCommandsUpdate) {
    tracing::debug!(
        target: crate::logging::targets::APP_SESSION,
        event_name = "available_commands_applied",
        message = "available commands update applied",
        outcome = "success",
        command_count = cmds.available_commands.len(),
    );
    *app.available_commands_mut() = cmds.available_commands;
    crate::app::plugins::clamp_selection(app);
    if app.slash().is_some() {
        super::slash::update_query(app);
    }
}

pub(super) fn apply_available_agents_update(app: &mut App, agents: model::AvailableAgentsUpdate) {
    tracing::debug!(
        target: crate::logging::targets::APP_SESSION,
        event_name = "available_agents_applied",
        message = "available agents update applied",
        outcome = "success",
        agent_count = agents.available_agents.len(),
    );
    *app.available_agents_mut() = agents.available_agents;
    if app.subagent().is_some() {
        super::subagent::update_query(app);
    }
}

pub fn apply_mode_state_update(app: &mut App, mode: crate::app::ModeState) {
    let mode_changed = app.mode().map(|current| current.current_mode_id.as_str())
        != Some(mode.current_mode_id.as_str());
    app.set_mode(Some(mode));
    if mode_changed {
        app.invalidate_layout(InvalidationLevel::Global);
    }
    if matches!(app.pending_command_ack(), Some(PendingCommandAck::CurrentMode)) {
        session::clear_pending_command(app);
    }
}

pub fn apply_current_model_update(app: &mut App, current_model: model::CurrentModel) {
    let next_resolved_id = current_model.resolved_id.clone();
    let next_display_short = current_model.display_name_short.clone();
    let next_display_long = current_model.display_name_long.clone();
    let pending_ack_before = format!("{:?}", app.pending_command_ack());
    app.set_current_model(Some(current_model));
    let clearing_pending =
        matches!(app.pending_command_ack(), Some(PendingCommandAck::CurrentModel));
    if matches!(app.pending_command_ack(), Some(PendingCommandAck::CurrentModel)) {
        session::clear_pending_command(app);
    }
    tracing::debug!(
        target: crate::logging::targets::APP_SESSION,
        event_name = "current_model_update_applied",
        message = "current model update applied",
        outcome = "success",
        resolved_id = %next_resolved_id,
        display_name_short = %next_display_short,
        display_name_long = %next_display_long,
        clearing_pending = clearing_pending,
        pending_ack_before = %pending_ack_before,
    );
}

pub fn apply_current_mode_update(app: &mut App, update: &model::CurrentModeUpdate) {
    let mode_id = update.current_mode_id.to_string();
    let mut mode_changed = false;
    if let Some(mode) = app.mode_mut() {
        mode_changed = mode.current_mode_id != mode_id;
        if let Some(info) = mode.available_modes.iter().find(|m| m.id == mode_id) {
            mode.current_mode_name.clone_from(&info.name);
            mode.current_mode_id = mode_id;
        } else {
            mode.current_mode_name.clone_from(&mode_id);
            mode.current_mode_id = mode_id;
        }
    }
    if mode_changed {
        app.invalidate_layout(InvalidationLevel::Global);
    }
    if matches!(app.pending_command_ack(), Some(PendingCommandAck::CurrentMode)) {
        session::clear_pending_command(app);
    }
}

pub(super) fn apply_session_status_update(app: &mut App, status: model::SessionStatus) {
    // The CLI emits `status:"compacting"` as the first inbound frame
    // after a `/compact` user message and `status:null` when
    // compaction settles - verified against the sdk_compact wire
    // baseline. `is_compacting` is driven purely from this path; no
    // optimistic-set in `handle_compact_submit` is needed.
    let was_compacting = app.is_compacting();
    if matches!(status, model::SessionStatus::Compacting) {
        app.set_is_compacting(true);
    } else {
        clear_compaction_state(app, true);
    }
    if was_compacting && matches!(status, model::SessionStatus::Idle) {
        crate::app::session_runtime::request_context_usage_refresh(app);
    }
}

pub(super) fn handle_runtime_session_state_update(
    app: &mut App,
    state: model::RuntimeSessionState,
) {
    app.set_runtime_session_state(Some(state));
    match state {
        model::RuntimeSessionState::Running => {
            if matches!(app.status, AppStatus::Ready | AppStatus::Thinking | AppStatus::Running)
                && !app.is_compacting()
            {
                app.status = AppStatus::Running;
            }
        }
        model::RuntimeSessionState::RequiresAction => {}
        model::RuntimeSessionState::Idle => {
            if matches!(app.status, AppStatus::Thinking | AppStatus::Running)
                && !app.is_compacting()
                && !app.pending_cancel()
            {
                app.status = AppStatus::Ready;
            }
        }
    }
}

pub(super) fn handle_settings_parse_error(
    app: &mut App,
    file: Option<&str>,
    path: &str,
    message: &str,
) {
    let trimmed = message.trim();
    if trimmed.is_empty() {
        return;
    }
    let rendered = match (file.filter(|value| !value.trim().is_empty()), path.trim()) {
        (Some(file), "") => format!("Settings parse error in {file}: {trimmed}"),
        (Some(file), path) => format!("Settings parse error in {file} at {path}: {trimmed}"),
        (None, "") => format!("Settings parse error: {trimmed}"),
        (None, path) => format!("Settings parse error at {path}: {trimmed}"),
    };
    push_system_message_with_severity(app, Some(SystemSeverity::Error), &rendered);
}

pub(crate) fn push_system_message_with_severity(
    app: &mut App,
    severity: Option<SystemSeverity>,
    message: &str,
) {
    app.push_message_tracked(ChatMessage::new(
        MessageRole::System(severity),
        vec![MessageBlock::Text(TextBlock::from_complete(message))],
        None,
    ));
    app.enforce_history_retention_tracked();
    app.active_viewport_mut().engage_auto_scroll();
}

pub(super) fn clear_compaction_state(app: &mut App, emit_manual_success: bool) {
    if !app.is_compacting() && !app.pending_compact_clear() {
        return;
    }
    let should_emit_success = emit_manual_success && app.pending_compact_clear();
    app.set_pending_compact_clear(false);
    app.set_is_compacting(false);
    if should_emit_success {
        push_system_message_with_severity(
            app,
            Some(SystemSeverity::Info),
            "Session successfully compacted.",
        );
    }
}

#[cfg(test)]
fn handle_normal_key(app: &mut App, key: KeyEvent) {
    super::keys::handle_normal_key(app, key);
}

#[cfg(test)]
fn handle_mention_key(app: &mut App, key: KeyEvent) {
    super::keys::handle_mention_key(app, key);
}

#[cfg(test)]
fn dispatch_key_by_focus(app: &mut App, key: KeyEvent) {
    super::keys::dispatch_key_by_focus(app, key);
}

#[cfg(test)]
mod tests {
    // =====
    // TESTS: 40
    // =====

    use super::*;
    use crate::app::{
        ActiveView, BlockCache, HelpView, SelectionKind, SelectionPoint, SelectionState,
        TextBlockSpacing, TodoItem, TodoStatus, ToolCallInfo, ToolCallScope, UsageSnapshot,
        UsageSourceKind, mention,
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
    use forge_primitives::cloud::service_status::ServiceSeverity;
    use forge_workspace::SessionUpdate;
    use pretty_assertions::assert_eq;
    use ratatui::layout::Rect;

    use std::time::{Duration, Instant};

    /// Helper: synthetic [`forge_workspace::SessionKey`] used to
    /// tag `ClientEvent`s emitted by tests. Tests built around
    /// `App::test_default` always have one bucket keyed by
    /// `App::PRE_CONNECT_KEY`, and tests that swap session ids in
    /// (via `App::set_session_id`) migrate the bucket onto the new
    /// key - but for tagging synthetic events both forms route
    /// through the active-session matcher in
    /// [`super::handle_client_event`], so the pre-Connected key is
    /// what the multiplexer expects when no real
    /// Connect/SessionReplaced has flowed through yet.
    fn active_session_key(app: &App) -> forge_workspace::SessionKey {
        app.active_session_key
            .clone()
            .unwrap_or_else(|| forge_workspace::SessionKey::from_session_id(App::PRE_CONNECT_KEY))
    }

    // Helper: build a minimal ToolCallInfo with given id + status

    fn tool_call(id: &str, status: model::ToolCallStatus) -> ToolCallInfo {
        ToolCallInfo {
            id: id.into(),
            title: id.into(),
            sdk_tool_name: "Read".into(),
            raw_input: None,
            raw_input_bytes: 0,
            output_metadata: None,
            task_metadata: None,
            status,
            content: vec![],
            hidden: false,
            terminal_id: None,
            terminal_command: None,
            terminal_output: None,
            terminal_output_len: 0,
            terminal_bytes_seen: 0,
            terminal_snapshot_mode: crate::app::TerminalSnapshotMode::AppendOnly,
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

    fn assistant_msg(blocks: Vec<MessageBlock>) -> ChatMessage {
        ChatMessage::new(MessageRole::Assistant, blocks, None)
    }

    fn append_tool_call_block(app: &mut App, tool_id: &str) -> (usize, usize) {
        app.active_messages_mut().push(assistant_msg(vec![MessageBlock::ToolCall(Box::new(
            tool_call(tool_id, model::ToolCallStatus::InProgress),
        ))]));
        let msg_idx = app.messages().len().saturating_sub(1);
        app.index_tool_call(tool_id.into(), msg_idx, 0);
        (msg_idx, 0)
    }

    fn user_msg(text: &str) -> ChatMessage {
        ChatMessage::new(
            MessageRole::User,
            vec![MessageBlock::Text(TextBlock::from_complete(text))],
            None,
        )
    }

    fn first_block_text(msg: &ChatMessage) -> &str {
        match msg.blocks.first() {
            Some(MessageBlock::Text(block)) => &block.text,
            Some(MessageBlock::Notice(block)) => &block.text.text,
            Some(MessageBlock::ToolCall(_)) => panic!("expected text-like block, found tool call"),
            Some(MessageBlock::Welcome(_)) => panic!("expected text-like block, found welcome"),
            Some(MessageBlock::ImageAttachment(_)) => {
                panic!("expected text-like block, found image attachment")
            }
            None => panic!("expected message block"),
        }
    }

    // shorten_tool_title

    #[test]
    fn shorten_unix_path() {
        let result = tool_calls::shorten_tool_title(
            "Read /home/user/project/src/main.rs",
            "/home/user/project",
        );
        assert_eq!(result, "Read src/main.rs");
    }

    #[test]
    fn register_tool_call_scope_treats_agent_as_subagent_root() {
        let mut app = make_test_app();
        let scope = tool_calls::register_tool_call_scope(&mut app, "tool-agent", "Agent", None);
        assert_eq!(scope, ToolCallScope::SubagentRoot);
    }

    #[test]
    fn register_tool_call_scope_treats_task_as_subagent_root() {
        let mut app = make_test_app();
        let scope = tool_calls::register_tool_call_scope(&mut app, "tool-task", "Task", None);
        assert_eq!(scope, ToolCallScope::SubagentRoot);
    }

    #[test]
    fn register_tool_call_scope_uses_explicit_parent_for_subagent_child() {
        let mut app = make_test_app();
        let scope = tool_calls::register_tool_call_scope(
            &mut app,
            "tool-child",
            "Bash",
            Some("tool-parent"),
        );
        assert_eq!(
            scope,
            ToolCallScope::SubagentChild { parent_tool_use_id: "tool-parent".to_owned() }
        );
    }

    /// Regression: when a Task was cancelled mid-turn, `active_task_ids` was never cleared
    /// because `finalize_in_progress_tool_calls` doesn't call `remove_active_task` and
    /// `clear_tool_scope_tracking` (called on `TurnComplete`) did not clear `active_task_ids`.
    /// The leaked ID caused main-agent tools on the next turn to be classified as Subagent,
    /// which eventually caused main-agent tools to inherit the wrong scope.
    #[test]
    fn turn_complete_after_cancelled_task_leaves_no_stale_active_task_ids() {
        let mut app = make_test_app();

        // Simulate a Task tool call arriving as in-progress (no Completed update will follow)
        send_msg(
            &mut app,
            assistant_message(vec![tool_use_block(
                "task-1",
                "Task",
                serde_json::json!({"description": "Research"}),
            )]),
        );
        assert!(app.active_task_ids().contains("task-1"), "task must be tracked while InProgress");

        // User cancels then TurnComplete finalizes the turn
        let session_key = active_session_key(&app);
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::TurnCancelled { key: session_key },
        );
        let session_key = active_session_key(&app);
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::TurnComplete {
                key: session_key,
                terminal_reason: None,
            },
        );

        // Stale task ID must be gone after turn boundary
        assert!(app.active_task_ids().is_empty(), "stale task id must not survive TurnComplete");

        // Next turn: a normal main-agent Glob must get MainAgent scope, not Subagent
        send_msg(
            &mut app,
            assistant_message(vec![tool_use_block(
                "glob-1",
                "Glob",
                serde_json::json!({"pattern": "**/*.rs"}),
            )]),
        );
        assert_eq!(
            app.tool_call_scope("glob-1"),
            Some(ToolCallScope::MainAgent),
            "main-agent tool must not be misclassified as Subagent after stale task is cleared"
        );
    }

    #[test]
    fn shorten_windows_path() {
        let result = tool_calls::shorten_tool_title(
            "Read C:\\Users\\me\\project\\src\\main.rs",
            "C:\\Users\\me\\project",
        );
        assert_eq!(result, "Read src/main.rs");
    }

    #[test]
    fn shorten_no_match_returns_original() {
        let result =
            tool_calls::shorten_tool_title("Read /other/path/file.rs", "/home/user/project");
        assert_eq!(result, "Read /other/path/file.rs");
    }

    // shorten_tool_title

    #[test]
    fn shorten_empty_cwd() {
        let result = tool_calls::shorten_tool_title("Read /some/path/file.rs", "");
        assert_eq!(result, "Read /some/path/file.rs");
    }

    #[test]
    fn shorten_cwd_with_trailing_slash() {
        let result = tool_calls::shorten_tool_title(
            "Read /home/user/project/file.rs",
            "/home/user/project/",
        );
        assert_eq!(result, "Read file.rs");
    }

    #[test]
    fn shorten_title_is_just_path() {
        let result =
            tool_calls::shorten_tool_title("/home/user/project/file.rs", "/home/user/project");
        assert_eq!(result, "file.rs");
    }

    #[test]
    fn shorten_mixed_separators() {
        let result = tool_calls::shorten_tool_title(
            "Read C:/Users/me/project/src/lib.rs",
            "C:\\Users\\me\\project",
        );
        assert_eq!(result, "Read src/lib.rs");
    }

    #[test]
    fn shorten_empty_title() {
        assert_eq!(tool_calls::shorten_tool_title("", "/some/cwd"), "");
    }

    #[test]
    fn shorten_title_no_path_at_all() {
        assert_eq!(tool_calls::shorten_tool_title("Read", "/home/user"), "Read");
        assert_eq!(tool_calls::shorten_tool_title("Write something", "/proj"), "Write something");
    }

    #[test]
    fn shorten_title_equals_cwd_exactly() {
        // Title IS the cwd path - after stripping, nothing left
        let result = tool_calls::shorten_tool_title("/home/user/project", "/home/user/project");
        // The cwd+/ won't match because title doesn't have trailing content after cwd
        // cwd_norm = "/home/user/project/", title doesn't contain that
        assert_eq!(result, "/home/user/project");
    }

    // shorten_tool_title

    #[test]
    fn shorten_partial_match_no_false_positive() {
        let result = tool_calls::shorten_tool_title("Read /home/username/file.rs", "/home/user");
        assert_eq!(result, "Read /home/username/file.rs");
    }

    #[test]
    fn shorten_deeply_nested_path() {
        let cwd = "/a/b/c/d/e/f/g";
        let title = "Read /a/b/c/d/e/f/g/h/i/j.rs";
        let result = tool_calls::shorten_tool_title(title, cwd);
        assert_eq!(result, "Read h/i/j.rs");
    }

    #[test]
    fn shorten_cwd_appears_multiple_times() {
        let result = tool_calls::shorten_tool_title("Diff /proj/a.rs /proj/b.rs", "/proj");
        assert_eq!(result, "Diff a.rs b.rs");
    }

    /// Spaces in path (real Windows path with spaces).
    #[test]
    fn shorten_spaces_in_path() {
        let result = tool_calls::shorten_tool_title(
            "Read C:\\Users\\user\\Desktop\\project\\src\\main.rs",
            "C:\\Users\\user\\Desktop\\project",
        );
        assert_eq!(result, "Read src/main.rs");
    }

    /// Unicode characters in path components.
    #[test]
    fn shorten_unicode_in_path() {
        let result = tool_calls::shorten_tool_title(
            "Read /home/\u{00FC}ser/\u{30D7}\u{30ED}\u{30B8}\u{30A7}\u{30AF}\u{30C8}/src/lib.rs",
            "/home/\u{00FC}ser/\u{30D7}\u{30ED}\u{30B8}\u{30A7}\u{30AF}\u{30C8}",
        );
        assert_eq!(result, "Read src/lib.rs");
    }

    /// Root as cwd (Unix).
    #[test]
    fn shorten_cwd_is_root_unix() {
        // cwd = "/" => with_sep = "/", so "/foo/bar.rs".contains("/") => replaces
        let result = tool_calls::shorten_tool_title("Read /foo/bar.rs", "/");
        // "/" is first path component = "" (empty), heuristic check uses "" which is in everything
        // After normalization: cwd = "/", with_sep = "/", title contains "/" => replaces ALL "/"
        assert_eq!(result, "Read foobar.rs");
    }

    /// Root as cwd (Windows).
    #[test]
    fn shorten_cwd_is_drive_root_windows() {
        let result = tool_calls::shorten_tool_title("Read C:\\src\\main.rs", "C:\\");
        assert_eq!(result, "Read src/main.rs");
    }

    /// Very long path (stress test).
    #[test]
    fn shorten_very_long_path() {
        let segments: String = (0..50).fold(String::new(), |mut s, i| {
            use std::fmt::Write;
            write!(s, "/seg{i}").unwrap();
            s
        });
        let cwd = segments.clone();
        let title = format!("Read {segments}/deep/file.rs");
        let result = tool_calls::shorten_tool_title(&title, &cwd);
        assert_eq!(result, "Read deep/file.rs");
    }

    /// Case sensitivity: paths are case-sensitive.
    #[test]
    fn shorten_case_sensitive() {
        let result =
            tool_calls::shorten_tool_title("Read /Home/User/Project/file.rs", "/home/user/project");
        // Different case, so the first-component heuristic "home" matches "Home"?
        // No: cwd_start = "home", title doesn't contain "home" (has "Home") => early return
        assert_eq!(result, "Read /Home/User/Project/file.rs");
    }

    /// Cwd that is a prefix at directory boundary but not at cwd boundary.
    #[test]
    fn shorten_cwd_prefix_boundary() {
        // cwd="/pro" should NOT strip from "/project/file.rs"
        let result = tool_calls::shorten_tool_title("Read /project/file.rs", "/pro");
        // cwd_start = "pro", title contains "pro" (in "project") => proceeds to normalize
        // with_sep = "/pro/", title_norm = "Read /project/file.rs", doesn't contain "/pro/"
        assert_eq!(result, "Read /project/file.rs");
    }

    #[test]
    fn split_index_prefers_double_newline() {
        let text = "first\n\nsecond";
        let split_at = streaming::find_text_block_split_index(text);
        assert_eq!(split_at, Some("first\n\n".len()));
    }

    #[test]
    fn split_index_soft_limit_prefers_newline() {
        use super::super::default_cache_split_policy;
        let prefix = "a".repeat(default_cache_split_policy().soft_limit_bytes - 1);
        let text = format!("{prefix}\n{}", "b".repeat(32));
        let split_at = streaming::find_text_block_split_index(&text).expect("expected split index");
        assert_eq!(&text[..split_at], format!("{prefix}\n"));
    }

    #[test]
    fn split_index_hard_limit_uses_sentence_when_needed() {
        use super::super::default_cache_split_policy;
        let prefix = "a".repeat(default_cache_split_policy().hard_limit_bytes + 32);
        let text = format!("{prefix}. tail");
        let split_at = streaming::find_text_block_split_index(&text).expect("expected split index");
        assert_eq!(&text[..split_at], format!("{prefix}."));
    }

    #[test]
    fn split_index_ignores_double_newline_inside_code_fence() {
        let text = "```\nline1\n\nline2\n```";
        assert!(streaming::find_text_block_split_index(text).is_none());
    }

    #[test]
    fn agent_message_chunk_splits_into_frozen_text_blocks() {
        let mut app = make_test_app();
        send_msg(&mut app, assistant_message(vec![text_block("p1\n\np2\n\np3")]));

        assert_eq!(app.messages().len(), 1);
        let Some(last) = app.messages().last() else {
            panic!("missing assistant message");
        };
        assert!(matches!(last.role, MessageRole::Assistant));
        assert_eq!(last.blocks.len(), 3);
        let Some(MessageBlock::Text(b1)) = last.blocks.first() else {
            panic!("expected first text block");
        };
        let Some(MessageBlock::Text(b2)) = last.blocks.get(1) else {
            panic!("expected second text block");
        };
        let Some(MessageBlock::Text(b3)) = last.blocks.get(2) else {
            panic!("expected third text block");
        };
        assert_eq!(b1.text, "p1\n\n");
        assert_eq!(b2.text, "p2\n\n");
        assert_eq!(b3.text, "p3");
        assert_eq!(b1.trailing_spacing, TextBlockSpacing::ParagraphBreak);
        assert_eq!(b2.trailing_spacing, TextBlockSpacing::ParagraphBreak);
        assert_eq!(b3.trailing_spacing, TextBlockSpacing::None);
    }

    // has_in_progress_tool_calls

    fn make_test_app() -> App {
        App::test_default()
    }

    fn test_current_model(model_name: &str) -> model::CurrentModel {
        model::CurrentModel::new(model_name, model_name, model_name).authoritative(true)
    }

    fn connected_event(model_name: &str) -> SessionUpdate {
        SessionUpdate::Connected {
            key: forge_workspace::SessionKey::from_session_id("test-session".to_owned()),
            session_id: forge_primitives::SessionId::new("test-session"),
            cwd: "/test".into(),
            current_model: test_current_model_primitives(model_name),
            available_models: Vec::new(),
            mode: None,
            history: Vec::new(),
        }
    }

    fn test_current_model_primitives(model_name: &str) -> forge_primitives::CurrentModel {
        forge_primitives::CurrentModel {
            resolved_id: model_name.to_owned(),
            display_name_short: model_name.to_owned(),
            display_name_long: model_name.to_owned(),
            requested_id: None,
            catalog_id: None,
            supports_effort: false,
            supported_effort_levels: Vec::new(),
            supports_fast_mode: None,
            supports_auto_mode: None,
            supports_adaptive_thinking: None,
            is_authoritative: true,
        }
    }

    fn user_text_message(text: &str) -> forge_primitives::Message {
        forge_primitives::Message::User {
            message: forge_primitives::UserEnvelope {
                role: "user".to_owned(),
                content: vec![forge_primitives::ContentBlock::Text { text: text.to_owned() }],
            },
            session_id: String::new(),
            parent_tool_use_id: None,
            uuid: None,
            tool_use_result: None,
        }
    }

    fn assistant_text_message(text: &str) -> forge_primitives::Message {
        forge_primitives::Message::Assistant {
            message: forge_primitives::AssistantEnvelope {
                id: "msg_test".to_owned(),
                role: "assistant".to_owned(),
                model: "claude-test".to_owned(),
                content: vec![forge_primitives::ContentBlock::Text { text: text.to_owned() }],
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

    fn assistant_tool_use_message(
        tool_use_id: &str,
        name: &str,
        input: serde_json::Value,
    ) -> forge_primitives::Message {
        forge_primitives::Message::Assistant {
            message: forge_primitives::AssistantEnvelope {
                id: "msg_test".to_owned(),
                role: "assistant".to_owned(),
                model: "claude-test".to_owned(),
                content: vec![forge_primitives::ContentBlock::ToolUse {
                    id: tool_use_id.to_owned(),
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

    fn app_with_bridge_connection()
    -> (App, tokio::sync::mpsc::UnboundedReceiver<forge_primitives::AgentCommand>) {
        let mut app = make_test_app();
        let rx = app.install_testing_stub();
        (app, rx)
    }

    // Wire-message helpers for the SdkMessageReceived path. Mirror the
    // shared `tests/integration/message_helpers.rs` versions; each
    // compilation unit needs its own copy since inline `#[test]` modules
    // can't import test-only modules from the `tests/` dir.

    fn assistant_message(
        content: Vec<forge_primitives::ContentBlock>,
    ) -> forge_primitives::Message {
        forge_primitives::Message::Assistant {
            message: forge_primitives::AssistantEnvelope {
                id: "msg_test".to_owned(),
                role: "assistant".to_owned(),
                model: "claude-test".to_owned(),
                content,
                stop_reason: None,
                stop_sequence: None,
                usage: None,
            },
            session_id: "test-session".to_owned(),
            parent_tool_use_id: None,
            error: None,
            uuid: None,
        }
    }

    fn user_message(content: Vec<forge_primitives::ContentBlock>) -> forge_primitives::Message {
        forge_primitives::Message::User {
            message: forge_primitives::UserEnvelope { role: "user".to_owned(), content },
            session_id: "test-session".to_owned(),
            parent_tool_use_id: None,
            uuid: None,
            tool_use_result: None,
        }
    }

    fn system_message(subtype: &str, data: serde_json::Value) -> forge_primitives::Message {
        forge_primitives::Message::System {
            subtype: subtype.to_owned(),
            session_id: Some("test-session".to_owned()),
            data,
        }
    }

    fn rate_limit_event(
        rate_limit_info: forge_primitives::RateLimitInfo,
    ) -> forge_primitives::Message {
        forge_primitives::Message::RateLimitEvent {
            rate_limit_info,
            uuid: "rl_test".to_owned(),
            session_id: "test-session".to_owned(),
        }
    }

    /// Build a wire `RateLimitInfo` whose serialised JSON drives the
    /// `build_rate_limit_update` parser exactly as the original
    /// `model::RateLimitUpdate` shape did.
    ///
    /// `rate_limit_type` is taken as a `&str` and routed through the
    /// flattened `raw` map so non-enum-typed values like `"daily"` (used
    /// by some tests) pass through to the model side as `Some(string)`.
    /// `is_using_overage` and `surpassed_threshold` go through the same
    /// raw map as `isUsingOverage` / `surpassedThreshold` (camelCase keys
    /// the build_rate_limit_update parser reads directly).
    fn build_rate_limit_info(
        status: forge_primitives::RateLimitStatus,
        resets_at_secs: Option<i64>,
        utilization: Option<f64>,
        rate_limit_type: Option<&str>,
        is_using_overage: Option<bool>,
        surpassed_threshold: Option<f64>,
    ) -> forge_primitives::RateLimitInfo {
        let mut raw = serde_json::Map::new();
        if let Some(v) = rate_limit_type {
            raw.insert("rateLimitType".to_owned(), serde_json::json!(v));
        }
        if let Some(v) = is_using_overage {
            raw.insert("isUsingOverage".to_owned(), serde_json::json!(v));
        }
        if let Some(v) = surpassed_threshold {
            raw.insert("surpassedThreshold".to_owned(), serde_json::json!(v));
        }
        forge_primitives::RateLimitInfo {
            status,
            resets_at: resets_at_secs,
            rate_limit_type: None,
            utilization,
            overage_status: None,
            overage_resets_at: None,
            overage_disabled_reason: None,
            raw,
        }
    }

    fn tool_use_block(
        id: &str,
        name: &str,
        input: serde_json::Value,
    ) -> forge_primitives::ContentBlock {
        forge_primitives::ContentBlock::ToolUse { id: id.to_owned(), name: name.to_owned(), input }
    }

    fn text_block(text: &str) -> forge_primitives::ContentBlock {
        forge_primitives::ContentBlock::Text { text: text.to_owned() }
    }

    fn tool_result_block(
        tool_use_id: &str,
        content: serde_json::Value,
    ) -> forge_primitives::ContentBlock {
        forge_primitives::ContentBlock::ToolResult {
            tool_use_id: tool_use_id.to_owned(),
            content,
            is_error: false,
        }
    }

    /// Dispatch a wire `Message` envelope as `SdkMessageReceived`,
    /// adopting `"test-session"` as the app's session id on first use
    /// so the session-id guard in the dispatcher accepts the envelope.
    /// If the app already has a session id set, the wire envelope is
    /// dispatched with that id so the strict-mismatch check passes.
    fn send_msg(app: &mut App, msg: forge_primitives::Message) {
        if app.session_id().is_none() {
            app.set_session_id(Some(model::SessionId::new("test-session")));
        }
        let session_id =
            app.session_id().map_or_else(|| "test-session".to_owned(), |s| s.to_string());
        apply_session_update(app, forge_workspace::SessionUpdate::ChatAppended { session_id, msg });
    }

    #[test]
    fn raw_output_string_maps_to_terminal_text() {
        let raw = serde_json::json!("hello\nworld");
        assert_eq!(
            tool_updates::raw_output_to_terminal_text(&raw).as_deref(),
            Some("hello\nworld")
        );
    }

    #[test]
    fn raw_output_text_array_maps_to_terminal_text() {
        let raw = serde_json::json!([
            {"type": "text", "text": "first"},
            {"type": "text", "text": "second"}
        ]);
        assert_eq!(
            tool_updates::raw_output_to_terminal_text(&raw).as_deref(),
            Some("first\nsecond")
        );
    }

    #[test]
    fn execute_tool_update_uses_raw_output_fallback() {
        let mut app = make_test_app();
        send_msg(
            &mut app,
            assistant_message(vec![tool_use_block(
                "tc-exec",
                "Bash",
                serde_json::json!({"command": "echo line 1"}),
            )]),
        );

        send_msg(
            &mut app,
            user_message(vec![tool_result_block("tc-exec", serde_json::json!("line 1\nline 2"))]),
        );

        let Some((mi, bi)) = app.lookup_tool_call("tc-exec") else {
            panic!("tool call not indexed");
        };
        let Some(MessageBlock::ToolCall(tc)) =
            app.messages().get(mi).and_then(|m| m.blocks.get(bi))
        else {
            panic!("tool call block missing");
        };
        assert_eq!(tc.terminal_output.as_deref(), Some("line 1\nline 2"));
    }

    #[test]
    fn late_tool_update_for_removed_tool_does_not_corrupt_active_task_set() {
        let mut app = make_test_app();
        app.set_session_id(Some(model::SessionId::new("test-session")));
        app.active_messages_mut().push(assistant_msg(vec![MessageBlock::ToolCall(Box::new(
            tool_call("tool-stale", model::ToolCallStatus::Completed),
        ))]));
        app.index_tool_call("tool-stale".into(), 0, 0);
        app.register_tool_call_scope(
            "tool-stale".into(),
            ToolCallScope::SubagentChild { parent_tool_use_id: "task-1".to_owned() },
        );

        let removed = app.remove_message_tracked(0);
        assert!(removed.is_some());
        assert_eq!(app.tool_call_scope("tool-stale"), None);

        // Send a tool_use re-emit for the removed tool - the wire path
        // attempts to reopen it as in_progress and the code must not
        // resurrect an entry in `active_task_ids` (the original test
        // exercised this via SessionUpdate::ToolCallUpdate).
        send_msg(
            &mut app,
            assistant_message(vec![tool_use_block(
                "tool-stale",
                "Read",
                serde_json::json!({"file_path": "stale.rs"}),
            )]),
        );

        assert!(app.active_task_ids().is_empty());
    }

    #[test]
    fn tool_call_update_noop_does_not_bump_epochs() {
        let mut app = make_test_app();
        send_msg(
            &mut app,
            assistant_message(vec![tool_use_block(
                "tc-noop",
                "Read",
                serde_json::json!({"file_path": "noop.rs"}),
            )]),
        );

        let (mi, bi) = app.lookup_tool_call("tc-noop").expect("tool call not indexed");
        let (before_render, before_layout, before_oldest_stale) = {
            let MessageBlock::ToolCall(tc) = &app.messages()[mi].blocks[bi] else {
                panic!("tool call block missing");
            };
            (tc.render_epoch, tc.layout_epoch, app.active_viewport_mut().oldest_stale_index())
        };

        // Re-send the same tool_use envelope. The wire path keeps the
        // tool_call open with the same in_progress status - assert the
        // re-emit doesn't bump any cache invalidation epochs.
        send_msg(
            &mut app,
            assistant_message(vec![tool_use_block(
                "tc-noop",
                "Read",
                serde_json::json!({"file_path": "noop.rs"}),
            )]),
        );

        let MessageBlock::ToolCall(tc) = &app.messages()[mi].blocks[bi] else {
            panic!("tool call block missing");
        };
        assert_eq!(tc.render_epoch, before_render);
        assert_eq!(tc.layout_epoch, before_layout);
        assert_eq!(app.active_viewport_mut().oldest_stale_index(), before_oldest_stale);
    }

    #[test]
    fn has_in_progress_empty_messages() {
        let app = make_test_app();
        assert!(!tool_calls::has_in_progress_tool_calls(&app));
    }

    #[test]
    fn has_in_progress_no_tool_calls() {
        let mut app = make_test_app();
        app.active_messages_mut()
            .push(assistant_msg(vec![MessageBlock::Text(TextBlock::from_complete("hello"))]));
        assert!(!tool_calls::has_in_progress_tool_calls(&app));
    }

    #[test]
    fn has_in_progress_with_pending_tool() {
        let mut app = make_test_app();
        app.active_messages_mut().push(assistant_msg(vec![MessageBlock::ToolCall(Box::new(
            tool_call("tc1", model::ToolCallStatus::Pending),
        ))]));
        app.bind_active_turn_assistant_to_tail();
        assert!(tool_calls::has_in_progress_tool_calls(&app));
    }

    #[test]
    fn has_in_progress_with_in_progress_tool() {
        let mut app = make_test_app();
        app.active_messages_mut().push(assistant_msg(vec![MessageBlock::ToolCall(Box::new(
            tool_call("tc1", model::ToolCallStatus::InProgress),
        ))]));
        app.bind_active_turn_assistant_to_tail();
        assert!(tool_calls::has_in_progress_tool_calls(&app));
    }

    #[test]
    fn has_in_progress_all_completed() {
        let mut app = make_test_app();
        app.active_messages_mut().push(assistant_msg(vec![MessageBlock::ToolCall(Box::new(
            tool_call("tc1", model::ToolCallStatus::Completed),
        ))]));
        assert!(!tool_calls::has_in_progress_tool_calls(&app));
    }

    #[test]
    fn has_in_progress_all_failed() {
        let mut app = make_test_app();
        app.active_messages_mut().push(assistant_msg(vec![MessageBlock::ToolCall(Box::new(
            tool_call("tc1", model::ToolCallStatus::Failed),
        ))]));
        assert!(!tool_calls::has_in_progress_tool_calls(&app));
    }

    // has_in_progress_tool_calls

    #[test]
    fn has_in_progress_user_message_last() {
        let mut app = make_test_app();
        app.active_messages_mut().push(user_msg("hi"));
        assert!(!tool_calls::has_in_progress_tool_calls(&app));
    }

    /// Without an explicit owner, in-progress tools do not count even if the last assistant has them.
    #[test]
    fn has_in_progress_requires_explicit_owner() {
        let mut app = make_test_app();
        app.active_messages_mut().push(assistant_msg(vec![MessageBlock::ToolCall(Box::new(
            tool_call("tc1", model::ToolCallStatus::InProgress),
        ))]));
        app.active_messages_mut().push(user_msg("thanks"));
        assert!(!tool_calls::has_in_progress_tool_calls(&app));
    }

    /// The owned assistant decides the result even when another assistant trails later.
    #[test]
    fn has_in_progress_uses_owned_assistant_not_latest_assistant() {
        let mut app = make_test_app();
        app.active_messages_mut().push(assistant_msg(vec![MessageBlock::ToolCall(Box::new(
            tool_call("tc1", model::ToolCallStatus::InProgress),
        ))]));
        app.active_messages_mut().push(user_msg("ok"));
        app.active_messages_mut().push(assistant_msg(vec![MessageBlock::ToolCall(Box::new(
            tool_call("tc2", model::ToolCallStatus::Completed),
        ))]));
        app.bind_active_turn_assistant(0);
        assert!(tool_calls::has_in_progress_tool_calls(&app));
    }

    #[test]
    fn has_in_progress_mixed_completed_and_pending() {
        let mut app = make_test_app();
        app.active_messages_mut().push(assistant_msg(vec![
            MessageBlock::ToolCall(Box::new(tool_call("tc1", model::ToolCallStatus::Completed))),
            MessageBlock::ToolCall(Box::new(tool_call("tc2", model::ToolCallStatus::InProgress))),
        ]));
        app.bind_active_turn_assistant_to_tail();
        assert!(tool_calls::has_in_progress_tool_calls(&app));
    }

    /// Text blocks mixed with tool calls - text blocks are correctly skipped.
    #[test]
    fn has_in_progress_text_and_tools_mixed() {
        let mut app = make_test_app();
        app.active_messages_mut().push(assistant_msg(vec![
            MessageBlock::Text(TextBlock::from_complete("thinking...")),
            MessageBlock::ToolCall(Box::new(tool_call("tc1", model::ToolCallStatus::Completed))),
            MessageBlock::Text(TextBlock::from_complete("done")),
        ]));
        assert!(!tool_calls::has_in_progress_tool_calls(&app));
    }

    /// Stress: 100 completed tool calls + 1 pending at the end.
    #[test]
    fn has_in_progress_stress_100_tools_one_pending() {
        let mut app = make_test_app();
        let mut blocks: Vec<MessageBlock> = (0..100)
            .map(|i| {
                MessageBlock::ToolCall(Box::new(tool_call(
                    &format!("tc{i}"),
                    model::ToolCallStatus::Completed,
                )))
            })
            .collect();
        blocks.push(MessageBlock::ToolCall(Box::new(tool_call(
            "tc_pending",
            model::ToolCallStatus::Pending,
        ))));
        app.active_messages_mut().push(assistant_msg(blocks));
        app.bind_active_turn_assistant_to_tail();
        assert!(tool_calls::has_in_progress_tool_calls(&app));
    }

    /// Stress: 100 completed tool calls, none pending.
    #[test]
    fn has_in_progress_stress_100_tools_all_done() {
        let mut app = make_test_app();
        let blocks: Vec<MessageBlock> = (0..100)
            .map(|i| {
                MessageBlock::ToolCall(Box::new(tool_call(
                    &format!("tc{i}"),
                    model::ToolCallStatus::Completed,
                )))
            })
            .collect();
        app.active_messages_mut().push(assistant_msg(blocks));
        assert!(!tool_calls::has_in_progress_tool_calls(&app));
    }

    /// Mix of Failed and Completed - neither counts as in-progress.
    #[test]
    fn has_in_progress_failed_and_completed_mix() {
        let mut app = make_test_app();
        app.active_messages_mut().push(assistant_msg(vec![
            MessageBlock::ToolCall(Box::new(tool_call("tc1", model::ToolCallStatus::Completed))),
            MessageBlock::ToolCall(Box::new(tool_call("tc2", model::ToolCallStatus::Failed))),
            MessageBlock::ToolCall(Box::new(tool_call("tc3", model::ToolCallStatus::Completed))),
        ]));
        assert!(!tool_calls::has_in_progress_tool_calls(&app));
    }

    /// Empty assistant message (no blocks at all).
    #[test]
    fn has_in_progress_empty_assistant_blocks() {
        let mut app = make_test_app();
        app.active_messages_mut().push(assistant_msg(vec![]));
        assert!(!tool_calls::has_in_progress_tool_calls(&app));
    }

    // make_test_app - verify defaults

    #[test]
    fn test_app_defaults() {
        let app = make_test_app();
        assert!(app.messages().is_empty());
        assert_eq!(app.viewport().scroll_offset, 0);
        assert_eq!(app.viewport().scroll_target, 0);
        assert!(app.viewport().auto_scroll);
        assert!(!app.should_quit);
        assert!(app.session_id().is_none());
        assert_eq!(app.files_accessed(), 0);
        assert!(app.tools_collapsed);
        assert!(!app.force_redraw);
        assert!(app.todos().is_empty());
        assert!(app.selection().is_none());
        assert!(app.mention().is_none());
        assert!(!app.cancelled_turn_pending_hint());
        assert!(app.rendered_chat_lines.is_empty());
        assert!(app.rendered_input_lines.is_empty());
        assert!(matches!(app.status, AppStatus::Ready));
    }

    #[test]
    fn turn_complete_after_cancel_renders_interrupted_hint() {
        let mut app = make_test_app();

        let session_key = active_session_key(&app);
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::TurnCancelled { key: session_key },
        );
        assert!(app.cancelled_turn_pending_hint());

        let session_key = active_session_key(&app);
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::TurnComplete {
                key: session_key,
                terminal_reason: None,
            },
        );

        assert!(!app.cancelled_turn_pending_hint());
        let last = app.messages().last().expect("expected interruption hint message");
        assert!(matches!(last.role, MessageRole::System(Some(SystemSeverity::Info))));
        let Some(MessageBlock::Text(block)) = last.blocks.first() else {
            panic!("expected text block");
        };
        assert_eq!(block.text, "Conversation interrupted. Tell the model how to proceed.");
    }

    #[test]
    fn turn_complete_after_manual_cancel_marks_tail_assistant_layout_dirty() {
        let mut app = make_test_app();
        app.status = AppStatus::Thinking;
        app.active_messages_mut().push(user_msg("build app"));
        app.active_messages_mut().push(assistant_msg(vec![MessageBlock::Text(
            TextBlock::from_complete("partial output"),
        )]));
        app.set_pending_cancel(true);

        let session_key = active_session_key(&app);
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::TurnComplete {
                key: session_key,
                terminal_reason: None,
            },
        );

        assert!(matches!(app.status, AppStatus::Ready));
        assert!(!app.active_viewport_mut().message_height_is_current(1));
        let Some(last) = app.messages().last() else {
            panic!("expected interruption hint message");
        };
        assert!(matches!(last.role, MessageRole::System(Some(SystemSeverity::Info))));
    }

    #[test]
    fn connected_updates_welcome_session_id_while_pristine() {
        let mut app = make_test_app();
        app.active_messages_mut().push(ChatMessage::welcome(
            env!("CARGO_PKG_VERSION"),
            "-",
            "/test",
            "-",
        ));
        let Some(MessageBlock::Welcome(welcome)) = app.active_messages_mut()[0].blocks.first_mut()
        else {
            panic!("expected welcome block");
        };
        welcome.tip_seed = 7;

        apply_session_update(&mut app, connected_event("claude-updated"));

        let Some(first) = app.messages().first() else {
            panic!("missing welcome message");
        };
        let Some(MessageBlock::Welcome(welcome)) = first.blocks.first() else {
            panic!("expected welcome block");
        };
        assert_eq!(welcome.session_id, "test-session");
        assert_eq!(welcome.tip_seed, 7);
    }

    #[test]
    fn connected_leaves_account_line_skeleton_until_data_arrives() {
        let mut app = make_test_app();
        app.active_messages_mut().push(ChatMessage::welcome(
            env!("CARGO_PKG_VERSION"),
            "old",
            "/test",
            "old",
        ));

        apply_session_update(&mut app, connected_event("opus"));

        let Some(first) = app.messages().first() else {
            panic!("missing welcome message");
        };
        let Some(MessageBlock::Welcome(welcome)) = first.blocks.first() else {
            panic!("expected welcome block");
        };
        // Workspace mode: the renderer shows the "Account: …"
        // skeleton while the picker / status snapshot are still in
        // flight, so the user sees a stable placeholder rather than a
        // brief empty row that fills in.
        assert_eq!(welcome.account_label, "Account");
        assert_eq!(welcome.subscription, "…");
    }

    #[test]
    fn connected_requests_mcp_snapshot_even_outside_mcp_tab() {
        let (mut app, mut rx) = app_with_bridge_connection();
        app.active_view = crate::app::ActiveView::Plugins;
        app.mcp_mut().servers.push(forge_primitives::McpServerStatus {
            name: "supabase".into(),
            status: forge_primitives::McpServerConnectionStatus::Connected,
            server_info: None,
            error: None,
            config: None,
            scope: None,
            tools: None,
            sampling_configured: None,
            sampling_required: None,
        });

        apply_session_update(&mut app, connected_event("claude-updated"));

        let envelope = rx.try_recv().expect("mcp snapshot command");
        assert_eq!(
            envelope,
            forge_primitives::AgentCommand::GetMcpSnapshot {
                session_id: forge_primitives::SessionId::new("test-session".to_owned()),
            }
        );
        assert!(app.mcp().in_flight);
        assert!(app.mcp().servers.is_empty());
    }

    #[test]
    fn connected_updates_cwd_and_clears_resuming_marker() {
        let mut app = make_test_app();
        app.active_messages_mut().push(ChatMessage::welcome(
            env!("CARGO_PKG_VERSION"),
            "-",
            "/test",
            "-",
        ));
        *app.resuming_session_id_mut() = Some("resume-123".into());

        apply_session_update(
            &mut app,
            SessionUpdate::Connected {
                key: forge_workspace::SessionKey::from_session_id("session-cwd".to_owned()),
                session_id: forge_primitives::SessionId::new("session-cwd"),
                cwd: "/changed".into(),
                current_model: test_current_model_primitives("claude-updated"),
                available_models: Vec::new(),
                mode: None,
                history: Vec::new(),
            },
        );

        assert_eq!(app.cwd_raw(), "/changed");
        assert_eq!(app.cwd(), "/changed");
        assert!(app.resuming_session_id().is_none());
        let Some(first) = app.messages().first() else {
            panic!("missing welcome message");
        };
        let Some(MessageBlock::Welcome(welcome)) = first.blocks.first() else {
            panic!("expected welcome block");
        };
        assert_eq!(welcome.cwd, "/changed");
    }

    #[test]
    fn connected_updates_welcome_once_even_after_chat_started() {
        let mut app = make_test_app();
        app.active_messages_mut().push(ChatMessage::welcome(
            env!("CARGO_PKG_VERSION"),
            "-",
            "/test",
            "-",
        ));
        let Some(MessageBlock::Welcome(welcome)) = app.active_messages_mut()[0].blocks.first_mut()
        else {
            panic!("expected welcome block");
        };
        welcome.tip_seed = 11;
        app.active_messages_mut().push(user_msg("hello"));

        apply_session_update(&mut app, connected_event("claude-updated"));

        let Some(first) = app.messages().first() else {
            panic!("missing first message");
        };
        let Some(MessageBlock::Welcome(welcome)) = first.blocks.first() else {
            panic!("expected welcome block");
        };
        assert_eq!(welcome.session_id, "test-session");
        assert_eq!(welcome.tip_seed, 11);
    }

    #[test]
    fn current_model_update_does_not_mutate_welcome_snapshot_after_settings_reconcile() {
        let mut app = make_test_app();
        app.set_session_id(Some(model::SessionId::new("session-1")));
        app.set_current_model(Some(test_current_model("opus")));
        *app.active_messages_mut() =
            vec![ChatMessage::welcome(env!("CARGO_PKG_VERSION"), "-", "/test", "session-1")];
        crate::app::config::store::set_model(
            &mut app.config.committed_settings_document,
            Some("opus"),
        );

        crate::app::config::store::set_model(
            &mut app.config.committed_settings_document,
            Some("haiku"),
        );

        // The wire path delivers model changes via System("init") with
        // a `model` field; same downstream path as the original
        // SessionUpdate::CurrentModelUpdate.
        send_msg(&mut app, system_message("init", serde_json::json!({"model": "claude-opus-4-7"})));

        let Some(MessageBlock::Welcome(welcome)) = app.messages()[0].blocks.first() else {
            panic!("expected welcome block");
        };
        assert_eq!(welcome.session_id, "session-1");
        // Reconcile must not touch the welcome - value stays at
        // whatever the test fixture wrote ("-").
        assert_eq!(welcome.subscription, "-");
    }

    #[test]
    fn connected_resets_session_scoped_view_data() {
        let mut app = make_test_app();
        app.active_messages_mut().push(user_msg("hello"));
        app.status = AppStatus::Running;
        app.set_files_accessed(9);
        app.usage_mut().snapshot = Some(UsageSnapshot {
            source: UsageSourceKind::Oauth,
            fetched_at: std::time::SystemTime::now(),
            five_hour: None,
            seven_day: None,
            seven_day_opus: None,
            seven_day_sonnet: None,
            extra_usage: None,
        });
        app.set_account_info(Some(forge_primitives::AccountInfo {
            email: Some("old@example.com".into()),
            organization: None,
            subscription_type: None,
            token_source: None,
            api_key_source: None,
            api_provider: None,
        }));
        app.plugins.installed.push(crate::app::plugins::InstalledPluginEntry {
            id: "old-plugin".into(),
            version: None,
            scope: "user".into(),
            enabled: true,
            installed_at: None,
            last_updated: None,
            project_path: None,
            capability: crate::app::plugins::PluginCapability::Skill,
        });
        app.plugins.last_inventory_refresh_at = Some(Instant::now());

        apply_session_update(&mut app, connected_event("claude-updated"));

        assert!(matches!(app.status, AppStatus::Ready));
        assert_eq!(app.messages().len(), 1);
        assert!(matches!(app.messages()[0].role, MessageRole::Welcome));
        assert_eq!(app.files_accessed(), 0);
        assert!(app.usage().snapshot.is_none());
        assert!(app.account_info().is_none());
        assert!(app.plugins.installed.is_empty());
        assert!(app.plugins.last_inventory_refresh_at.is_none());
    }

    #[test]
    fn current_model_update_leaves_existing_welcome_snapshot_unchanged() {
        let mut app = make_test_app();
        app.set_current_model(Some(test_current_model("opus")));
        app.active_messages_mut().push(ChatMessage::welcome(
            env!("CARGO_PKG_VERSION"),
            "-",
            "/test",
            "-",
        ));
        app.active_messages_mut().push(user_msg("hello"));

        send_msg(&mut app, system_message("init", serde_json::json!({"model": "claude-opus-4-7"})));

        let Some(first) = app.messages().first() else {
            panic!("missing first message");
        };
        let Some(MessageBlock::Welcome(welcome)) = first.blocks.first() else {
            panic!("expected welcome block");
        };
        assert_eq!(welcome.session_id, "-");

        send_msg(
            &mut app,
            system_message("init", serde_json::json!({"model": "claude-sonnet-4-5"})),
        );

        let Some(first) = app.messages().first() else {
            panic!("missing first message");
        };
        let Some(MessageBlock::Welcome(welcome)) = first.blocks.first() else {
            panic!("expected welcome block");
        };
        assert_eq!(welcome.session_id, "-");
    }

    #[test]
    fn auth_required_sets_hint_without_prefilling_login_command() {
        let mut app = make_test_app();
        app.input_mut().set_text("keep me");

        let session_key = active_session_key(&app);
        apply_session_update(
            &mut app,
            SessionUpdate::AuthRequired {
                key: session_key,
                method_name: "oauth".into(),
                method_description: "Open browser".into(),
            },
        );

        assert!(matches!(app.status, AppStatus::Ready));
        assert_eq!(app.input().text(), "keep me");
        let Some(hint) = &app.login_hint() else {
            panic!("expected login hint");
        };
        assert_eq!(hint.method_name, "oauth");
        assert_eq!(hint.method_description, "Open browser");
    }

    #[test]
    fn service_status_warning_pushes_system_warning_without_locking_input() {
        let mut app = make_test_app();

        apply_session_update(
            &mut app,
            SessionUpdate::ServiceStatus {
                severity: ServiceSeverity::Warning,
                message: "Claude Code status: Partial Outage (indicator: minor).".into(),
            },
        );

        assert!(matches!(app.status, AppStatus::Ready));
        let Some(last) = app.messages().last() else {
            panic!("expected system message");
        };
        assert!(matches!(last.role, MessageRole::System(Some(SystemSeverity::Warning))));
    }

    #[test]
    fn service_status_error_pushes_system_error_without_locking_input() {
        let mut app = make_test_app();
        app.input_mut().set_text("draft stays");

        apply_session_update(
            &mut app,
            SessionUpdate::ServiceStatus {
                severity: ServiceSeverity::Error,
                message: "Claude Code status: Major Outage (indicator: major).".into(),
            },
        );

        assert!(matches!(app.status, AppStatus::Ready));
        assert_eq!(app.input().text(), "draft stays");
        let Some(last) = app.messages().last() else {
            panic!("expected system message");
        };
        assert!(matches!(last.role, MessageRole::System(Some(SystemSeverity::Error))));
    }

    #[test]
    fn session_replaced_resets_chat_and_transient_state() {
        let mut app = make_test_app();
        app.active_messages_mut().push(ChatMessage::welcome(
            env!("CARGO_PKG_VERSION"),
            "-",
            "/test",
            "-",
        ));
        let Some(MessageBlock::Welcome(welcome)) = app.active_messages_mut()[0].blocks.first_mut()
        else {
            panic!("expected welcome block");
        };
        welcome.tip_seed = 5;
        app.active_messages_mut().push(user_msg("hello"));
        app.active_messages_mut()
            .push(assistant_msg(vec![MessageBlock::Text(TextBlock::from_complete("world"))]));
        app.status = AppStatus::Running;
        app.set_files_accessed(9);
        app.todos_mut().push(TodoItem {
            id: "1".to_owned(),
            content: "Task".into(),
            status: TodoStatus::InProgress,
            active_form: String::new(),
        });
        *app.mention_mut() = Some(mention::MentionState::new(0, 0, String::new(), Vec::new()));
        app.mcp_mut().servers.push(forge_primitives::McpServerStatus {
            name: "supabase".into(),
            status: forge_primitives::McpServerConnectionStatus::Connected,
            server_info: None,
            error: None,
            config: None,
            scope: None,
            tools: None,
            sampling_configured: None,
            sampling_required: None,
        });

        apply_session_update(
            &mut app,
            SessionUpdate::SessionReplaced {
                key: forge_workspace::SessionKey::from_session_id("replacement".to_owned()),
                session_id: forge_primitives::SessionId::new("replacement"),
                cwd: "/replacement".into(),
                current_model: test_current_model_primitives("new-model"),
                available_models: Vec::new(),
                mode: None,
                history: Vec::new(),
            },
        );

        assert!(matches!(app.status, AppStatus::Ready));
        assert_eq!(app.session_id().map(|s| s.to_string()).as_deref(), Some("replacement"));
        assert_eq!(app.current_model().map(|model| model.resolved_id.as_str()), Some("new-model"));
        assert_eq!(app.messages().len(), 1);
        assert!(matches!(app.messages()[0].role, MessageRole::Welcome));
        assert_eq!(app.files_accessed(), 0);
        assert!(app.todos().is_empty());
        assert!(app.mention().is_none());
        assert!(app.mcp().servers.is_empty());
        assert_eq!(app.cwd_raw(), "/replacement");
        assert_eq!(app.cwd(), "/replacement");
        let Some(MessageBlock::Welcome(welcome)) = app.messages()[0].blocks.first() else {
            panic!("expected welcome block");
        };
        assert_eq!(welcome.cwd, "/replacement");
        assert_ne!(welcome.tip_seed, 5);
    }

    #[test]
    fn session_replaced_requests_mcp_snapshot_even_outside_mcp_tab() {
        let (mut app, mut rx) = app_with_bridge_connection();
        app.active_view = crate::app::ActiveView::Plugins;
        app.mcp_mut().servers.push(forge_primitives::McpServerStatus {
            name: "supabase".into(),
            status: forge_primitives::McpServerConnectionStatus::Connected,
            server_info: None,
            error: None,
            config: None,
            scope: None,
            tools: None,
            sampling_configured: None,
            sampling_required: None,
        });

        apply_session_update(
            &mut app,
            SessionUpdate::SessionReplaced {
                key: forge_workspace::SessionKey::from_session_id("replacement".to_owned()),
                session_id: forge_primitives::SessionId::new("replacement"),
                cwd: "/replacement".into(),
                current_model: test_current_model_primitives("new-model"),
                available_models: Vec::new(),
                mode: None,
                history: Vec::new(),
            },
        );

        let envelope = rx.try_recv().expect("mcp snapshot command");
        assert_eq!(
            envelope,
            forge_primitives::AgentCommand::GetMcpSnapshot {
                session_id: forge_primitives::SessionId::new("replacement".to_owned()),
            }
        );
        assert!(app.mcp().in_flight);
        assert!(app.mcp().servers.is_empty());
    }

    #[test]
    fn connected_requests_status_snapshot_on_connect() {
        let (mut app, mut rx) = app_with_bridge_connection();

        apply_session_update(&mut app, connected_event("claude-updated"));

        let mcp = rx.try_recv().expect("mcp snapshot command");
        assert_eq!(
            mcp,
            forge_primitives::AgentCommand::GetMcpSnapshot {
                session_id: forge_primitives::SessionId::new("test-session".to_owned()),
            }
        );
        let status = rx.try_recv().expect("status snapshot command");
        assert_eq!(
            status,
            forge_primitives::AgentCommand::GetStatusSnapshot {
                session_id: forge_primitives::SessionId::new("test-session".to_owned()),
            }
        );
    }

    #[test]
    fn stale_status_snapshot_for_old_session_is_ignored() {
        let mut app = make_test_app();
        app.set_session_id(Some(model::SessionId::new("current-session")));

        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::StatusSnapshot {
                session_id: "old-session".into(),
                account: forge_primitives::AccountInfo {
                    email: Some("old@example.com".into()),
                    organization: None,
                    subscription_type: None,
                    token_source: None,
                    api_key_source: None,
                    api_provider: None,
                },
                forge_account: None,
            },
        );

        assert!(app.account_info().is_none());
    }

    #[test]
    fn forge_account_identity_ready_stores_name_but_keeps_welcome_skeleton_until_tier_arrives() {
        let mut app = make_test_app();
        app.active_messages_mut().push(ChatMessage::welcome(
            env!("CARGO_PKG_VERSION"),
            "",
            "/test",
            "session-1",
        ));
        app.set_session_id(Some(model::SessionId::new("session-1")));

        // Pre-snapshot: bridge tells us which account got picked.
        let session_key = active_session_key(&app);
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::ForgeAccountIdentity {
                key: session_key,
                display_name: "Stargate".into(),
            },
        );

        // App state stores the name (Status panel needs it).
        assert_eq!(app.active_account_display_name().as_deref(), Some("Stargate"));

        // Welcome row shows the "Account: …" skeleton because the
        // tier hasn't arrived yet - committing "Account: Stargate"
        // now would flicker into "Account: Stargate · team" once
        // the status snapshot lands.
        let Some(MessageBlock::Welcome(welcome)) = app.messages()[0].blocks.first() else {
            panic!("expected welcome block");
        };
        assert_eq!(welcome.account_label, "Account");
        assert_eq!(welcome.subscription, "…");
    }

    #[test]
    fn status_snapshot_with_forge_account_renders_account_label() {
        let mut app = make_test_app();
        app.active_messages_mut().push(ChatMessage::welcome(
            env!("CARGO_PKG_VERSION"),
            "-",
            "/test",
            "session-1",
        ));
        app.set_session_id(Some(model::SessionId::new("session-1")));

        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::StatusSnapshot {
                session_id: "session-1".into(),
                account: forge_primitives::AccountInfo {
                    email: None,
                    organization: None,
                    subscription_type: Some("team".into()),
                    token_source: None,
                    api_key_source: None,
                    api_provider: None,
                },
                forge_account: Some(forge_primitives::ForgeAccountIdentity::new("Stargate".into())),
            },
        );

        let Some(MessageBlock::Welcome(welcome)) = app.messages()[0].blocks.first() else {
            panic!("expected welcome block");
        };
        assert_eq!(welcome.account_label, "Account");
        assert_eq!(welcome.subscription, "Stargate · team");
    }

    #[test]
    fn status_snapshot_without_forge_account_keeps_workspace_account_skeleton() {
        let mut app = make_test_app();
        app.active_messages_mut().push(ChatMessage::welcome(
            env!("CARGO_PKG_VERSION"),
            "-",
            "/test",
            "session-1",
        ));
        app.set_session_id(Some(model::SessionId::new("session-1")));

        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::StatusSnapshot {
                session_id: "session-1".into(),
                account: forge_primitives::AccountInfo {
                    email: None,
                    organization: None,
                    subscription_type: Some("Claude Max".into()),
                    token_source: None,
                    api_key_source: None,
                    api_provider: None,
                },
                forge_account: None,
            },
        );

        // Workspace mode: without a forge_account display name to
        // pair with the subscription tier, the row stays on the
        // "Account: …" skeleton rather than flipping to the legacy
        // `Subscription: <tier>` label.
        let Some(MessageBlock::Welcome(welcome)) = app.messages()[0].blocks.first() else {
            panic!("expected welcome block");
        };
        assert_eq!(welcome.account_label, "Account");
        assert_eq!(welcome.subscription, "…");
    }

    #[test]
    fn stale_mcp_snapshot_for_old_session_is_ignored() {
        let mut app = make_test_app();
        app.set_session_id(Some(model::SessionId::new("current-session")));
        app.mcp_mut().servers.push(forge_primitives::McpServerStatus {
            name: "current".into(),
            status: forge_primitives::McpServerConnectionStatus::Connected,
            server_info: None,
            error: None,
            config: None,
            scope: None,
            tools: None,
            sampling_configured: None,
            sampling_required: None,
        });

        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::McpSnapshot {
                session_id: "old-session".into(),
                servers: vec![forge_primitives::McpServerStatus {
                    name: "stale".into(),
                    status: forge_primitives::McpServerConnectionStatus::Connected,
                    server_info: None,
                    error: None,
                    config: None,
                    scope: None,
                    tools: None,
                    sampling_configured: None,
                    sampling_required: None,
                }],
                error: None,
            },
        );

        assert_eq!(app.mcp().servers.len(), 1);
        assert_eq!(app.mcp().servers[0].name, "current");
    }

    #[test]
    fn recent_sessions_routes_to_targeted_bucket_not_active_bucket() {
        // Regression for the /resume autocomplete showing the wrong
        // project's sessions: each bucket owns its own
        // `recent_sessions`. A listing delivered for bucket B must
        // NOT overwrite bucket A's list, even if A is currently active.
        let mut app = make_test_app();
        let key_a = forge_workspace::SessionKey::from_str_for_test("project-a");
        let key_b = forge_workspace::SessionKey::from_str_for_test("project-b");
        app.sessions.insert(key_a.clone(), crate::app::session::UiSession::new(key_a.clone()));
        app.sessions.insert(key_b.clone(), crate::app::session::UiSession::new(key_b.clone()));
        app.active_session_key = Some(key_a.clone());

        // Seed A's list directly so we have an observable baseline.
        app.sessions.get_mut(&key_a).expect("bucket a").recent_sessions =
            vec![crate::app::RecentSessionInfo {
                session_id: "a-only".into(),
                summary: "From A".into(),
                last_modified_ms: 100,
                cwd: Some("/proj-a".into()),
                custom_title: None,
                first_prompt: None,
            }];

        // Listing targets B. The active bucket is A.
        apply_session_update(
            &mut app,
            SessionUpdate::SessionsListed {
                key: key_b.clone(),
                sessions: vec![forge_primitives::SessionListEntry {
                    session_id: "b-only".into(),
                    summary: "From B".into(),
                    last_modified_ms: 200,
                    cwd: Some("/proj-b".into()),
                    custom_title: None,
                    first_prompt: None,
                }],
            },
        );

        // A (active) still has its original list, B got the new one.
        assert_eq!(app.recent_sessions().len(), 1);
        assert_eq!(app.recent_sessions()[0].session_id, "a-only");
        let bucket_b = app.sessions.get(&key_b).expect("bucket b");
        assert_eq!(bucket_b.recent_sessions.len(), 1);
        assert_eq!(bucket_b.recent_sessions[0].session_id, "b-only");
    }

    #[test]
    fn stale_plugin_inventory_result_for_old_cwd_is_ignored() {
        let mut app = make_test_app();
        app.set_cwd_raw("/current");

        apply_session_update(
            &mut app,
            SessionUpdate::PluginsInventoryUpdated {
                cwd_raw: "/old".into(),
                snapshot: crate::app::plugins::PluginsInventorySnapshot {
                    installed: vec![crate::app::plugins::InstalledPluginEntry {
                        id: "stale-plugin".into(),
                        version: None,
                        scope: "user".into(),
                        enabled: true,
                        installed_at: None,
                        last_updated: None,
                        project_path: None,
                        capability: crate::app::plugins::PluginCapability::Skill,
                    }],
                    marketplace: Vec::new(),
                    marketplaces: Vec::new(),
                },
                claude_path: std::path::PathBuf::from("claude"),
            },
        );

        assert!(app.plugins.installed.is_empty());
    }

    #[test]
    fn slash_command_error_while_resuming_returns_ready_and_clears_marker() {
        let mut app = make_test_app();
        app.status = AppStatus::CommandPending;
        *app.resuming_session_id_mut() = Some("resume-123".into());

        let session_key = active_session_key(&app);
        apply_session_update(
            &mut app,
            SessionUpdate::SlashCommandError { key: session_key, message: "resume failed".into() },
        );

        assert!(matches!(app.status, AppStatus::Ready));
        assert!(app.resuming_session_id().is_none());
    }

    #[test]
    fn mcp_operation_error_stays_in_mcp_feedback_and_out_of_chat() {
        let mut app = make_test_app();
        app.active_view = crate::app::ActiveView::Mcp;
        app.config.status_message =
            Some("Starting MCP auth for claude.ai Google Calendar...".into());
        app.mcp_mut().in_flight = true;

        let session_key = active_session_key(&app);
        apply_session_update(
            &mut app,
            SessionUpdate::McpOperationError {
                key: session_key,
                error: forge_primitives::McpOperationError {
                    server_name: Some("claude.ai Google Calendar".into()),
                    operation: "authenticate".into(),
                    message: "Server type \"claudeai-proxy\" does not support OAuth authentication"
                        .into(),
                },
            },
        );

        assert_eq!(
            app.mcp().last_error.as_deref(),
            Some(
                "Failed to authenticate MCP server claude.ai Google Calendar: Server type \"claudeai-proxy\" does not support OAuth authentication"
            )
        );
        assert_eq!(app.config.last_error, app.mcp().last_error);
        assert!(app.config.status_message.is_none());
        assert!(!app.mcp().in_flight);
        assert!(app.messages().is_empty());
    }

    #[test]
    fn current_mode_update_clears_pending_when_expected() {
        let mut app = make_test_app();
        app.status = AppStatus::CommandPending;
        *app.pending_command_label_mut() = Some("Switching mode...".into());
        *app.pending_command_ack_mut() = Some(PendingCommandAck::CurrentMode);
        app.set_mode(Some(crate::app::ModeState {
            current_mode_id: "code".to_owned(),
            current_mode_name: "Code".to_owned(),
            available_modes: vec![
                crate::app::ModeInfo {
                    id: "code".to_owned(),
                    name: "Code".to_owned(),
                    description: None,
                },
                crate::app::ModeInfo {
                    id: "plan".to_owned(),
                    name: "Plan".to_owned(),
                    description: None,
                },
            ],
        }));
        app.active_messages_mut().push(user_msg("seed"));
        let layout_generation_before = app.viewport().layout_generation;

        // Wire path: server-side mode switches arrive via System("status")
        // with a `permissionMode` field.
        send_msg(&mut app, system_message("status", serde_json::json!({"permissionMode": "plan"})));

        assert!(matches!(app.status, AppStatus::Ready));
        assert!(app.pending_command_label().is_none());
        assert!(app.pending_command_ack().is_none());
        let layout_generation_after = app.viewport().layout_generation;
        let mode = app.mode().cloned().expect("mode should be present");
        assert_eq!(mode.current_mode_id, "plan");
        assert_eq!(mode.current_mode_name, "Plan");
        assert_eq!(layout_generation_after, layout_generation_before + 1);
    }

    #[test]
    fn mode_state_update_invalidates_layout_when_mode_changes() {
        let mut app = make_test_app();
        app.set_mode(Some(crate::app::ModeState {
            current_mode_id: "code".to_owned(),
            current_mode_name: "Code".to_owned(),
            available_modes: vec![
                crate::app::ModeInfo {
                    id: "code".to_owned(),
                    name: "Code".to_owned(),
                    description: None,
                },
                crate::app::ModeInfo {
                    id: "plan".to_owned(),
                    name: "Plan".to_owned(),
                    description: None,
                },
            ],
        }));
        app.active_messages_mut().push(user_msg("seed"));
        let layout_generation_before = app.viewport().layout_generation;

        // Wire path: System("init") with permissionMode rebuilds the
        // mode state and applies via apply_mode_state_update - same
        // downstream invalidate-layout effect as the original
        // SessionUpdate::ModeStateUpdate.
        send_msg(&mut app, system_message("init", serde_json::json!({"permissionMode": "plan"})));

        assert_eq!(app.viewport().layout_generation, layout_generation_before + 1);
    }

    #[test]
    fn current_model_update_updates_state_and_clears_pending_when_expected() {
        let mut app = make_test_app();
        app.status = AppStatus::CommandPending;
        *app.pending_command_label_mut() = Some("Switching model...".into());
        *app.pending_command_ack_mut() = Some(PendingCommandAck::CurrentModel);
        app.set_current_model(Some(test_current_model("old-model")));

        send_msg(&mut app, system_message("init", serde_json::json!({"model": "sonnet"})));

        assert!(matches!(app.status, AppStatus::Ready));
        assert_eq!(app.current_model().map(|model| model.resolved_id.as_str()), Some("sonnet"));
        assert!(app.pending_command_label().is_none());
        assert!(app.pending_command_ack().is_none());
    }

    #[test]
    fn resume_does_not_add_confirmation_system_message() {
        let mut app = make_test_app();
        *app.resuming_session_id_mut() = Some("requested-123".into());

        apply_session_update(
            &mut app,
            SessionUpdate::SessionReplaced {
                key: forge_workspace::SessionKey::from_session_id("active-456".to_owned()),
                session_id: forge_primitives::SessionId::new("active-456"),
                cwd: "/replacement".into(),
                current_model: test_current_model_primitives("new-model"),
                available_models: Vec::new(),
                mode: None,
                history: Vec::new(),
            },
        );

        assert_eq!(app.messages().len(), 1);
        assert!(matches!(app.messages()[0].role, MessageRole::Welcome));
        assert!(app.resuming_session_id().is_none());
        assert!(matches!(app.status, AppStatus::Ready));
    }

    #[test]
    fn resume_history_renders_user_message_chunks() {
        let mut app = make_test_app();
        let history_updates =
            vec![user_text_message("first user line"), assistant_text_message("assistant reply")];

        apply_session_update(
            &mut app,
            SessionUpdate::SessionReplaced {
                key: forge_workspace::SessionKey::from_session_id("active-456".to_owned()),
                session_id: forge_primitives::SessionId::new("active-456"),
                cwd: "/replacement".into(),
                current_model: test_current_model_primitives("new-model"),
                available_models: Vec::new(),
                mode: None,
                history: history_updates,
            },
        );

        assert_eq!(app.messages().len(), 3);
        assert!(matches!(app.messages()[0].role, MessageRole::Welcome));
        assert!(matches!(app.messages()[1].role, MessageRole::User));
        assert!(matches!(app.messages()[2].role, MessageRole::Assistant));

        let Some(MessageBlock::Text(user_text)) = app.messages()[1].blocks.first() else {
            panic!("expected user text block");
        };
        assert_eq!(user_text.text, "first user line");
    }

    #[test]
    fn resume_history_preserves_turn_order_between_user_and_assistant_messages() {
        let mut app = make_test_app();
        let history_updates = vec![
            user_text_message("first user"),
            assistant_text_message("first assistant"),
            user_text_message("second user"),
            assistant_text_message("second assistant"),
        ];

        apply_session_update(
            &mut app,
            SessionUpdate::SessionReplaced {
                key: forge_workspace::SessionKey::from_session_id("active-457".to_owned()),
                session_id: forge_primitives::SessionId::new("active-457"),
                cwd: "/replacement".into(),
                current_model: test_current_model_primitives("new-model"),
                available_models: Vec::new(),
                mode: None,
                history: history_updates,
            },
        );

        let rendered: Vec<(MessageRole, String)> = app
            .messages()
            .iter()
            .filter_map(|message| {
                let text = message.blocks.iter().find_map(|block| match block {
                    MessageBlock::Text(block) => Some(block.text.clone()),
                    _ => None,
                })?;
                Some((message.role.clone(), text))
            })
            .collect();

        assert_eq!(
            rendered,
            vec![
                (MessageRole::User, "first user".to_owned()),
                (MessageRole::Assistant, "first assistant".to_owned()),
                (MessageRole::User, "second user".to_owned()),
                (MessageRole::Assistant, "second assistant".to_owned()),
            ]
        );
    }

    #[test]
    fn resume_history_forces_open_tool_calls_to_failed() {
        let mut app = make_test_app();
        // "Bash" name normalises to ToolKind::Execute via the resume
        // synthesizer; matches the prior model::ToolKind::Execute.
        let open_tool = assistant_tool_use_message(
            "resume-open",
            "Bash",
            serde_json::json!({"command": "Execute command"}),
        );

        apply_session_update(
            &mut app,
            SessionUpdate::SessionReplaced {
                key: forge_workspace::SessionKey::from_session_id("active-789".to_owned()),
                session_id: forge_primitives::SessionId::new("active-789"),
                cwd: "/replacement".into(),
                current_model: test_current_model_primitives("new-model"),
                available_models: Vec::new(),
                mode: None,
                history: vec![open_tool],
            },
        );

        let Some((mi, bi)) = app.lookup_tool_call("resume-open") else {
            panic!("missing tool call index");
        };
        let Some(MessageBlock::ToolCall(tc)) =
            app.messages().get(mi).and_then(|m| m.blocks.get(bi))
        else {
            panic!("expected tool call block");
        };
        assert_eq!(tc.status, model::ToolCallStatus::Failed);
    }

    #[test]
    fn resume_history_clears_active_turn_owner_after_replay() {
        let mut app = make_test_app();

        apply_session_update(
            &mut app,
            SessionUpdate::SessionReplaced {
                key: forge_workspace::SessionKey::from_session_id("active-790".to_owned()),
                session_id: forge_primitives::SessionId::new("active-790"),
                cwd: "/replacement".into(),
                current_model: test_current_model_primitives("new-model"),
                available_models: Vec::new(),
                mode: None,
                history: vec![assistant_text_message("assistant reply")],
            },
        );

        assert_eq!(app.active_turn_assistant_idx(), None);
    }

    #[test]
    fn resume_history_clears_tool_scope_tracking_after_replay() {
        let mut app = make_test_app();
        // "Task" name normalises to ToolKind::Think via the resume
        // synthesizer and gets the matching `claudeCode.toolName` meta.
        let task_tool = assistant_tool_use_message(
            "resume-task",
            "Task",
            serde_json::json!({"description": "Run subagent"}),
        );

        apply_session_update(
            &mut app,
            SessionUpdate::SessionReplaced {
                key: forge_workspace::SessionKey::from_session_id("active-791".to_owned()),
                session_id: forge_primitives::SessionId::new("active-791"),
                cwd: "/replacement".into(),
                current_model: test_current_model_primitives("new-model"),
                available_models: Vec::new(),
                mode: None,
                history: vec![task_tool],
            },
        );

        assert!(app.active_task_ids().is_empty());
        assert_eq!(app.tool_call_scope("resume-task"), None);
    }

    #[test]
    fn turn_complete_without_cancel_does_not_render_interrupted_hint() {
        let mut app = make_test_app();
        let session_key = active_session_key(&app);
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::TurnComplete {
                key: session_key,
                terminal_reason: None,
            },
        );
        assert!(app.messages().is_empty());
    }

    #[test]
    fn turn_complete_with_trailing_user_anticipates_next_turn() {
        // Mid-turn-submitted user bubbles end up at the tail of the
        // chat when the in-flight turn wraps. Claude immediately
        // starts a follow-up turn to consume them, but its first
        // content chunk takes ~1-2s to arrive. forge anticipates the
        // next turn by pushing an empty assistant placeholder + flipping
        // status to Thinking so the spinner stays visible.
        let mut app = make_test_app();
        app.set_session_id(Some(model::SessionId::new("session-anticipate")));
        app.active_messages_mut().push(user_msg("first"));
        app.active_messages_mut().push(assistant_msg(vec![MessageBlock::Text(
            TextBlock::from_complete("response to first"),
        )]));
        app.active_messages_mut().push(user_msg("mid-turn-1"));
        app.active_messages_mut().push(user_msg("mid-turn-2"));
        app.status = AppStatus::Running;

        let session_key = active_session_key(&app);
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::TurnComplete {
                key: session_key,
                terminal_reason: None,
            },
        );

        // An empty assistant placeholder was appended after the two
        // mid-turn user bubbles, and the active turn assistant index
        // points at it.
        let last = app.messages().last().expect("trailing assistant placeholder");
        assert!(matches!(last.role, MessageRole::Assistant));
        assert!(last.blocks.is_empty(), "placeholder is empty until claude streams text");
        assert_eq!(app.active_turn_assistant_message_idx(), Some(app.messages().len() - 1));
        // Status flipped back to Thinking so the spinner renders.
        assert!(matches!(app.status, AppStatus::Thinking));
    }

    #[test]
    fn turn_complete_with_trailing_assistant_does_not_anticipate() {
        // Standard turn completion (no mid-turn submits): tail is the
        // assistant message that just streamed. No extra placeholder
        // pushed, status returns to Ready.
        let mut app = make_test_app();
        app.set_session_id(Some(model::SessionId::new("session-normal")));
        app.active_messages_mut().push(user_msg("hi"));
        app.active_messages_mut()
            .push(assistant_msg(vec![MessageBlock::Text(TextBlock::from_complete("hello back"))]));
        app.status = AppStatus::Running;

        let before = app.messages().len();
        let session_key = active_session_key(&app);
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::TurnComplete {
                key: session_key,
                terminal_reason: None,
            },
        );

        assert_eq!(app.messages().len(), before, "no extra placeholder appended");
        assert!(matches!(app.status, AppStatus::Ready));
    }

    #[test]
    fn turn_complete_keeps_history_and_adds_compaction_success_after_manual_boundary() {
        let mut app = make_test_app();
        app.set_session_id(Some(model::SessionId::new("session-x")));
        app.active_messages_mut().push(user_msg("/compact"));
        app.active_messages_mut()
            .push(assistant_msg(vec![MessageBlock::Text(TextBlock::from_complete("compacted"))]));
        send_msg(
            &mut app,
            system_message(
                "compact_boundary",
                serde_json::json!({
                    "compact_metadata": {"trigger": "manual", "pre_tokens": 123_456}
                }),
            ),
        );
        assert!(app.pending_compact_clear());

        let session_key = active_session_key(&app);
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::TurnComplete {
                key: session_key,
                terminal_reason: None,
            },
        );

        assert!(!app.pending_compact_clear());
        assert_eq!(app.messages().len(), 3);
        let Some(ChatMessage {
            role: MessageRole::System(Some(SystemSeverity::Info)), blocks, ..
        }) = app.messages().last()
        else {
            panic!("expected compaction success system message");
        };
        let Some(MessageBlock::Text(block)) = blocks.first() else {
            panic!("expected text block");
        };
        assert_eq!(block.text, "Session successfully compacted.");
        assert_eq!(app.session_id().map(|s| s.to_string()).as_deref(), Some("session-x"));
    }

    #[test]
    fn first_agent_chunk_clears_unconfirmed_compacting_without_success_message() {
        let mut app = make_test_app();
        app.set_is_compacting(true);

        send_msg(&mut app, assistant_message(vec![text_block("regular answer")]));

        assert!(!app.is_compacting());
        assert!(!app.pending_compact_clear());
        assert!(app.messages().iter().all(|message| {
            !matches!(
                message,
                ChatMessage { role: MessageRole::System(Some(SystemSeverity::Info)), .. }
            )
        }));
    }

    #[test]
    fn session_status_idle_does_not_emit_compaction_success_without_boundary() {
        let mut app = make_test_app();
        app.set_is_compacting(true);

        // Wire path: SessionStatus::Idle arrives as System("status")
        // with `"status": null` (the CLI's idle signal).
        send_msg(&mut app, system_message("status", serde_json::json!({"status": null})));

        assert!(!app.is_compacting());
        assert!(!app.pending_compact_clear());
        assert!(app.messages().is_empty());
    }

    #[test]
    fn session_status_compacting_sets_is_compacting_via_wire() {
        // After dropping the optimistic-set in handle_compact_submit,
        // `is_compacting` is driven exclusively by the wire status
        // frame. Verified via the captured sdk_compact baseline: the
        // CLI emits `status:"compacting"` as the first inbound frame
        // after `/compact` flows out.
        let mut app = make_test_app();
        assert!(!app.is_compacting());

        send_msg(&mut app, system_message("status", serde_json::json!({"status": "compacting"})));

        assert!(app.is_compacting(), "wire status=compacting must flip is_compacting");
    }

    #[test]
    fn turn_error_keeps_history_when_compact_pending() {
        let mut app = make_test_app();
        app.set_pending_compact_clear(true);
        app.active_messages_mut().push(user_msg("/compact"));

        let session_key = active_session_key(&app);
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::TurnError {
                key: session_key,
                message: "adapter failed".into(),
                class: None,
                terminal_reason: None,
            },
        );

        assert!(!app.pending_compact_clear());
        assert!(matches!(app.status, AppStatus::Error));
        assert_eq!(app.messages().len(), 3);
        assert!(matches!(app.messages()[0].role, MessageRole::User));
        let Some(ChatMessage {
            role: MessageRole::System(Some(SystemSeverity::Info)), blocks, ..
        }) = app.messages().get(1)
        else {
            panic!("expected compaction success system message");
        };
        let Some(MessageBlock::Text(block)) = blocks.first() else {
            panic!("expected text block");
        };
        assert_eq!(block.text, "Session successfully compacted.");
        let Some(ChatMessage { role: MessageRole::System(_), blocks, .. }) = app.messages().last()
        else {
            panic!("expected system error message");
        };
        let Some(MessageBlock::Text(block)) = blocks.first() else {
            panic!("expected text block");
        };
        assert!(block.text.contains("Turn failed: adapter failed"));
        assert!(block.text.contains("Press Ctrl+Q to quit and try again"));
    }

    #[test]
    fn turn_cancel_keeps_manual_compaction_success_pending_until_exit() {
        let mut app = make_test_app();
        app.set_pending_compact_clear(true);
        app.set_is_compacting(true);

        let session_key = active_session_key(&app);
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::TurnCancelled { key: session_key },
        );

        assert!(app.pending_compact_clear());
        assert!(app.is_compacting());
    }

    #[test]
    fn turn_error_after_cancel_keeps_compaction_success_before_interrupted_hint() {
        let mut app = make_test_app();
        app.active_messages_mut().push(user_msg("/compact"));
        app.set_pending_compact_clear(true);
        app.set_is_compacting(true);

        let session_key = active_session_key(&app);
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::TurnCancelled { key: session_key },
        );
        let session_key = active_session_key(&app);
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::TurnError {
                key: session_key,
                message: "cancelled".into(),
                class: None,
                terminal_reason: None,
            },
        );

        assert_eq!(app.messages().len(), 3);
        assert!(matches!(app.messages()[1].role, MessageRole::System(Some(SystemSeverity::Info))));
        let Some(MessageBlock::Text(block)) = app.messages()[1].blocks.first() else {
            panic!("expected text block");
        };
        assert_eq!(block.text, "Session successfully compacted.");
        let Some(MessageBlock::Text(block)) = app.messages()[2].blocks.first() else {
            panic!("expected text block");
        };
        assert_eq!(block.text, "Conversation interrupted. Tell the model how to proceed.");
    }

    #[test]
    fn turn_error_plan_limit_shows_next_steps_guidance() {
        let mut app = make_test_app();

        let session_key = active_session_key(&app);
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::TurnError {
                key: session_key,
                message: "HTTP 429 Too Many Requests: max turns exceeded".into(),
                class: None,
                terminal_reason: None,
            },
        );

        assert!(matches!(app.status, AppStatus::Error));
        let Some(ChatMessage { role: MessageRole::System(_), blocks, .. }) = app.messages().last()
        else {
            panic!("expected system error message");
        };
        assert!(matches!(blocks.first(), Some(MessageBlock::Notice(_))));
        let text = first_block_text(app.messages().last().expect("expected message"));
        assert!(text.contains("Turn blocked by account or plan limits"));
        assert!(text.contains("Next steps:"));
        assert!(text.contains("Check quota/billing"));
    }

    #[test]
    fn classified_turn_error_plan_limit_uses_guidance_without_text_matching() {
        let mut app = make_test_app();

        let session_key = active_session_key(&app);
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::TurnError {
                key: session_key,
                message: "turn failed".into(),
                class: Some(forge_workspace::TurnErrorClass::PlanLimit),
                terminal_reason: None,
            },
        );

        assert!(matches!(app.status, AppStatus::Error));
        let Some(ChatMessage { role: MessageRole::System(_), blocks, .. }) = app.messages().last()
        else {
            panic!("expected system error message");
        };
        assert!(matches!(blocks.first(), Some(MessageBlock::Notice(_))));
        let text = first_block_text(app.messages().last().expect("expected message"));
        assert!(text.contains("Turn blocked by account or plan limits"));
        assert!(text.contains("Next steps:"));
    }

    #[test]
    fn classified_turn_error_auth_required_shows_error_in_session() {
        // AuthRequired surfaces as an in-session error banner (status
        // = Error, input locked) rather than force-quitting the TUI.
        // Other sessions in this forge may still be healthy and the
        // user can refresh auth from another terminal then re-prompt.
        let mut app = make_test_app();

        let session_key = active_session_key(&app);
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::TurnError {
                key: session_key,
                message: "auth required".into(),
                class: Some(forge_workspace::TurnErrorClass::AuthRequired),
                terminal_reason: None,
            },
        );

        assert!(matches!(app.status, AppStatus::Error));
        assert!(!app.should_quit, "AuthRequired must NOT force-quit the TUI");
        assert_eq!(
            app.exit_error, None,
            "AuthRequired must NOT set exit_error (no exit, no error overlay)",
        );
    }

    #[test]
    fn turn_error_clears_tool_scope_tracking() {
        let mut app = make_test_app();
        app.active_messages_mut().push(assistant_msg(vec![MessageBlock::ToolCall(Box::new(
            tool_call("task-1", model::ToolCallStatus::InProgress),
        ))]));
        app.register_tool_call_scope("task-1".into(), ToolCallScope::SubagentRoot);
        app.insert_active_task("task-1".into());

        let session_key = active_session_key(&app);
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::TurnError {
                key: session_key,
                message: "boom".into(),
                class: None,
                terminal_reason: None,
            },
        );

        assert!(app.active_task_ids().is_empty());
        assert_eq!(app.tool_call_scope("task-1"), None);
    }

    #[test]
    fn auth_required_clears_active_turn_runtime_tracking() {
        let mut app = make_test_app();
        app.status = AppStatus::Running;
        app.set_session_id(Some(model::SessionId::new("session-auth")));
        app.set_current_model(Some(test_current_model("claude-old")));
        app.set_mode(Some(crate::app::ModeState {
            current_mode_id: "plan".into(),
            current_mode_name: "Plan".into(),
            available_modes: vec![crate::app::ModeInfo {
                id: "plan".into(),
                name: "Plan".into(),
                description: None,
            }],
        }));
        app.set_fast_mode_state(model::FastModeState::On);
        app.active_messages_mut().push(assistant_msg(vec![MessageBlock::ToolCall(Box::new(
            tool_call("task-1", model::ToolCallStatus::InProgress),
        ))]));
        app.bind_active_turn_assistant(0);
        app.register_tool_call_scope("task-1".into(), ToolCallScope::SubagentRoot);
        app.insert_active_task("task-1".into());

        let session_key = active_session_key(&app);
        apply_session_update(
            &mut app,
            SessionUpdate::AuthRequired {
                key: session_key,
                method_name: "oauth".into(),
                method_description: "Open browser".into(),
            },
        );

        assert_eq!(app.active_turn_assistant_idx(), None);
        assert!(app.active_task_ids().is_empty());
        let Some(MessageBlock::ToolCall(tc)) = app.messages()[0].blocks.first() else {
            panic!("expected tool call block");
        };
        assert_eq!(tc.status, model::ToolCallStatus::Failed);
        assert!(app.session_id().is_none());
        assert!(app.current_model().is_none());
        assert!(app.mode().is_none());
        assert_eq!(app.fast_mode_state(), model::FastModeState::Off);
    }

    #[test]
    fn fatal_event_sets_exit_error_and_quits() {
        let mut app = make_test_app();

        apply_session_update(
            &mut app,
            SessionUpdate::FatalError(crate::error::AppError::ConnectionFailed),
        );

        assert!(matches!(app.status, AppStatus::Error));
        assert!(app.should_quit);
        assert_eq!(app.exit_error, Some(crate::error::AppError::ConnectionFailed));
    }

    #[test]
    fn connection_failed_clears_active_turn_runtime_tracking() {
        let mut app = make_test_app();
        app.status = AppStatus::Running;
        app.active_messages_mut().push(assistant_msg(vec![MessageBlock::ToolCall(Box::new(
            tool_call("task-1", model::ToolCallStatus::InProgress),
        ))]));
        app.bind_active_turn_assistant(0);
        app.register_tool_call_scope("task-1".into(), ToolCallScope::SubagentRoot);
        app.insert_active_task("task-1".into());

        let session_key = active_session_key(&app);
        apply_session_update(
            &mut app,
            SessionUpdate::ConnectionFailed {
                key: session_key,
                message: "bridge down".into(),
                fatal: false,
            },
        );

        assert_eq!(app.active_turn_assistant_idx(), None);
        assert!(app.active_task_ids().is_empty());
        let Some(MessageBlock::ToolCall(tc)) = app.messages()[0].blocks.first() else {
            panic!("expected tool call block");
        };
        assert_eq!(tc.status, model::ToolCallStatus::Failed);
    }

    #[test]
    fn fatal_event_clears_active_turn_runtime_tracking() {
        let mut app = make_test_app();
        app.status = AppStatus::Running;
        app.active_messages_mut().push(assistant_msg(vec![MessageBlock::ToolCall(Box::new(
            tool_call("task-1", model::ToolCallStatus::InProgress),
        ))]));
        app.bind_active_turn_assistant(0);
        app.register_tool_call_scope("task-1".into(), ToolCallScope::SubagentRoot);
        app.insert_active_task("task-1".into());

        apply_session_update(
            &mut app,
            SessionUpdate::FatalError(crate::error::AppError::ConnectionFailed),
        );

        assert_eq!(app.active_turn_assistant_idx(), None);
        assert!(app.active_task_ids().is_empty());
        let Some(MessageBlock::ToolCall(tc)) = app.messages()[0].blocks.first() else {
            panic!("expected tool call block");
        };
        assert_eq!(tc.status, model::ToolCallStatus::Failed);
    }

    #[test]
    fn compaction_boundary_enables_compacting_and_records_boundary() {
        let mut app = make_test_app();
        assert!(!app.is_compacting());

        send_msg(
            &mut app,
            system_message(
                "compact_boundary",
                serde_json::json!({
                    "compact_metadata": {"trigger": "manual", "pre_tokens": 123_456}
                }),
            ),
        );

        assert!(app.is_compacting());
        assert!(app.pending_compact_clear());
        assert_eq!(
            app.session_usage().last_compaction_trigger,
            Some(model::CompactionTrigger::Manual)
        );
        assert_eq!(app.session_usage().last_compaction_pre_tokens, Some(123_456));
    }

    #[test]
    fn auto_compaction_boundary_sets_compacting_without_manual_success_pending() {
        let mut app = make_test_app();
        assert!(!app.is_compacting());

        send_msg(
            &mut app,
            system_message(
                "compact_boundary",
                serde_json::json!({
                    "compact_metadata": {"trigger": "auto", "pre_tokens": 234_567}
                }),
            ),
        );

        assert!(app.is_compacting());
        assert!(!app.pending_compact_clear());
        assert_eq!(
            app.session_usage().last_compaction_trigger,
            Some(model::CompactionTrigger::Auto)
        );
        assert_eq!(app.session_usage().last_compaction_pre_tokens, Some(234_567));
    }

    #[test]
    fn fast_mode_update_sets_state() {
        let mut app = make_test_app();
        assert_eq!(app.fast_mode_state(), model::FastModeState::Off);

        // Wire path: FastMode arrives via the `fast_mode_state` field
        // on a System("status") data record, parsed by
        // events::sdk_message::apply_fast_mode_update.
        send_msg(
            &mut app,
            system_message("status", serde_json::json!({"fast_mode_state": "cooldown"})),
        );

        assert_eq!(app.fast_mode_state(), model::FastModeState::Cooldown);
    }

    #[test]
    fn rate_limit_notices_dedup_and_upgrade_in_place() {
        let mut app = make_test_app();

        let warning_info = build_rate_limit_info(
            forge_primitives::RateLimitStatus::AllowedWarning,
            Some(123),
            Some(0.92),
            Some("five_hour"),
            None,
            None,
        );

        send_msg(&mut app, rate_limit_event(warning_info.clone()));
        send_msg(&mut app, rate_limit_event(warning_info.clone()));

        assert_eq!(app.messages().len(), 1);
        assert!(matches!(
            app.messages()[0].role,
            MessageRole::System(Some(SystemSeverity::Warning))
        ));
        assert!(matches!(app.messages()[0].blocks.first(), Some(MessageBlock::Notice(_))));

        let rejected_info = forge_primitives::RateLimitInfo {
            status: forge_primitives::RateLimitStatus::Rejected,
            ..warning_info
        };
        send_msg(&mut app, rate_limit_event(rejected_info.clone()));
        send_msg(&mut app, rate_limit_event(rejected_info));

        assert_eq!(app.messages().len(), 1);
        assert!(matches!(app.messages()[0].role, MessageRole::System(Some(SystemSeverity::Error))));
        assert!(first_block_text(&app.messages()[0]).contains("Rate limit reached"));
    }

    #[test]
    fn plan_limit_turn_error_upgrades_inline_notice_in_active_assistant() {
        let mut app = make_test_app();
        app.status = AppStatus::Thinking;
        app.active_messages_mut().push(user_msg("hello"));
        app.active_messages_mut().push(assistant_msg(vec![MessageBlock::Text(
            TextBlock::from_complete("partial response"),
        )]));
        app.bind_active_turn_assistant(1);

        send_msg(
            &mut app,
            rate_limit_event(build_rate_limit_info(
                forge_primitives::RateLimitStatus::AllowedWarning,
                Some(1_741_280_000),
                Some(0.95),
                Some("five_hour"),
                None,
                None,
            )),
        );
        assert_eq!(app.messages().len(), 2);
        assert_eq!(app.messages()[1].blocks.len(), 2);
        assert!(matches!(app.messages()[1].blocks[1], MessageBlock::Notice(_)));
        assert_eq!(app.turn_notice_refs().len(), 1);

        let session_key = active_session_key(&app);
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::TurnError {
                key: session_key,
                message: "HTTP 429 Too Many Requests".to_owned(),
                class: Some(forge_workspace::TurnErrorClass::PlanLimit),
                terminal_reason: None,
            },
        );

        assert!(matches!(app.status, AppStatus::Error));
        assert_eq!(app.messages().len(), 2);
        assert_eq!(app.messages()[1].blocks.len(), 2);
        let Some(MessageBlock::Notice(block)) = app.messages()[1].blocks.get(1) else {
            panic!("expected inline notice block");
        };
        assert_eq!(block.severity, SystemSeverity::Warning);
        assert!(block.text.text.contains("Approaching rate limit"));
        assert!(block.text.text.contains("Turn blocked by account or plan limits"));
        assert!(app.turn_notice_refs().is_empty());
    }

    #[test]
    fn different_rate_limit_incident_in_later_turn_keeps_older_notice() {
        let mut app = make_test_app();
        app.set_last_rate_limit_update(Some(model::RateLimitUpdate {
            status: model::RateLimitStatus::AllowedWarning,
            resets_at: Some(1_741_280_000.0),
            utilization: Some(0.95),
            rate_limit_type: Some("five_hour".to_owned()),
            overage_status: None,
            overage_resets_at: None,
            overage_disabled_reason: None,
            is_using_overage: None,
            surpassed_threshold: None,
        }));
        app.status = AppStatus::Thinking;
        app.active_messages_mut().push(user_msg("first"));
        app.active_messages_mut().push(assistant_msg(vec![]));
        app.bind_active_turn_assistant(1);

        let session_key = active_session_key(&app);
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::TurnError {
                key: session_key,
                message: "HTTP 429 Too Many Requests".to_owned(),
                class: Some(forge_workspace::TurnErrorClass::PlanLimit),
                terminal_reason: None,
            },
        );

        assert_eq!(app.messages().len(), 2);
        let first_notice_text = match app.messages()[1].blocks.as_slice() {
            [MessageBlock::Notice(block)] => block.text.text.clone(),
            _ => panic!("expected first turn notice"),
        };
        assert!(first_notice_text.contains("Approaching rate limit"));

        app.status = AppStatus::Thinking;
        app.active_messages_mut().push(user_msg("second"));
        app.active_messages_mut().push(assistant_msg(vec![]));
        app.bind_active_turn_assistant(3);
        send_msg(
            &mut app,
            rate_limit_event(build_rate_limit_info(
                forge_primitives::RateLimitStatus::Rejected,
                Some(1_741_290_000),
                None,
                Some("daily"),
                None,
                None,
            )),
        );

        assert_eq!(app.messages().len(), 4);
        let Some(MessageBlock::Notice(first_notice)) = app.messages()[1].blocks.first() else {
            panic!("expected first turn notice");
        };
        assert_eq!(first_notice.text.text, first_notice_text);
        let Some(MessageBlock::Notice(second_notice)) = app.messages()[3].blocks.first() else {
            panic!("expected second turn notice");
        };
        assert!(second_notice.text.text.contains("daily rate limit"));
        assert_ne!(second_notice.text.text, first_notice_text);
    }

    /// A Standalone notice (inserted with no active assistant turn)
    /// must be promotable to Inline when an assistant turn becomes
    /// active and the same `dedup_key` re-upserts: exactly one ref,
    /// of type Inline, no panic.
    #[test]
    fn upsert_turn_notice_promotes_standalone_to_inline_without_panic() {
        let mut app = make_test_app();

        let info = build_rate_limit_info(
            forge_primitives::RateLimitStatus::AllowedWarning,
            Some(123),
            Some(0.92),
            Some("five_hour"),
            None,
            None,
        );

        // 1. No active assistant turn -> upsert lands as Standalone.
        send_msg(&mut app, rate_limit_event(info.clone()));
        assert_eq!(app.turn_notice_refs().len(), 1);
        assert!(
            matches!(app.turn_notice_refs()[0].location, TurnNoticeLocation::Standalone { .. }),
            "precondition: first upsert is Standalone, got {:?}",
            app.turn_notice_refs()[0].location,
        );

        // 2. Bind an active assistant turn so the next upsert takes
        // the standalone->inline promotion branch.
        app.status = AppStatus::Thinking;
        app.active_messages_mut().push(user_msg("hello"));
        app.active_messages_mut().push(assistant_msg(vec![]));
        app.bind_active_turn_assistant(app.messages().len() - 1);

        // 3. Same dedup_key, now with active turn -> promote-to-inline
        // success-path.
        send_msg(&mut app, rate_limit_event(info));

        assert_eq!(app.turn_notice_refs().len(), 1, "exactly one ref after promotion",);
        assert!(
            matches!(app.turn_notice_refs()[0].location, TurnNoticeLocation::Inline { .. }),
            "promoted ref must be Inline, got {:?}",
            app.turn_notice_refs()[0].location,
        );
    }

    #[test]
    fn turn_notice_tracking_clears_on_turn_complete_and_session_reset() {
        let mut app = make_test_app();
        app.status = AppStatus::Thinking;
        app.active_messages_mut().push(user_msg("hello"));
        app.active_messages_mut().push(assistant_msg(vec![]));
        app.bind_active_turn_assistant(1);

        send_msg(
            &mut app,
            rate_limit_event(build_rate_limit_info(
                forge_primitives::RateLimitStatus::AllowedWarning,
                Some(123),
                Some(0.91),
                Some("five_hour"),
                None,
                None,
            )),
        );

        assert_eq!(app.turn_notice_refs().len(), 1);
        let session_key = active_session_key(&app);
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::TurnComplete {
                key: session_key,
                terminal_reason: None,
            },
        );
        assert!(app.turn_notice_refs().is_empty());

        app.status = AppStatus::Thinking;
        app.active_messages_mut().push(user_msg("again"));
        app.active_messages_mut().push(assistant_msg(vec![]));
        app.bind_active_turn_assistant(app.messages().len() - 1);
        send_msg(
            &mut app,
            rate_limit_event(build_rate_limit_info(
                forge_primitives::RateLimitStatus::AllowedWarning,
                Some(456),
                Some(0.92),
                Some("daily"),
                None,
                None,
            )),
        );
        assert_eq!(app.turn_notice_refs().len(), 1);

        // `SessionReplaced` fires on /new, /login, /resume against the
        // active bucket; a Connected event for a different key is a
        // background project's connection, not a session reset. The
        // notice-clearing assertion targets SessionReplaced on the
        // active key.
        let active_key = active_session_key(&app);
        apply_session_update(
            &mut app,
            SessionUpdate::SessionReplaced {
                key: active_key,
                session_id: forge_primitives::SessionId::new("new-session"),
                cwd: "/test".into(),
                current_model: test_current_model_primitives("claude"),
                available_models: Vec::new(),
                mode: None,
                history: Vec::new(),
            },
        );
        assert!(app.turn_notice_refs().is_empty());
    }

    #[test]
    fn turn_error_after_cancel_shows_interrupted_hint_instead_of_error_block() {
        let mut app = make_test_app();
        app.active_messages_mut().push(user_msg("build app"));

        let session_key = active_session_key(&app);
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::TurnCancelled { key: session_key },
        );
        assert!(app.cancelled_turn_pending_hint());

        let session_key = active_session_key(&app);
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::TurnError {
                key: session_key,
                message: "Error: Request was aborted.\n    at stack line".into(),
                class: None,
                terminal_reason: None,
            },
        );

        assert!(!app.cancelled_turn_pending_hint());
        assert!(matches!(app.status, AppStatus::Ready));

        let Some(last) = app.messages().last() else {
            panic!("expected interruption hint message");
        };
        assert!(matches!(last.role, MessageRole::System(Some(SystemSeverity::Info))));
        let Some(MessageBlock::Text(block)) = last.blocks.first() else {
            panic!("expected text block");
        };
        assert_eq!(block.text, "Conversation interrupted. Tell the model how to proceed.");
    }

    #[test]
    fn turn_cancel_marks_active_tools_failed() {
        let mut app = make_test_app();
        app.active_messages_mut().push(assistant_msg(vec![
            MessageBlock::ToolCall(Box::new(tool_call("tc1", model::ToolCallStatus::InProgress))),
            MessageBlock::ToolCall(Box::new(tool_call("tc2", model::ToolCallStatus::Pending))),
            MessageBlock::ToolCall(Box::new(tool_call("tc3", model::ToolCallStatus::Completed))),
        ]));

        let session_key = active_session_key(&app);
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::TurnCancelled { key: session_key },
        );

        let Some(last) = app.messages().last() else {
            panic!("missing assistant message");
        };
        let statuses: Vec<model::ToolCallStatus> = last
            .blocks
            .iter()
            .filter_map(|b| match b {
                MessageBlock::ToolCall(tc) => Some(tc.status),
                _ => None,
            })
            .collect();
        assert_eq!(
            statuses,
            vec![
                model::ToolCallStatus::Failed,
                model::ToolCallStatus::Failed,
                model::ToolCallStatus::Completed
            ]
        );
    }

    #[test]
    fn turn_complete_marks_lingering_tools_completed() {
        let mut app = make_test_app();
        app.active_messages_mut().push(assistant_msg(vec![
            MessageBlock::ToolCall(Box::new(tool_call("tc1", model::ToolCallStatus::InProgress))),
            MessageBlock::ToolCall(Box::new(tool_call("tc2", model::ToolCallStatus::Pending))),
        ]));

        let session_key = active_session_key(&app);
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::TurnComplete {
                key: session_key,
                terminal_reason: None,
            },
        );

        let Some(last) = app.messages().last() else {
            panic!("missing assistant message");
        };
        let statuses: Vec<model::ToolCallStatus> = last
            .blocks
            .iter()
            .filter_map(|b| match b {
                MessageBlock::ToolCall(tc) => Some(tc.status),
                _ => None,
            })
            .collect();
        assert_eq!(
            statuses,
            vec![model::ToolCallStatus::Completed, model::ToolCallStatus::Completed]
        );
    }

    #[test]
    fn ctrl_v_not_inserted_by_chat_key_handlers() {
        for handler in [
            handle_normal_key as fn(&mut App, KeyEvent),
            handle_mention_key as fn(&mut App, KeyEvent),
        ] {
            let mut app = make_test_app();
            handler(&mut app, KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL));
            assert_eq!(app.input().text(), "");
        }
    }

    #[test]
    fn pending_paste_payload_blocks_overlapping_key_text_insertion() {
        let mut app = make_test_app();
        *app.pending_paste_text_mut() = "clipboard".to_owned();

        handle_normal_key(&mut app, KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));

        assert_eq!(app.input().text(), "");
    }

    #[test]
    fn altgr_at_inserts_char_and_activates_mention() {
        let mut app = make_test_app();
        handle_normal_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('@'), KeyModifiers::CONTROL | KeyModifiers::ALT),
        );

        assert_eq!(app.input().text(), "@");
        assert!(app.mention().is_some());
    }

    #[test]
    fn word_nav_backspace_and_delete_use_word_operations() {
        let mut app = make_test_app();
        app.input_mut().set_text("hello world");

        handle_normal_key(&mut app, KeyEvent::new(KeyCode::Backspace, WORD_NAV_MOD));
        assert_eq!(app.input().text(), "hello ");

        app.input_mut().move_home();
        handle_normal_key(&mut app, KeyEvent::new(KeyCode::Delete, WORD_NAV_MOD));
        assert_eq!(app.input().text(), " ");
    }

    #[test]
    fn cmd_z_and_redo_undo_and_redo_textarea_history() {
        let mut app = make_test_app();
        app.input_mut().set_text("hello world");

        handle_normal_key(&mut app, KeyEvent::new(KeyCode::Backspace, WORD_NAV_MOD));
        assert_eq!(app.input().text(), "hello ");

        handle_normal_key(&mut app, KeyEvent::new(KeyCode::Char('z'), CMD_MOD));
        assert_eq!(app.input().text(), "hello world");

        // Redo: Cmd+Shift+Z on macOS (reported as 'Z' with SUPER), Ctrl+Y elsewhere.
        #[cfg(target_os = "macos")]
        handle_normal_key(&mut app, KeyEvent::new(KeyCode::Char('Z'), CMD_MOD));
        #[cfg(not(target_os = "macos"))]
        handle_normal_key(&mut app, KeyEvent::new(KeyCode::Char('y'), CMD_MOD));
        assert_eq!(app.input().text(), "hello ");
    }

    #[test]
    fn word_nav_left_right_move_by_word() {
        let mut app = make_test_app();
        app.input_mut().set_text("hello world");
        app.input_mut().move_home();

        handle_normal_key(&mut app, KeyEvent::new(KeyCode::Right, WORD_NAV_MOD));
        assert!(app.input().cursor_col() > 0);

        handle_normal_key(&mut app, KeyEvent::new(KeyCode::Left, WORD_NAV_MOD));
        assert_eq!(app.input().cursor_col(), 0);
    }

    #[test]
    fn help_overlay_left_right_switches_help_view_tab() {
        let mut app = make_test_app();
        app.input_mut().set_text("?");
        app.help_open = true;
        app.help_view = HelpView::Keys;

        dispatch_key_by_focus(&mut app, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(app.help_view, HelpView::SlashCommands);

        dispatch_key_by_focus(&mut app, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(app.help_view, HelpView::Subagents);

        dispatch_key_by_focus(&mut app, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(app.help_view, HelpView::SlashCommands);

        dispatch_key_by_focus(&mut app, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(app.help_view, HelpView::Keys);
    }

    #[test]
    fn connecting_state_allows_navigation_and_help_shortcuts() {
        let mut app = make_test_app();
        app.status = AppStatus::Connecting;
        app.help_view = HelpView::Keys;
        app.active_viewport_mut().scroll_target = 2;

        // Chat navigation remains available during startup.
        handle_terminal_event(&mut app, Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)));
        assert_eq!(app.viewport().scroll_target, 1);
        handle_terminal_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        );
        assert_eq!(app.viewport().scroll_target, 2);

        // Help toggle via "?" remains available.
        handle_terminal_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE)),
        );
        assert!(app.is_help_active());

        // Help tab navigation still works.
        handle_terminal_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
        );
        assert_eq!(app.help_view, HelpView::SlashCommands);
    }

    #[test]
    fn connecting_state_blocks_input_shortcuts_and_tab() {
        let mut app = make_test_app();
        app.status = AppStatus::Connecting;
        app.input_mut().set_text("seed");
        *app.pending_submit_mut() = None;
        app.help_view = HelpView::Keys;

        for key in [
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('@'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        ] {
            handle_terminal_event(&mut app, Event::Key(key));
        }

        assert_eq!(app.input().text(), "seed");
        assert!(app.pending_submit().is_none());
        assert_eq!(app.help_view, HelpView::Keys);
    }

    #[test]
    fn ctrl_c_with_non_empty_selection_does_not_quit_and_clears_selection() {
        let mut app = make_test_app();
        let _clipboard =
            crate::app::keys::override_test_clipboard(crate::app::keys::TestClipboardMode::Succeed);
        app.rendered_input_lines = vec!["copy".to_owned()];
        *app.selection_mut() = Some(crate::app::SelectionState {
            kind: crate::app::SelectionKind::Input,
            start: crate::app::SelectionPoint { row: 0, col: 0 },
            end: crate::app::SelectionPoint { row: 0, col: 4 },
            dragging: false,
        });

        handle_terminal_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        );

        assert!(!app.should_quit);
        assert!(app.selection().is_none());
    }

    #[test]
    fn ctrl_c_without_selection_quits() {
        let mut app = make_test_app();
        *app.selection_mut() = None;

        handle_terminal_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        );

        assert!(app.should_quit);
    }

    #[test]
    fn ctrl_c_second_press_after_copy_quits() {
        let mut app = make_test_app();
        let _clipboard =
            crate::app::keys::override_test_clipboard(crate::app::keys::TestClipboardMode::Succeed);
        app.rendered_input_lines = vec!["copy".to_owned()];
        *app.selection_mut() = Some(crate::app::SelectionState {
            kind: crate::app::SelectionKind::Input,
            start: crate::app::SelectionPoint { row: 0, col: 0 },
            end: crate::app::SelectionPoint { row: 0, col: 4 },
            dragging: false,
        });

        handle_terminal_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        );
        assert!(!app.should_quit);
        assert!(app.selection().is_none());

        handle_terminal_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        );
        assert!(app.should_quit);
    }

    #[test]
    fn ctrl_c_with_clipboard_failure_preserves_selection_without_quitting() {
        let mut app = make_test_app();
        let _clipboard =
            crate::app::keys::override_test_clipboard(crate::app::keys::TestClipboardMode::Fail);
        app.rendered_input_lines = vec!["copy".to_owned()];
        *app.selection_mut() = Some(crate::app::SelectionState {
            kind: crate::app::SelectionKind::Input,
            start: crate::app::SelectionPoint { row: 0, col: 0 },
            end: crate::app::SelectionPoint { row: 0, col: 4 },
            dragging: false,
        });

        handle_terminal_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        );

        assert!(!app.should_quit);
        assert!(app.selection().is_some());
    }

    #[test]
    fn ctrl_c_with_zero_length_selection_quits() {
        let mut app = make_test_app();
        app.rendered_input_lines = vec!["copy".to_owned()];
        *app.selection_mut() = Some(crate::app::SelectionState {
            kind: crate::app::SelectionKind::Input,
            start: crate::app::SelectionPoint { row: 0, col: 0 },
            end: crate::app::SelectionPoint { row: 0, col: 0 },
            dragging: false,
        });

        handle_terminal_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        );

        assert!(app.should_quit);
    }

    #[test]
    fn ctrl_c_with_whitespace_selection_copies_and_clears_selection() {
        let mut app = make_test_app();
        let _clipboard =
            crate::app::keys::override_test_clipboard(crate::app::keys::TestClipboardMode::Succeed);
        app.rendered_input_lines = vec!["   ".to_owned()];
        *app.selection_mut() = Some(crate::app::SelectionState {
            kind: crate::app::SelectionKind::Input,
            start: crate::app::SelectionPoint { row: 0, col: 0 },
            end: crate::app::SelectionPoint { row: 0, col: 1 },
            dragging: false,
        });

        handle_terminal_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        );

        assert!(!app.should_quit);
        assert!(app.selection().is_none());
    }

    #[test]
    fn ctrl_q_quits_even_with_selection() {
        let mut app = make_test_app();
        *app.selection_mut() = Some(crate::app::SelectionState {
            kind: crate::app::SelectionKind::Input,
            start: crate::app::SelectionPoint { row: 0, col: 0 },
            end: crate::app::SelectionPoint { row: 0, col: 0 },
            dragging: false,
        });

        handle_terminal_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        );

        assert!(app.should_quit);
    }

    #[test]
    fn connecting_state_ctrl_q_quits() {
        let mut app = make_test_app();
        app.status = AppStatus::Connecting;

        handle_terminal_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        );

        assert!(app.should_quit);
    }

    #[test]
    fn error_state_blocks_input_shortcuts() {
        let mut app = make_test_app();
        app.status = AppStatus::Error;
        app.input_mut().set_text("seed");
        *app.pending_submit_mut() = None;

        for key in [
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('@'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        ] {
            handle_terminal_event(&mut app, Event::Key(key));
        }

        assert_eq!(app.input().text(), "seed");
        assert!(app.pending_submit().is_none());
    }

    #[test]
    fn error_state_ctrl_q_quits() {
        let mut app = make_test_app();
        app.status = AppStatus::Error;

        handle_terminal_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        );

        assert!(app.should_quit);
    }

    #[test]
    fn error_state_ctrl_c_quits() {
        let mut app = make_test_app();
        app.status = AppStatus::Error;

        handle_terminal_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        );

        assert!(app.should_quit);
    }

    #[test]
    fn error_state_blocks_paste_events() {
        let mut app = make_test_app();
        app.status = AppStatus::Error;

        handle_terminal_event(&mut app, Event::Paste("blocked".into()));

        assert!(app.pending_paste_text().is_empty());
        assert!(app.input().is_empty());
    }

    #[test]
    fn mouse_scroll_clears_selection_before_scrolling() {
        let mut app = make_test_app();
        app.active_viewport_mut().scroll_target = 2;
        *app.selection_mut() = Some(crate::app::SelectionState {
            kind: crate::app::SelectionKind::Chat,
            start: crate::app::SelectionPoint { row: 0, col: 0 },
            end: crate::app::SelectionPoint { row: 0, col: 1 },
            dragging: false,
        });

        handle_terminal_event(
            &mut app,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            }),
        );

        assert!(app.selection().is_none());
        assert_eq!(app.viewport().scroll_target, 5);
    }

    #[test]
    fn mouse_down_on_scrollbar_rail_starts_drag_and_scrolls() {
        let mut app = make_test_app();
        app.rendered_chat_area = Rect::new(0, 0, 19, 10);
        app.active_viewport_mut().height_prefix_sums = vec![30];
        app.active_viewport_mut().scrollbar_thumb_top = 0.0;
        app.active_viewport_mut().scrollbar_thumb_size = 3.0;
        *app.selection_mut() = Some(crate::app::SelectionState {
            kind: crate::app::SelectionKind::Chat,
            start: crate::app::SelectionPoint { row: 0, col: 0 },
            end: crate::app::SelectionPoint { row: 0, col: 1 },
            dragging: false,
        });

        handle_terminal_event(
            &mut app,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: 19,
                row: 9,
                modifiers: KeyModifiers::NONE,
            }),
        );

        assert!(app.scrollbar_drag.is_some());
        assert!(app.selection().is_none());
        assert!(!app.viewport().auto_scroll);
        assert!(app.viewport().scroll_target > 0);
    }

    #[test]
    fn dragging_scrollbar_thumb_can_reach_bottom_and_top() {
        let mut app = make_test_app();
        app.rendered_chat_area = Rect::new(0, 0, 19, 10);
        app.active_viewport_mut().height_prefix_sums = vec![30];
        app.active_viewport_mut().scrollbar_thumb_top = 0.0;
        app.active_viewport_mut().scrollbar_thumb_size = 3.0;

        handle_terminal_event(
            &mut app,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: 19,
                row: 0,
                modifiers: KeyModifiers::NONE,
            }),
        );
        handle_terminal_event(
            &mut app,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Drag(crossterm::event::MouseButton::Left),
                column: 19,
                row: 9,
                modifiers: KeyModifiers::NONE,
            }),
        );
        assert_eq!(app.viewport().scroll_target, 20);

        handle_terminal_event(
            &mut app,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Drag(crossterm::event::MouseButton::Left),
                column: 19,
                row: 0,
                modifiers: KeyModifiers::NONE,
            }),
        );
        assert_eq!(app.viewport().scroll_target, 0);

        handle_terminal_event(
            &mut app,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Up(crossterm::event::MouseButton::Left),
                column: 19,
                row: 0,
                modifiers: KeyModifiers::NONE,
            }),
        );
        assert!(app.scrollbar_drag.is_none());
    }

    #[test]
    fn click_on_tool_call_row_flips_per_tool_collapse_override() {
        // Build a single tool-call message and pre-populate the
        // measurement caches the hit-test relies on. Without these
        // the helper bails out (block_visible_height returns None).
        let mut app = make_test_app();
        let (msg_idx, block_idx) = append_tool_call_block(&mut app, "tool-1");
        let chat_width: u16 = 40;
        let tool_height: usize = 4;
        let layout_generation = app.viewport().layout_generation;
        if let MessageBlock::ToolCall(tc) =
            &mut app.active_messages_mut()[msg_idx].blocks[block_idx]
        {
            tc.last_measured_width = chat_width;
            tc.last_measured_height = tool_height;
            tc.last_measured_y_in_msg = 0;
            tc.last_measured_layout_epoch = tc.layout_epoch;
            tc.last_measured_layout_generation = layout_generation;
        } else {
            panic!("seeded tool-call block not found");
        }
        app.active_viewport_mut().height_prefix_sums = vec![tool_height];
        app.active_viewport_mut().scroll_offset = 0;
        app.rendered_chat_area = Rect::new(0, 0, chat_width, 10);
        app.tools_collapsed = false;

        // With `tools_collapsed = false`, the resolver defaults groups
        // to L0Bodies. A click at L0 or L2 routes to the group cycle
        // (NOT the per-tool toggle this test exercises), so we cycle
        // twice (L0 -> L2 -> L1) to land at L1Titles where the click
        // hits the per-tool path.
        let leader_id = crate::ui::message::grouping::GroupId::from_leader_id("tool-1");
        let _ = app.cycle_group_collapse_level(&leader_id);
        let _ = app.cycle_group_collapse_level(&leader_id);

        // Click somewhere inside the tool call's rendered y-range.
        handle_terminal_event(
            &mut app,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: 10,
                row: 1,
                modifiers: KeyModifiers::NONE,
            }),
        );
        let MessageBlock::ToolCall(tc) = &app.messages()[msg_idx].blocks[block_idx] else {
            panic!("tool-call block missing post-click");
        };
        assert_eq!(tc.collapsed_override, Some(true));
        // Selection should NOT have started - click was consumed.
        assert!(app.selection().is_none());

        // mark_tool_call_layout_dirty zeroed the cached measurement so a
        // real re-render would re-fill it. The test doesn't run the
        // render pass, so re-prime manually before the second click.
        let layout_generation = app.viewport().layout_generation;
        if let MessageBlock::ToolCall(tc) =
            &mut app.active_messages_mut()[msg_idx].blocks[block_idx]
        {
            tc.last_measured_width = chat_width;
            tc.last_measured_height = tool_height;
            tc.last_measured_y_in_msg = 0;
            tc.last_measured_layout_epoch = tc.layout_epoch;
            tc.last_measured_layout_generation = layout_generation;
        }

        // A second click toggles back.
        handle_terminal_event(
            &mut app,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: 10,
                row: 1,
                modifiers: KeyModifiers::NONE,
            }),
        );
        let MessageBlock::ToolCall(tc) = &app.messages()[msg_idx].blocks[block_idx] else {
            panic!("tool-call block missing post-second-click");
        };
        assert_eq!(tc.collapsed_override, Some(false));
    }

    #[test]
    fn click_outside_tool_call_blocks_falls_through_to_selection() {
        // Mixed message: a leading text block then a tool-call block.
        // Click on row 0 should land on the text block (no toggle, just
        // a chat selection).
        let mut app = make_test_app();
        let chat_width: u16 = 40;

        let mut text_block = TextBlock::from_complete("hello\nworld");
        text_block.cache.set_height(2, chat_width);

        let mut tool = tool_call("tool-x", model::ToolCallStatus::InProgress);
        tool.last_measured_width = chat_width;
        tool.last_measured_height = 3;
        tool.last_measured_y_in_msg = 2; // sits after the 2-row text block

        app.active_messages_mut().push(assistant_msg(vec![
            MessageBlock::Text(text_block),
            MessageBlock::ToolCall(Box::new(tool)),
        ]));
        app.index_tool_call("tool-x".into(), 0, 1);
        app.active_viewport_mut().height_prefix_sums = vec![5]; // text(2) + tool(3)
        app.active_viewport_mut().scroll_offset = 0;
        app.rendered_chat_area = Rect::new(0, 0, chat_width, 10);

        // Click on the text portion (row 0) inside the chat area.
        handle_terminal_event(
            &mut app,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: 4,
                row: 0,
                modifiers: KeyModifiers::NONE,
            }),
        );
        let MessageBlock::ToolCall(tc) = &app.messages()[0].blocks[1] else {
            panic!("tool-call block missing");
        };
        assert!(tc.collapsed_override.is_none());
        // A text-area click should have started a selection.
        assert!(app.selection().is_some());
    }

    #[test]
    fn dragging_uses_displayed_thumb_track_when_scrollbar_is_smoothed() {
        let mut app = make_test_app();
        app.rendered_chat_area = Rect::new(0, 0, 19, 10);
        app.active_viewport_mut().height_prefix_sums = vec![30];
        app.active_viewport_mut().scrollbar_thumb_top = 2.0;
        app.active_viewport_mut().scrollbar_thumb_size = 6.0;

        handle_terminal_event(
            &mut app,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: 19,
                row: 7,
                modifiers: KeyModifiers::NONE,
            }),
        );
        handle_terminal_event(
            &mut app,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Drag(crossterm::event::MouseButton::Left),
                column: 19,
                row: 9,
                modifiers: KeyModifiers::NONE,
            }),
        );

        assert_eq!(app.viewport().scroll_target, 20);
    }

    #[test]
    fn up_down_without_focus_scrolls_chat() {
        let mut app = make_test_app();
        app.active_viewport_mut().scroll_target = 5;
        app.active_viewport_mut().auto_scroll = true;

        handle_normal_key(&mut app, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.viewport().scroll_target, 4);
        assert!(!app.viewport().auto_scroll);

        handle_normal_key(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.viewport().scroll_target, 5);
    }

    #[test]
    fn up_down_moves_input_cursor_when_multiline() {
        let mut app = make_test_app();
        app.input_mut().set_text("line1\nline2\nline3");
        let _ = app.input_mut().set_cursor(1, 3);
        app.active_viewport_mut().scroll_target = 7;

        handle_normal_key(&mut app, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.input().cursor_row(), 0);
        assert_eq!(app.viewport().scroll_target, 7);

        handle_normal_key(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.input().cursor_row(), 1);
        assert_eq!(app.viewport().scroll_target, 7);
    }

    #[test]
    fn down_at_input_bottom_falls_back_to_chat_scroll() {
        let mut app = make_test_app();
        app.input_mut().set_text("line1\nline2");
        let _ = app.input_mut().set_cursor(1, 0);
        app.active_viewport_mut().scroll_target = 2;

        handle_normal_key(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));

        assert_eq!(app.input().cursor_row(), 1);
        assert_eq!(app.viewport().scroll_target, 3);
    }

    #[test]
    fn settings_view_routes_enter_to_close_not_chat_submit() {
        let mut app = make_test_app();
        let dir = tempfile::tempdir().expect("tempdir");
        app.settings_home_override = Some(dir.path().to_path_buf());
        app.set_cwd_raw(dir.path().to_string_lossy().to_string());
        crate::app::config::open_plugins(&mut app).expect("open plugins");
        app.input_mut().set_text("seed");

        handle_terminal_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );

        assert_eq!(app.active_view, ActiveView::Chat);
        assert_eq!(app.input().text(), "seed");
        assert!(app.pending_submit().is_none());
    }

    #[test]
    fn settings_view_ignores_paste_events() {
        let mut app = make_test_app();
        app.active_view = ActiveView::Plugins;

        handle_terminal_event(&mut app, Event::Paste("blocked".into()));

        assert!(app.pending_paste_text().is_empty());
        assert!(app.input().is_empty());
    }

    #[test]
    fn clipboard_paste_shortcut_dispatches_on_release() {
        let key = crossterm::event::KeyEvent {
            code: KeyCode::Char('v'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Release,
            state: crossterm::event::KeyEventState::NONE,
        };
        assert!(should_dispatch_key_event(key));
    }

    #[test]
    fn non_paste_shortcut_release_is_ignored() {
        let key = crossterm::event::KeyEvent {
            code: KeyCode::Char('q'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Release,
            state: crossterm::event::KeyEventState::NONE,
        };
        assert!(!should_dispatch_key_event(key));
    }

    #[test]
    fn settings_view_ignores_mouse_events() {
        let mut app = make_test_app();
        app.active_view = ActiveView::Plugins;
        app.active_viewport_mut().scroll_target = 4;
        *app.selection_mut() = Some(SelectionState {
            kind: SelectionKind::Chat,
            start: SelectionPoint { row: 0, col: 0 },
            end: SelectionPoint { row: 0, col: 1 },
            dragging: false,
        });

        handle_terminal_event(
            &mut app,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            }),
        );

        assert_eq!(app.viewport().scroll_target, 4);
        assert!(app.selection().is_some());
    }

    #[test]
    fn buffered_paste_char_does_not_force_redraw() {
        let mut app = make_test_app();
        let now = Instant::now();

        assert_eq!(
            app.paste_burst.on_char('a', now),
            super::super::paste_burst::CharAction::Passthrough('a')
        );
        assert_eq!(
            app.paste_burst.on_char('b', now + Duration::from_millis(1)),
            super::super::paste_burst::CharAction::Consumed
        );
        assert_eq!(
            app.paste_burst.on_char('c', now + Duration::from_millis(2)),
            super::super::paste_burst::CharAction::RetroCapture(1)
        );

        app.needs_redraw = false;
        handle_terminal_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)),
        );

        assert!(!app.needs_redraw);
        assert!(app.input().is_empty());
    }

    #[test]
    fn api_retry_updates_single_warning_notice() {
        let mut app = make_test_app();
        // Wire path: ApiRetryUpdate arrives as System("api_retry") with
        // attempt / max_retries / retry_delay_ms / error_status / error
        // fields parsed by build_api_retry_update.
        send_msg(
            &mut app,
            system_message(
                "api_retry",
                serde_json::json!({
                    "attempt": 1,
                    "max_retries": 4,
                    "retry_delay_ms": 1000,
                    "error_status": null,
                    "error": "unknown",
                }),
            ),
        );
        send_msg(
            &mut app,
            system_message(
                "api_retry",
                serde_json::json!({
                    "attempt": 2,
                    "max_retries": 4,
                    "retry_delay_ms": 1500,
                    "error_status": 529,
                    "error": "server_error",
                }),
            ),
        );

        assert_eq!(app.messages().len(), 1);
        assert_eq!(app.turn_notice_refs().len(), 1);
        let MessageBlock::Notice(notice) = &app.messages()[0].blocks[0] else {
            panic!("expected API retry notice");
        };
        assert_eq!(notice.severity, SystemSeverity::Warning);
        assert_eq!(notice.text.text, "API retry 2/4 after server_error HTTP 529, retrying in 1.5s",);
    }

    #[test]
    fn prompt_suggestion_tab_accepts_empty_input_only_after_todo_focus() {
        let mut app = make_test_app();
        app.set_prompt_suggestion(Some("Write focused tests".to_owned()));

        handle_normal_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

        assert_eq!(app.input().text(), "Write focused tests");
        assert!(app.prompt_suggestion().is_none());
    }

    #[test]
    fn runtime_session_state_updates_status_with_guards() {
        let mut app = make_test_app();
        // Wire path: RuntimeSessionStateUpdate arrives as
        // System("session_state_changed") with a `state` field.
        send_msg(
            &mut app,
            system_message("session_state_changed", serde_json::json!({"state": "running"})),
        );
        assert_eq!(app.runtime_session_state(), Some(model::RuntimeSessionState::Running));
        assert!(matches!(app.status, AppStatus::Running));

        app.status = AppStatus::Error;
        send_msg(
            &mut app,
            system_message("session_state_changed", serde_json::json!({"state": "idle"})),
        );
        assert!(matches!(app.status, AppStatus::Error));
    }

    #[test]
    fn settings_parse_error_surfaces_system_error_message() {
        let mut app = make_test_app();
        // Wire path: SettingsParseError arrives as a `settings_errors`
        // entry inside a System("init") data record.
        send_msg(
            &mut app,
            system_message(
                "init",
                serde_json::json!({
                    "settings_errors": [{
                        "file": "C:/work/.claude/settings.json",
                        "path": "permissions.allow",
                        "message": "Expected array",
                    }]
                }),
            ),
        );

        assert_eq!(app.messages().len(), 1);
        assert!(matches!(app.messages()[0].role, MessageRole::System(Some(SystemSeverity::Error))));
        let MessageBlock::Text(text) = &app.messages()[0].blocks[0] else {
            panic!("expected settings parse error text");
        };
        assert_eq!(
            text.text,
            "Settings parse error in C:/work/.claude/settings.json at permissions.allow: Expected array",
        );
    }

    #[test]
    fn internal_error_detection_accepts_xml_payload() {
        use forge_workspace::translate::error_handling::looks_like_internal_error;
        let payload =
            "<error><code>-32603</code><message>Adapter process crashed</message></error>";
        assert!(looks_like_internal_error(payload));
    }

    #[test]
    fn internal_error_detection_rejects_plain_bash_failure() {
        use forge_workspace::translate::error_handling::looks_like_internal_error;
        let payload = "bash: unknown_command: command not found";
        assert!(!looks_like_internal_error(payload));
    }

    #[test]
    fn summarize_internal_error_prefers_xml_message() {
        use forge_workspace::translate::error_handling::summarize_internal_error;
        let payload =
            "<error><code>-32603</code><message>Adapter process crashed</message></error>";
        assert_eq!(summarize_internal_error(payload), "Adapter process crashed");
    }

    #[test]
    fn summarize_internal_error_reads_json_rpc_message() {
        use forge_workspace::translate::error_handling::summarize_internal_error;
        let payload = r#"{"jsonrpc":"2.0","error":{"code":-32603,"message":"internal rpc fault"}}"#;
        assert_eq!(summarize_internal_error(payload), "internal rpc fault");
    }

    #[test]
    fn internal_error_detection_accepts_permission_zod_payload() {
        use forge_workspace::translate::error_handling::looks_like_internal_error;
        let payload = "Tool permission request failed: ZodError: [{\"message\":\"Invalid input\"}]";
        assert!(looks_like_internal_error(payload));
    }

    #[test]
    fn summarize_internal_error_prefers_permission_failure_summary() {
        use forge_workspace::translate::error_handling::summarize_internal_error;
        let payload = "Tool permission request failed: ZodError: [{\"message\":\"Invalid input: expected record, received undefined\"}]";
        assert_eq!(
            summarize_internal_error(payload),
            "Tool permission request failed: Invalid input: expected record, received undefined"
        );
    }
}
