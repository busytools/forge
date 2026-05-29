use crate::agent::model;

pub use forge_primitives::runtime::{ModeInfo, ModeState};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HelpView {
    #[default]
    Keys,
    SlashCommands,
    Subagents,
}

/// Login hint displayed when authentication is required during connection.
/// Rendered as a banner above the input field.
pub struct LoginHint {
    pub method_name: String,
    pub method_description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingCommandAck {
    CurrentMode,
    CurrentModel,
    ConfigOption { option_id: String },
}

/// Snapshot of a `Message::StopHookSummary` event bound to an
/// assistant message in chat (#273). Rendered as a collapsed
/// 1-liner with `[▶ expand]`; the expanded view enumerates the
/// per-hook breakdown.
#[derive(Debug, Clone)]
pub struct StopHookSummaryState {
    /// Owning assistant message id, used to anchor the chip + body
    /// when re-rendering on scroll.
    pub message_idx: usize,
    /// Number of hooks that fired. Wire `hookCount`.
    pub actions: u32,
    /// Per-hook command + duration. Wire `hookInfos`.
    pub hooks: Vec<StopHookEntry>,
}

/// One row in the expanded stop-hook summary.
#[derive(Debug, Clone)]
pub struct StopHookEntry {
    pub command: String,
    pub duration_ms: u64,
}

/// #273 Task 8: lifecycle status of a Monitor (`Monitor` tool_use).
/// A Monitor row stays surfaced until ALL session monitors transition
/// to a terminal variant (`Stopped` / `Completed` / `TimedOut`); the
/// MONITORS Inspector section auto-clears when no monitor is still
/// `Running`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorStatus {
    /// Monitor is active. Persistent monitors stay `Running` until
    /// TaskStop or session end; non-persistent monitors run until
    /// their `timeout_ms` expires or the watched command exits.
    Running,
    /// Monitor terminated via TaskStop / killed / clean exit.
    Stopped,
    /// Monitor completed cleanly (synonym for Stopped on the
    /// renderer; preserved as a distinct variant in case downstream
    /// callers want to disambiguate normal-exit from explicit-kill).
    Completed,
    /// Monitor's `timeout_ms` fired. Renderer surfaces a distinct
    /// `· timed out` badge so users see the failure mode at a glance.
    TimedOut,
}

/// #273 Task 8: a single Monitor entry surfaced in chat + the
/// Inspector MONITORS section. Populated on Monitor tool_use,
/// updated on terminal lifecycle events (TaskStop, task_updated
/// with `status: stopped|killed|failed`, Result origin marker for
/// timeout).
#[derive(Debug, Clone)]
pub struct MonitorEntry {
    /// `tool_use_id` from the Monitor `tool_use` block — the
    /// canonical id chat-stream / Inspector / mouse routing all
    /// reference.
    pub tool_use_id: String,
    /// Task id assigned by the CLI when the Monitor starts (extracted
    /// from `tool_use_result.taskId`). `None` until the start
    /// confirmation arrives. Used to correlate against `TaskStarted`
    /// / `TaskUpdated` wire events.
    pub task_id: Option<String>,
    /// `tool_input.description` — the headline label.
    pub description: String,
    /// `tool_input.command` — the watched shell command.
    pub command: String,
    /// `tool_input.persistent` — when true the Monitor stays alive
    /// across multiple events; when false a single exit ends it.
    pub persistent: bool,
    /// `tool_input.timeout_ms` — zero when no explicit timeout
    /// (persistent monitors typically pass zero).
    pub timeout_ms: u64,
    /// Lifecycle status. Drives MONITORS-section visibility and the
    /// chat one-liner (`◉ Monitor started · …` vs `◉ Monitor
    /// stopped · …`).
    pub status: MonitorStatus,
    /// Rolling 12-line tail of monitor output (most-recent at the
    /// end). Bounded so a long-running Monitor doesn't grow the
    /// Inspector pane indefinitely.
    pub output_tail: std::collections::VecDeque<String>,
    /// Per-row expand toggle for the Inspector section. Click on the
    /// row in the Inspector flips this; `false` collapses to a
    /// one-liner row.
    pub expanded_in_inspector: bool,
}

impl MonitorEntry {
    /// Maximum lines kept in `output_tail`. Bounded so the Inspector
    /// row doesn't grow unbounded for a long-lived monitor.
    pub const OUTPUT_TAIL_MAX: usize = 12;

    /// True when this entry is still actively watching. Used by the
    /// MONITORS-section visibility predicate (`section drops when no
    /// running monitor remains`).
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.status == MonitorStatus::Running
    }

    /// Push an output line into the tail ring, evicting the oldest
    /// when capacity is reached. Caller passes the raw line text.
    pub fn push_output(&mut self, line: String) {
        if self.output_tail.len() == Self::OUTPUT_TAIL_MAX {
            self.output_tail.pop_front();
        }
        self.output_tail.push_back(line);
    }
}

/// #273 Task 9: lifecycle status of a Workflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowStatus {
    /// Workflow is in progress — phases / agents may still fire.
    InProgress,
    /// All phases reported terminal; renderer collapses the tree
    /// to a single-liner and the section auto-clears once every
    /// session workflow shares this status.
    Completed,
}

/// #273 Task 9: per-phase status (mirrors the wire `state` field's
/// canonical values).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseStatus {
    /// Phase has been declared by the workflow but no agent has
    /// started in it yet.
    Pending,
    /// At least one agent inside the phase is `start` / `progress`.
    InProgress,
    /// Every agent inside the phase reported `done`.
    Completed,
}

/// #273 Task 9: one phase row inside a WorkflowEntry's per-phase
/// tree. The renderer walks `phases` in `index` order; per-phase
/// logs are the truncated lifecycle markers (last_tool_name +
/// last_tool_summary + resultPreview) captured from
/// `WorkflowProgressEvent` events.
#[derive(Debug, Clone)]
pub struct PhaseEntry {
    /// 1-indexed phase number from the wire `index` field — matches
    /// the order `phase()` is called inside the script.
    pub index: u32,
    /// Phase title (from `phase("Ping")` literal).
    pub title: String,
    /// Current status — drives the glyph in the tree.
    pub status: PhaseStatus,
    /// Bounded ring buffer of per-phase log lines (each line is a
    /// captured agent transition summary). Capped at
    /// `WorkflowEntry::PHASE_LOG_MAX`.
    pub logs: std::collections::VecDeque<String>,
}

impl PhaseEntry {
    /// Push a log line, evicting oldest at capacity.
    pub fn push_log(&mut self, line: String) {
        if self.logs.len() == WorkflowEntry::PHASE_LOG_MAX {
            self.logs.pop_front();
        }
        self.logs.push_back(line);
    }
}

/// #273 Task 9: a single Workflow entry surfaced in chat + the
/// Inspector WORKFLOWS section. Populated on `Workflow` tool_use;
/// `phases` / `final_result_summary` updated from each
/// `Message::TaskProgress` carrying `workflow_progress` events.
#[derive(Debug, Clone)]
pub struct WorkflowEntry {
    /// `tool_use_id` from the Workflow tool_use block.
    pub tool_use_id: String,
    /// Task id assigned by the CLI when the Workflow starts
    /// (`tool_use_result.taskId` or `TaskStarted` mapping).
    pub task_id: Option<String>,
    /// `name` from the script's `export const meta = {...}` block,
    /// or the literal `"Workflow"` fallback.
    pub meta_name: String,
    /// Optional `description` field from the meta block. Surfaces
    /// as a DIM subtitle row in the Inspector tree.
    pub meta_description: Option<String>,
    /// Phase tree — built / rebuilt from each TaskProgress's full
    /// `workflow_progress` snapshot. Order matches phase index.
    pub phases: Vec<PhaseEntry>,
    /// Lifecycle status. Drives chat one-liner shape (`started` vs
    /// `done`) and the per-row collapsed glyph in the Inspector.
    pub status: WorkflowStatus,
    /// Final result preview captured from the terminating
    /// `workflow_agent.state == "done"` event's `resultPreview`.
    pub final_result_summary: Option<String>,
    /// Per-row expand toggle for the Inspector. Click flips this.
    pub expanded_in_inspector: bool,
}

impl WorkflowEntry {
    /// Maximum log lines kept per phase. Bounded so the Inspector
    /// row doesn't grow unbounded for a long-lived workflow.
    pub const PHASE_LOG_MAX: usize = 8;

    /// True when this workflow has at least one phase that hasn't
    /// reached `Completed`. The all-completed predicate drives the
    /// WORKFLOWS-section auto-clear.
    #[must_use]
    pub fn is_in_progress(&self) -> bool {
        self.status == WorkflowStatus::InProgress
    }

    /// Apply a full `workflow_progress` snapshot from a single
    /// `system/task_progress` event. The wire shape is a complete
    /// snapshot (not a delta), so this rebuilds the phase list.
    /// Phase-level logs accumulate across snapshots; agent
    /// transition events append to the matching phase.
    pub fn apply_workflow_progress(&mut self, events: &[forge_primitives::WorkflowProgressEvent]) {
        use forge_primitives::WorkflowProgressEvent;

        // Build the phase set first so phases with no agent
        // activity yet still surface.
        for event in events {
            if let WorkflowProgressEvent::WorkflowPhase { index, title } = event {
                let already = self.phases.iter().any(|p| p.index == *index);
                if !already {
                    self.phases.push(PhaseEntry {
                        index: *index,
                        title: title.clone(),
                        status: PhaseStatus::Pending,
                        logs: std::collections::VecDeque::new(),
                    });
                }
            }
        }

        // Walk agent events and update each matching phase. The
        // most recent state-per-agent wins (snapshots are
        // monotonic: start -> progress -> done).
        for event in events {
            let WorkflowProgressEvent::WorkflowAgent {
                phase_index,
                phase_title,
                state,
                last_tool_name,
                last_tool_summary,
                result_preview,
                ..
            } = event
            else {
                continue;
            };
            // Ensure phase exists (wire sometimes emits an agent
            // before a workflow_phase marker — defensive create).
            if !self.phases.iter().any(|p| p.index == *phase_index) {
                self.phases.push(PhaseEntry {
                    index: *phase_index,
                    title: phase_title.clone(),
                    status: PhaseStatus::Pending,
                    logs: std::collections::VecDeque::new(),
                });
            }
            let Some(phase) = self.phases.iter_mut().find(|p| p.index == *phase_index) else {
                continue;
            };
            phase.status = match state.as_str() {
                "done" => PhaseStatus::Completed,
                "start" | "progress" => PhaseStatus::InProgress,
                _ => phase.status,
            };
            if let Some(summary) = last_tool_summary.as_deref().filter(|s| !s.is_empty()) {
                let tool = last_tool_name.as_deref().unwrap_or("agent");
                phase.push_log(format!("{tool}: {summary}"));
            } else if let Some(tool) = last_tool_name.as_deref().filter(|s| !s.is_empty()) {
                phase.push_log(format!("running {tool}"));
            }
            if state == "done"
                && let Some(preview) = result_preview.as_deref().filter(|s| !s.is_empty())
            {
                self.final_result_summary = Some(preview.to_owned());
                // Once we see a done state with a result preview,
                // also recheck whether ALL phases are complete.
                if self.phases.iter().all(|p| p.status == PhaseStatus::Completed) {
                    self.status = WorkflowStatus::Completed;
                }
                return;
            }
        }
    }
}

/// A single inspector task item from Claude's `TaskCreate`/`TaskUpdate`
/// family (#268). CLI 2.1.156 deprecated the older `TodoWrite` tool;
/// forge no longer renders TodoWrite output. The `id` is assigned by
/// the CLI in `TaskCreate`'s result text (`"Task #N created
/// successfully:"`) and referenced by `TaskUpdate.taskId`.
#[derive(Debug, Clone)]
pub struct TodoItem {
    /// CLI-assigned task id (e.g. `"1"`). Used by `TaskUpdate` to
    /// locate the item to mutate or remove.
    pub id: String,
    pub content: String,
    pub status: TodoStatus,
    pub active_form: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentSessionInfo {
    pub session_id: String,
    pub summary: String,
    pub last_modified_ms: u64,
    pub cwd: Option<String>,
    pub custom_title: Option<String>,
    pub first_prompt: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct MessageUsage {
    pub input_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UsageSourceMode {
    #[default]
    Auto,
    Oauth,
    Cli,
}

// Wire-shape usage types live in forge-primitives::usage; re-exported
// here so existing import paths (`crate::app::UsageSnapshot`, etc.)
// keep resolving.
pub use forge_primitives::usage::{ExtraUsage, UsageSnapshot, UsageSourceKind, UsageWindow};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct UsageState {
    pub snapshot: Option<UsageSnapshot>,
    pub in_flight: bool,
    pub last_error: Option<String>,
    pub active_source: UsageSourceMode,
    pub last_attempted_source: Option<UsageSourceKind>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SessionUsageState {
    pub last_compaction_trigger: Option<model::CompactionTrigger>,
    pub last_compaction_pre_tokens: Option<u64>,
    pub context_usage_percent: Option<u8>,
    /// Raw model context-window size in tokens (e.g. 200_000 for
    /// Sonnet's base cap, 1_000_000 for the 1M variant). Read by
    /// the projects-pane footer to render `200K` / `1M` beneath
    /// the Ctx bar. `None` until the first ContextUsage poll
    /// returns a snapshot for this session.
    pub context_max_tokens: Option<u64>,
    pub context_usage_in_flight: bool,
    pub context_usage_refresh_pending: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct McpState {
    pub servers: Vec<forge_primitives::McpServerStatus>,
    pub in_flight: bool,
    pub last_error: Option<String>,
}

// Per-session SDK turn state lives in
// `forge_primitives::runtime::SessionTurnState`. Re-exported here
// so the existing `crate::app::state::types::SessionTurnState`
// import path resolves.
pub use forge_primitives::runtime::SessionTurnState;

pub const DEFAULT_RENDER_CACHE_BUDGET_BYTES: usize = 24 * 1024 * 1024;
pub const DEFAULT_HISTORY_RETENTION_MAX_BYTES: usize = 64 * 1024 * 1024;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderCacheBudget {
    pub max_bytes: usize,
    pub last_total_bytes: usize,
    pub last_evicted_bytes: usize,
    pub total_evictions: usize,
}

impl Default for RenderCacheBudget {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_RENDER_CACHE_BUDGET_BYTES,
            last_total_bytes: 0,
            last_evicted_bytes: 0,
            total_evictions: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryRetentionPolicy {
    pub max_bytes: usize,
}

impl Default for HistoryRetentionPolicy {
    fn default() -> Self {
        Self { max_bytes: DEFAULT_HISTORY_RETENTION_MAX_BYTES }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HistoryRetentionStats {
    pub total_before_bytes: usize,
    pub total_after_bytes: usize,
    pub dropped_messages: usize,
    pub dropped_bytes: usize,
    pub total_dropped_messages: usize,
    pub total_dropped_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheBudgetEnforceStats {
    pub total_before_bytes: usize,
    pub total_after_bytes: usize,
    pub evicted_bytes: usize,
    pub evicted_blocks: usize,
    /// Bytes in protected (non-evictable) blocks excluded from the budget comparison.
    pub protected_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppStatus {
    /// Waiting for bridge adapter connection (TUI shown, input disabled).
    Connecting,
    /// A slash command is in flight (input disabled, spinner shown).
    CommandPending,
    Ready,
    Thinking,
    Running,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolCallScope {
    MainAgent,
    SubagentRoot,
    SubagentChild { parent_tool_use_id: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionKind {
    Chat,
    Input,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionPoint {
    pub row: usize,
    pub col: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionState {
    pub kind: SelectionKind,
    pub start: SelectionPoint,
    pub end: SelectionPoint,
    pub dragging: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbarDragState {
    /// Row offset from thumb top where the initial click happened.
    pub thumb_grab_offset: usize,
    /// Visible track length used when the drag started.
    pub track_space: usize,
    /// Maximum scrollable row offset when the drag started.
    pub max_scroll: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PasteSessionState {
    pub id: u64,
    pub start: SelectionPoint,
    pub placeholder_index: Option<usize>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_entry_push_output_evicts_oldest_at_capacity() {
        let mut entry = MonitorEntry {
            tool_use_id: "tu".to_owned(),
            task_id: None,
            description: "watch".to_owned(),
            command: "tail".to_owned(),
            persistent: true,
            timeout_ms: 0,
            status: MonitorStatus::Running,
            output_tail: std::collections::VecDeque::new(),
            expanded_in_inspector: false,
        };
        for i in 0..MonitorEntry::OUTPUT_TAIL_MAX + 5 {
            entry.push_output(format!("line {i}"));
        }
        assert_eq!(entry.output_tail.len(), MonitorEntry::OUTPUT_TAIL_MAX);
        // Oldest 5 were evicted; tail starts at "line 5".
        assert_eq!(entry.output_tail.front().map(String::as_str), Some("line 5"));
        assert_eq!(
            entry.output_tail.back().map(String::as_str),
            Some("line 16"),
            "newest line stays at the back of the ring",
        );
    }

    #[test]
    fn workflow_entry_applies_progress_snapshot_to_build_phase_tree() {
        let mut entry = WorkflowEntry {
            tool_use_id: "tu".to_owned(),
            task_id: Some("task_1".to_owned()),
            meta_name: "minimal-ping".to_owned(),
            meta_description: Some("sanity".to_owned()),
            phases: Vec::new(),
            status: WorkflowStatus::InProgress,
            final_result_summary: None,
            expanded_in_inspector: false,
        };
        let events = vec![
            forge_primitives::WorkflowProgressEvent::WorkflowPhase {
                index: 1,
                title: "Ping".to_owned(),
            },
            forge_primitives::WorkflowProgressEvent::WorkflowAgent {
                index: 1,
                label: "ping".to_owned(),
                phase_index: 1,
                phase_title: "Ping".to_owned(),
                state: "start".to_owned(),
                last_tool_name: None,
                last_tool_summary: None,
                result_preview: None,
            },
        ];
        entry.apply_workflow_progress(&events);
        assert_eq!(entry.phases.len(), 1);
        assert_eq!(entry.phases[0].title, "Ping");
        assert_eq!(entry.phases[0].status, PhaseStatus::InProgress);
        assert_eq!(entry.status, WorkflowStatus::InProgress);
    }

    #[test]
    fn workflow_entry_completes_on_terminal_done_event_with_result() {
        let mut entry = WorkflowEntry {
            tool_use_id: "tu".to_owned(),
            task_id: Some("task_1".to_owned()),
            meta_name: "ping".to_owned(),
            meta_description: None,
            phases: vec![PhaseEntry {
                index: 1,
                title: "Ping".to_owned(),
                status: PhaseStatus::InProgress,
                logs: std::collections::VecDeque::new(),
            }],
            status: WorkflowStatus::InProgress,
            final_result_summary: None,
            expanded_in_inspector: false,
        };
        let events = vec![forge_primitives::WorkflowProgressEvent::WorkflowAgent {
            index: 1,
            label: "ping".to_owned(),
            phase_index: 1,
            phase_title: "Ping".to_owned(),
            state: "done".to_owned(),
            last_tool_name: Some("StructuredOutput".to_owned()),
            last_tool_summary: Some("pong".to_owned()),
            result_preview: Some("{\"answer\":\"pong\"}".to_owned()),
        }];
        entry.apply_workflow_progress(&events);
        assert_eq!(entry.phases[0].status, PhaseStatus::Completed);
        assert_eq!(entry.status, WorkflowStatus::Completed);
        assert_eq!(entry.final_result_summary.as_deref(), Some("{\"answer\":\"pong\"}"));
    }

    #[test]
    fn workflow_entry_logs_accumulate_with_bounded_ring() {
        let mut entry = WorkflowEntry {
            tool_use_id: "tu".to_owned(),
            task_id: None,
            meta_name: "w".to_owned(),
            meta_description: None,
            phases: vec![PhaseEntry {
                index: 1,
                title: "p".to_owned(),
                status: PhaseStatus::Pending,
                logs: std::collections::VecDeque::new(),
            }],
            status: WorkflowStatus::InProgress,
            final_result_summary: None,
            expanded_in_inspector: false,
        };
        for i in 0..WorkflowEntry::PHASE_LOG_MAX + 3 {
            entry.phases[0].push_log(format!("log {i}"));
        }
        assert_eq!(entry.phases[0].logs.len(), WorkflowEntry::PHASE_LOG_MAX);
        assert_eq!(entry.phases[0].logs.front().map(String::as_str), Some("log 3"));
    }

    #[test]
    fn monitor_entry_is_running_predicate_matches_status() {
        let mut entry = MonitorEntry {
            tool_use_id: "tu".to_owned(),
            task_id: None,
            description: "x".to_owned(),
            command: "y".to_owned(),
            persistent: false,
            timeout_ms: 0,
            status: MonitorStatus::Running,
            output_tail: std::collections::VecDeque::new(),
            expanded_in_inspector: false,
        };
        assert!(entry.is_running());
        entry.status = MonitorStatus::Stopped;
        assert!(!entry.is_running());
        entry.status = MonitorStatus::Completed;
        assert!(!entry.is_running());
        entry.status = MonitorStatus::TimedOut;
        assert!(!entry.is_running());
    }
}
