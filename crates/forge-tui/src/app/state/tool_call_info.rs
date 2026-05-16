use super::block_cache::BlockCache;
use crate::agent::model;

pub struct ToolCallInfo {
    pub id: String,
    pub title: String,
    /// The SDK tool name from `meta.claudeCode.toolName` when available.
    /// Falls back to a derived name when metadata is absent.
    pub sdk_tool_name: String,
    pub raw_input: Option<serde_json::Value>,
    pub raw_input_bytes: usize,
    pub output_metadata: Option<model::ToolOutputMetadata>,
    pub task_metadata: Option<model::TaskMetadata>,
    pub status: model::ToolCallStatus,
    pub content: Vec<model::ToolCallContent>,
    /// Hidden tool calls are subagent children - not rendered directly.
    pub hidden: bool,
    /// Terminal ID if this is a Bash-like SDK tool call with a running/completed terminal.
    pub terminal_id: Option<String>,
    /// The shell command that was executed (e.g. "echo hello && ls -la").
    pub terminal_command: Option<String>,
    /// Snapshot of terminal output, updated each frame while `InProgress`.
    pub terminal_output: Option<String>,
    /// Length of terminal buffer at last snapshot - used to skip O(n) re-snapshots
    /// when the buffer hasn't grown.
    pub terminal_output_len: usize,
    /// Number of terminal output bytes consumed for incremental append updates.
    pub terminal_bytes_seen: usize,
    /// Current terminal snapshot ingestion mode.
    pub terminal_snapshot_mode: TerminalSnapshotMode,
    /// Monotonic generation for render-affecting changes.
    pub render_epoch: u64,
    /// Monotonic generation for layout-affecting changes.
    pub layout_epoch: u64,
    /// Last measured width used by tool-call height cache.
    pub last_measured_width: u16,
    /// Last measured visual height in wrapped rows.
    pub last_measured_height: usize,
    /// Layout epoch used for the last measured height.
    pub last_measured_layout_epoch: u64,
    /// Global layout generation used for the last measured height.
    pub last_measured_layout_generation: u64,
    /// Per-block render cache for this tool call.
    pub cache: BlockCache,
    /// Inline permission prompt - rendered inside this tool call block.
    pub pending_permission: Option<InlinePermission>,
    /// Inline question prompt from `AskUserQuestion`.
    pub pending_question: Option<InlineQuestion>,
    /// Per-tool collapse override set by clicking the tool-call row.
    /// `None` means follow the global `app.tools_collapsed` default;
    /// `Some(true)` forces collapsed, `Some(false)` forces expanded.
    /// Hashed into the message render signature so flipping it
    /// invalidates the message-level render cache.
    pub collapsed_override: Option<bool>,
    /// Wrapped-row offset of this tool inside its parent message,
    /// captured during the assistant render pass. Combined with
    /// `last_measured_height` (and `viewport.cumulative_height_before`)
    /// it gives the absolute y-range used by mouse-click hit-testing.
    /// Cleared back to 0 whenever the tool isn't currently rendered
    /// (hidden subagent children, layout dirty, fresh construction).
    pub last_measured_y_in_msg: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalSnapshotMode {
    AppendOnly,
    ReplaceSnapshot,
}

impl ToolCallInfo {
    pub(crate) fn estimate_json_value_bytes(value: &serde_json::Value) -> usize {
        serde_json::to_string(value).map_or(0, |json| json.len())
    }

    pub fn is_execute_tool(&self) -> bool {
        is_execute_tool_name(&self.sdk_tool_name)
    }

    pub fn is_ask_question_tool(&self) -> bool {
        is_ask_question_tool_name(&self.sdk_tool_name)
    }

    pub fn is_exit_plan_mode_tool(&self) -> bool {
        is_exit_plan_mode_tool_name(&self.sdk_tool_name)
    }

    pub fn assistant_auto_backgrounded(&self) -> bool {
        self.output_metadata
            .as_ref()
            .and_then(|metadata| metadata.bash.as_ref())
            .and_then(|metadata| metadata.assistant_auto_backgrounded)
            .unwrap_or(false)
    }

    pub fn verification_nudge_needed(&self) -> bool {
        self.output_metadata
            .as_ref()
            .and_then(|metadata| metadata.todo_write.as_ref())
            .and_then(|metadata| metadata.verification_nudge_needed)
            .unwrap_or(false)
    }

    pub fn task_is_backgrounded(&self) -> bool {
        self.task_metadata.as_ref().and_then(|metadata| metadata.is_backgrounded).unwrap_or(false)
    }

    pub fn hidden_unless_focused_interaction(&self) -> bool {
        self.hidden
            && !self.pending_permission.as_ref().is_some_and(|permission| permission.focused)
            && !self.pending_question.as_ref().is_some_and(|question| question.focused)
    }

    pub fn is_hidden_focused_interaction(&self) -> bool {
        self.hidden
            && (self.pending_permission.as_ref().is_some_and(|permission| permission.focused)
                || self.pending_question.as_ref().is_some_and(|question| question.focused))
    }

    pub fn is_subagent_root_tool(&self) -> bool {
        !self.hidden && matches!(self.sdk_tool_name.as_str(), "Task" | "Agent")
    }

    /// Mark render cache for this tool call as stale.
    pub fn mark_tool_call_render_dirty(&mut self) {
        crate::perf::mark("tc_invalidations_requested");
        self.render_epoch = self.render_epoch.wrapping_add(1);
        self.cache.invalidate();
        crate::perf::mark("tc_invalidations_applied");
    }

    /// Mark layout cache for this tool call as stale.
    pub fn mark_tool_call_layout_dirty(&mut self) {
        self.layout_epoch = self.layout_epoch.wrapping_add(1);
        self.last_measured_width = 0;
        self.last_measured_height = 0;
        self.last_measured_layout_epoch = 0;
        self.last_measured_layout_generation = 0;
        self.last_measured_y_in_msg = 0;
        self.mark_tool_call_render_dirty();
    }

    pub fn cache_measurement_key_matches(&self, width: u16, layout_generation: u64) -> bool {
        self.last_measured_width == width
            && self.last_measured_layout_epoch == self.layout_epoch
            && self.last_measured_layout_generation == layout_generation
    }

    pub fn record_measured_height(&mut self, width: u16, height: usize, layout_generation: u64) {
        self.last_measured_width = width;
        self.last_measured_height = height;
        self.last_measured_layout_epoch = self.layout_epoch;
        self.last_measured_layout_generation = layout_generation;
    }

    pub fn set_raw_input(&mut self, raw_input: Option<serde_json::Value>) -> bool {
        if self.raw_input == raw_input {
            return false;
        }
        self.raw_input_bytes = raw_input.as_ref().map_or(0, Self::estimate_json_value_bytes);
        self.raw_input = raw_input;
        true
    }
}

pub fn is_execute_tool_name(tool_name: &str) -> bool {
    tool_name.eq_ignore_ascii_case("bash")
}

pub fn is_ask_question_tool_name(tool_name: &str) -> bool {
    tool_name.eq_ignore_ascii_case("askuserquestion")
}

pub fn is_exit_plan_mode_tool_name(tool_name: &str) -> bool {
    tool_name.eq_ignore_ascii_case("exitplanmode")
}

/// True when `tool_name` matches the long-running `Monitor` tool —
/// claude's streaming-process watcher (`persistent` or `timeout_ms`-
/// bounded). Used by the Inspector PROCESSES section to identify
/// in-flight monitors regardless of how the CLI happens to capitalise
/// the name (matches `is_execute_tool_name`'s style).
pub fn is_monitor_tool_name(tool_name: &str) -> bool {
    tool_name.eq_ignore_ascii_case("monitor")
}

/// True when `tool_name` matches the `CronCreate` scheduling tool.
/// CronCreate registers cron-style recurring or one-shot prompts
/// (see `forge_test_harness` captures + claude CLI binary trace).
/// Used by PROCESSES to surface scheduled jobs alongside live
/// backgrounded tasks.
pub fn is_cron_create_tool_name(tool_name: &str) -> bool {
    tool_name.eq_ignore_ascii_case("croncreate")
}

/// Permission state stored inline on a `ToolCallInfo`, so the permission
/// controls render inside the tool call block (unified edit/permission UX).
///
/// Phase 1+: the `response_tx` field has been replaced by `tool_id`.
/// Workspace owns the oneshot in `DomainSession.pending_interactions`;
/// the picker site dispatches `Command::RespondPermission { key,
/// tool_id, outcome }` via `Workspace::dispatch` instead of fulfilling
/// a local sender.
pub struct InlinePermission {
    pub options: Vec<model::PermissionOption>,
    pub display: Option<model::PermissionDisplay>,
    pub tool_id: String,
    pub selected_index: usize,
    /// Whether this permission currently has keyboard focus.
    /// When multiple permissions are pending, only the focused one
    /// shows the selection arrow and accepts Left/Right/Enter input.
    pub focused: bool,
}

pub struct InlineQuestion {
    pub prompt: model::QuestionPrompt,
    pub tool_id: String,
    pub focused_option_index: usize,
    pub selected_option_indices: std::collections::BTreeSet<usize>,
    pub notes: String,
    pub notes_cursor: usize,
    pub editing_notes: bool,
    pub focused: bool,
    pub question_index: usize,
    pub total_questions: usize,
}

#[cfg(test)]
mod tool_name_tests {
    use super::*;

    #[test]
    fn is_monitor_tool_name_matches_case_insensitive() {
        assert!(is_monitor_tool_name("Monitor"));
        assert!(is_monitor_tool_name("monitor"));
        assert!(is_monitor_tool_name("MONITOR"));
        assert!(!is_monitor_tool_name("Bash"));
        assert!(!is_monitor_tool_name("MonitorTool"));
        assert!(!is_monitor_tool_name(""));
    }

    #[test]
    fn is_cron_create_tool_name_matches_case_insensitive() {
        assert!(is_cron_create_tool_name("CronCreate"));
        assert!(is_cron_create_tool_name("croncreate"));
        assert!(is_cron_create_tool_name("CRONCREATE"));
        assert!(!is_cron_create_tool_name("CronDelete"));
        assert!(!is_cron_create_tool_name("CronList"));
        assert!(!is_cron_create_tool_name("Cron"));
        assert!(!is_cron_create_tool_name(""));
    }
}
