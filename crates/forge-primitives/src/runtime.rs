//! Runtime status - mode/model state, available models/commands/agents,
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
    pub supports_auto_mode: Option<bool>,
}

impl AvailableModel {
    pub fn new(id: impl Into<String>, display_name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            display_name: display_name.into(),
            description: None,
            supports_effort: false,
            supported_effort_levels: Vec::new(),
            supports_adaptive_thinking: None,
            supports_auto_mode: None,
        }
    }

    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn supports_effort(mut self, supports_effort: bool) -> Self {
        self.supports_effort = supports_effort;
        self
    }

    pub fn supported_effort_levels(mut self, supported_effort_levels: Vec<EffortLevel>) -> Self {
        self.supported_effort_levels = supported_effort_levels;
        self
    }

    pub fn supports_adaptive_thinking(mut self, supports_adaptive_thinking: Option<bool>) -> Self {
        self.supports_adaptive_thinking = supports_adaptive_thinking;
        self
    }

    pub fn supports_auto_mode(mut self, supports_auto_mode: Option<bool>) -> Self {
        self.supports_auto_mode = supports_auto_mode;
        self
    }
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
    pub supports_auto_mode: Option<bool>,
    pub supports_adaptive_thinking: Option<bool>,
    pub is_authoritative: bool,
}

impl CurrentModel {
    pub fn new(
        resolved_id: impl Into<String>,
        display_name_short: impl Into<String>,
        display_name_long: impl Into<String>,
    ) -> Self {
        Self {
            requested_id: None,
            resolved_id: resolved_id.into(),
            display_name_short: display_name_short.into(),
            display_name_long: display_name_long.into(),
            catalog_id: None,
            supports_effort: false,
            supported_effort_levels: Vec::new(),
            supports_auto_mode: None,
            supports_adaptive_thinking: None,
            is_authoritative: false,
        }
    }

    pub fn requested_id(mut self, requested_id: impl Into<String>) -> Self {
        self.requested_id = Some(requested_id.into());
        self
    }

    pub fn catalog_id(mut self, catalog_id: impl Into<String>) -> Self {
        self.catalog_id = Some(catalog_id.into());
        self
    }

    pub fn supports_effort(mut self, supports_effort: bool) -> Self {
        self.supports_effort = supports_effort;
        self
    }

    pub fn supported_effort_levels(mut self, supported_effort_levels: Vec<EffortLevel>) -> Self {
        self.supported_effort_levels = supported_effort_levels;
        self
    }

    pub fn supports_adaptive_thinking(mut self, supports_adaptive_thinking: Option<bool>) -> Self {
        self.supports_adaptive_thinking = supports_adaptive_thinking;
        self
    }

    pub fn supports_auto_mode(mut self, supports_auto_mode: Option<bool>) -> Self {
        self.supports_auto_mode = supports_auto_mode;
        self
    }

    pub fn authoritative(mut self, is_authoritative: bool) -> Self {
        self.is_authoritative = is_authoritative;
        self
    }
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

/// Why a turn ended - surfaced by `AgentEvent::TurnComplete` /
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
    #[serde(other)]
    Unknown,
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
            Self::Unknown => "unknown",
        }
    }
}

/// Lifecycle state of a session. `forge-tui` stores one per bucket to
/// render the Projects pane glyph; `forge-workspace` derives one per
/// worker for `WorkerStatus::activity`. The two derivations are
/// independent and need not agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SessionLifecycleState {
    /// No subprocess yet; lead exists conceptually but has never
    /// been spawned (or has been freed).
    #[default]
    Sleeping,
    /// Subprocess spawn in flight - between user click and first
    /// `Connected` event from the bridge.
    Spawning,
    /// Subprocess is alive and idle (no turn in progress).
    Idle,
    /// Subprocess is mid-turn or actively streaming.
    Running,
    /// Cannot proceed without a human. Two independent producers:
    /// `forge-tui` sets it when a session's connection failed on a
    /// rate limit, and `forge-workspace` derives it for a session
    /// holding a pending interaction (permission prompt,
    /// `AskUserQuestion`, elicitation).
    Attention,
    /// Bridge is waiting on `/login` to complete before the session
    /// can be spawned.
    AuthRequired,
    /// Setup (or running) hit a fatal error - `ConnectionFailed`
    /// drove this. Bucket is dead but kept in `app.sessions` so the
    /// user can see the error banner.
    Failed,
    /// User triggered `/logout`. The bridge is down; user can start
    /// a new session via /new or by clicking another project.
    LoggedOut,
}

/// Per-session SDK turn state - model-resolution cache, mode
/// capability state, MCP per-server cooldowns, and the auth/error
/// flags that survive across messages. Held authoritatively by
/// `forge-workspace`; `forge-tui` projects from there.
#[derive(Debug, Default)]
pub struct SessionTurnState {
    /// Live tool-call store keyed by `tool_use_id` for cross-message
    /// `tool_use ↔ tool_result` pairing.
    pub tool_calls: std::collections::HashMap<String, crate::session_update::ToolCall>,
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
    pub mode: Option<crate::permission::PermissionMode>,
    /// Permission modes the runtime currently supports.
    pub supported_mode_ids: Vec<crate::permission::PermissionMode>,
    /// Permission modes recognised but currently unavailable.
    pub runtime_unavailable_mode_ids: Vec<crate::permission::PermissionMode>,
    /// Current mode resolution alongside the human-readable label.
    pub mode_state: Option<ModeState>,

    /// Whether an `AvailableAgentsUpdate` has been emitted for this
    /// turn. The first `system/init` of a turn carries the catalogue;
    /// subsequent re-fires within the same turn are no-ops.
    pub agents_emitted_this_turn: bool,

    /// Last assistant error subtype seen on the wire - survives
    /// across messages so a subsequent `Result` can classify the
    /// turn correctly.
    pub last_assistant_error: Option<String>,
}
