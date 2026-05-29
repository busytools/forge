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
