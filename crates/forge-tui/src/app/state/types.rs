use crate::agent::model;
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeInfo {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeState {
    pub current_mode_id: String,
    pub current_mode_name: String,
    pub available_modes: Vec<ModeInfo>,
}

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

/// A single todo item from Claude's `TodoWrite` tool call.
#[derive(Debug, Clone)]
pub struct TodoItem {
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
    pub file_size_bytes: u64,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub custom_title: Option<String>,
    pub first_prompt: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionPickerState {
    /// Index of the currently highlighted session in `app.recent_sessions`.
    pub selected: usize,
    /// Scroll offset for when the list exceeds the visible area.
    pub scroll_offset: usize,
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

// Wire-shape usage types lifted to forge-agent::cloud (2026-05-05
// restructure). Re-exported here so existing import paths
// (`crate::app::UsageSnapshot`, etc.) keep resolving.
pub use forge_agent::cloud::{ExtraUsage, UsageSnapshot, UsageSourceKind, UsageWindow};

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
    pub context_usage_in_flight: bool,
    pub context_usage_refresh_pending: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct McpState {
    pub servers: Vec<forge_primitives::McpServerStatus>,
    pub in_flight: bool,
    pub last_error: Option<String>,
    pub pending_elicitation: Option<forge_primitives::ElicitationRequest>,
}

/// Per-session runtime state. Owns the in-flight `tool_call` store,
/// the model-resolution cache, the mode-capability state, the MCP
/// per-server cooldowns, and the auth/error flags that survive
/// across messages. The App's `handle_sdk_message` walks raw
/// `forge_primitives::Message` envelopes and reads/writes these
/// fields directly — they are the authoritative per-session store.
#[derive(Debug, Default)]
pub struct SessionTurnState {
    /// Live tool-call store keyed by `tool_use_id` for cross-message
    /// `tool_use ↔ tool_result` pairing.
    pub tool_calls: std::collections::HashMap<String, forge_primitives::ToolCall>,
    /// Maps task-tool `task_id` → `tool_use_id` so `TaskProgress` /
    /// `TaskNotification` messages can resolve back to the originating
    /// tool call for `ToolCallUpdate` emission.
    pub task_tool_use_ids: std::collections::HashMap<String, String>,

    /// Raw model id from the CLI's session-init payload.
    pub model_id: String,
    /// Model id explicitly requested via `/model`.
    pub requested_model_id: Option<String>,
    /// Resolved model id after runtime fallback from the requested id.
    pub resolved_runtime_model_id: Option<String>,

    /// Active permission mode (typed enum, not the wire string).
    /// Populated from System(init).permissionMode and the `SetMode`
    /// command path.
    pub mode: Option<crate::agent::state::PermissionMode>,
    /// Permission modes the runtime currently supports.
    pub supported_mode_ids: Vec<crate::agent::state::PermissionMode>,
    /// Permission modes recognised but currently unavailable.
    pub runtime_unavailable_mode_ids: Vec<crate::agent::state::PermissionMode>,
    /// Whether `bypassPermissions` mode is allowed for this session.
    pub supports_bypass_permissions_mode: bool,
    /// Current mode resolution alongside the human-readable label.
    pub mode_state: Option<forge_primitives::ModeState>,

    /// Sha-style fingerprint of the `available_agents` list — used to
    /// emit `AvailableAgentsUpdate` only when the catalogue changes.
    pub last_agents_signature: Option<String>,

    /// True once an `AuthRequired` event has been emitted for this
    /// session; suppresses re-emits on subsequent stream events.
    pub auth_hint_sent: bool,

    /// Last assistant error subtype seen on the wire — survives
    /// across messages so a subsequent `Result` can classify the
    /// turn correctly.
    pub last_assistant_error: Option<String>,

    /// Per-server cooldown timestamps for MCP status revalidation.
    pub mcp_status_revalidated_at: std::collections::HashMap<String, std::time::Instant>,

    /// Resume history collected during connect handshake; attached to
    /// the first Connected event payload.
    pub resume_updates: Option<Vec<forge_primitives::SessionUpdate>>,
}

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

#[derive(Debug, PartialEq, Eq)]
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
pub enum CancelOrigin {
    Manual,
    AutoQueue,
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
