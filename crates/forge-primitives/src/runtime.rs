//! Runtime status — mode/model state, available models/commands/agents,
//! rate-limit views, retry classification, session status, terminal
//! reason. Wire-shape state the agent ↔ UI channel passes around to
//! describe "what's the live session doing right now".

use serde::{Deserialize, Serialize};

use crate::messages::RateLimitStatus;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeInfo {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeState {
    pub current_mode_id: String,
    pub current_mode_name: String,
    pub available_modes: Vec<ModeInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvailableCommand {
    pub name: String,
    pub description: String,
    pub input_hint: Option<String>,
}

impl AvailableCommand {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self { name: name.into(), description: description.into(), input_hint: None }
    }

    pub fn input_hint(mut self, input_hint: impl Into<String>) -> Self {
        self.input_hint = Some(input_hint.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvailableAgent {
    pub name: String,
    pub description: String,
    pub model: Option<String>,
}

impl AvailableAgent {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self { name: name.into(), description: description.into(), model: None }
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EffortLevel {
    #[serde(rename = "low")]
    Low,
    #[serde(rename = "medium")]
    Medium,
    #[serde(rename = "high")]
    High,
    #[serde(rename = "xhigh")]
    Xhigh,
    #[serde(rename = "max")]
    Max,
}

impl EffortLevel {
    pub const fn as_stored(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
            Self::Xhigh => "Extra High",
            Self::Max => "Max",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::Low => "Fastest responses",
            Self::Medium => "Balanced speed and depth",
            Self::High => "Deeper reasoning",
            Self::Xhigh => "Extra-high reasoning",
            Self::Max => "Maximum reasoning",
        }
    }

    pub fn from_stored(value: &str) -> Option<Self> {
        match value {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" | "extra_high" => Some(Self::Xhigh),
            "max" => Some(Self::Max),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvailableModel {
    pub id: String,
    pub display_name: String,
    pub description: Option<String>,
    pub supports_effort: bool,
    #[serde(default)]
    pub supported_effort_levels: Vec<EffortLevel>,
    pub supports_adaptive_thinking: Option<bool>,
    pub supports_fast_mode: Option<bool>,
    pub supports_auto_mode: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentModel {
    pub requested_id: Option<String>,
    pub resolved_id: String,
    pub display_name_short: String,
    pub display_name_long: String,
    pub catalog_id: Option<String>,
    pub supports_effort: bool,
    #[serde(default)]
    pub supported_effort_levels: Vec<EffortLevel>,
    pub supports_fast_mode: Option<bool>,
    pub supports_auto_mode: Option<bool>,
    pub supports_adaptive_thinking: Option<bool>,
    pub is_authoritative: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FastModeState {
    #[default]
    Off,
    Cooldown,
    On,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiRetryError {
    AuthenticationFailed,
    BillingError,
    RateLimit,
    InvalidRequest,
    ServerError,
    MaxOutputTokens,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeSessionState {
    Idle,
    Running,
    RequiresAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsParseErrorUpdate {
    pub file: Option<String>,
    pub path: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimitUpdate {
    pub status: RateLimitStatus,
    pub resets_at: Option<f64>,
    pub utilization: Option<f64>,
    pub rate_limit_type: Option<String>,
    pub overage_status: Option<RateLimitStatus>,
    pub overage_resets_at: Option<f64>,
    pub overage_disabled_reason: Option<String>,
    pub is_using_overage: Option<bool>,
    pub surpassed_threshold: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiRetryUpdate {
    pub attempt: u64,
    pub max_retries: u64,
    pub retry_delay_ms: u64,
    pub error_status: Option<u16>,
    pub error: ApiRetryError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Compacting,
    Idle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTrigger {
    Manual,
    Auto,
}

/// Why a turn ended — surfaced by `AgentEvent::TurnComplete` /
/// `TurnError` to the UI for status-line classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalReason {
    BlockingLimit,
    RapidRefillBreaker,
    PromptTooLong,
    ImageError,
    ModelError,
    AbortedStreaming,
    AbortedTools,
    StopHookPrevented,
    HookStopped,
    ToolDeferred,
    MaxTurns,
    Completed,
}

impl TerminalReason {
    pub const fn as_stored(self) -> &'static str {
        match self {
            Self::BlockingLimit => "blocking_limit",
            Self::RapidRefillBreaker => "rapid_refill_breaker",
            Self::PromptTooLong => "prompt_too_long",
            Self::ImageError => "image_error",
            Self::ModelError => "model_error",
            Self::AbortedStreaming => "aborted_streaming",
            Self::AbortedTools => "aborted_tools",
            Self::StopHookPrevented => "stop_hook_prevented",
            Self::HookStopped => "hook_stopped",
            Self::ToolDeferred => "tool_deferred",
            Self::MaxTurns => "max_turns",
            Self::Completed => "completed",
        }
    }
}

/// Lifecycle state of a session, used by the Projects pane to render
/// the right state glyph and (in later phases) by the multiplexer to
/// decide redraw semantics. Promoted from `forge_tui::app::session`
/// in Phase 2 of the MVVM refactor (#102) so both `forge-tui` and
/// `forge-workspace` can project this state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionLifecycleState {
    /// No subprocess yet; lead exists conceptually but has never
    /// been spawned (or has been freed).
    #[default]
    Sleeping,
    /// Subprocess spawn in flight — between user click and first
    /// `Connected` event from the bridge.
    Spawning,
    /// Subprocess is alive and idle (no turn in progress).
    Idle,
    /// Subprocess is mid-turn or actively streaming.
    Running,
    /// Background session is paused on a permission prompt and
    /// needs user input to continue.
    Attention,
    /// Bridge is waiting on `/login` to complete before the session
    /// can be spawned.
    AuthRequired,
    /// Setup (or running) hit a fatal error — `ConnectionFailed`
    /// drove this. Bucket is dead but kept in `app.sessions` so the
    /// user can see the error banner.
    Failed,
    /// User triggered `/logout`. The bridge is down; user can start
    /// a new session via /new or by clicking another project.
    LoggedOut,
}

/// Per-session SDK turn state — model-resolution cache, mode
/// capability state, MCP per-server cooldowns, and the auth/error
/// flags that survive across messages. Promoted from
/// `forge_tui::app::state::types` in Phase 2 of the MVVM refactor
/// (#102) so `forge-workspace` can hold an authoritative copy
/// alongside the existing forge-tui projection.
#[derive(Debug, Default)]
pub struct SessionTurnState {
    /// Live tool-call store keyed by `tool_use_id` for cross-message
    /// `tool_use ↔ tool_result` pairing.
    pub tool_calls: std::collections::HashMap<String, crate::session_update::ToolCall>,
    /// Maps task-tool `task_id` → `tool_use_id` so `TaskProgress` /
    /// `TaskNotification` messages can resolve back to the originating
    /// tool call for `ToolCallUpdate` emission.
    pub task_tool_use_ids: std::collections::HashMap<String, String>,
    /// Currently-alive long-running task ids. Populated by
    /// `task_started`, drained by `task_updated` with a terminal
    /// `patch.status` (`completed` / `failed` / `killed` / `stopped`).
    /// Decoupled from the per-`ToolCall` status field because
    /// backgrounded Bash tool_results arrive immediately (flipping
    /// the tool call to `Completed`) while the underlying process
    /// continues running — only the task-lifecycle wire events
    /// describe true liveness. Inspector's PROCESSES section
    /// renders based on this set.
    pub alive_task_ids: std::collections::HashSet<String>,

    /// Raw model id from the CLI's session-init payload.
    pub model_id: String,
    /// Model id explicitly requested via `/model`.
    pub requested_model_id: Option<String>,
    /// Resolved model id after runtime fallback from the requested id.
    pub resolved_runtime_model_id: Option<String>,

    /// Active permission mode (typed enum, not the wire string).
    /// Populated from System(init).permissionMode and the `SetMode`
    /// command path.
    pub mode: Option<crate::permission::PermissionMode>,
    /// Permission modes the runtime currently supports.
    pub supported_mode_ids: Vec<crate::permission::PermissionMode>,
    /// Permission modes recognised but currently unavailable.
    pub runtime_unavailable_mode_ids: Vec<crate::permission::PermissionMode>,
    /// Whether `bypassPermissions` mode is allowed for this session.
    pub supports_bypass_permissions_mode: bool,
    /// Current mode resolution alongside the human-readable label.
    pub mode_state: Option<ModeState>,

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
}
