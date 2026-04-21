//! Top-level stream-json message shapes.
//!
//! Every line the `claude --output-format stream-json` binary emits is one
//! of these four variants. Mirrors Python SDK's `AssistantMessage`,
//! `UserMessage`, `SystemMessage`, `ResultMessage`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::content::ContentBlock;

/// One stream-json message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    /// An assistant turn (may be a partial chunk during streaming).
    Assistant {
        /// The nested Anthropic-API-shaped message envelope.
        message: AssistantEnvelope,
        /// Session id this turn belongs to.
        session_id: String,
        /// Parent tool-use id when this turn is a sub-agent spawned via `Task`.
        #[serde(default)]
        parent_tool_use_id: Option<String>,
    },

    /// A user turn — user prompts or tool-result envelopes.
    User {
        /// The nested user-message envelope.
        message: UserEnvelope,
        /// Session id this turn belongs to.
        session_id: String,
        /// Parent tool-use id when this is a sub-agent turn.
        #[serde(default)]
        parent_tool_use_id: Option<String>,
    },

    /// Out-of-band system event. `subtype` discriminates (init, notification, etc.).
    /// The rest of the payload is captured in `data`.
    System {
        /// System event discriminant (e.g. `"init"`, `"notification"`).
        subtype: String,
        /// Session id when the event is session-scoped.
        #[serde(default)]
        session_id: Option<String>,
        /// All other fields on the original message, captured verbatim.
        #[serde(flatten)]
        data: Value,
    },

    /// Rate-limit state transition. The CLI emits this when the current
    /// rate-limit window changes state (e.g. `allowed` → `allowed_warning`).
    /// Wire shape mirrors Python SDK v0.1.64 `types.py:1054-1107` +
    /// `_internal/message_parser.py:242-262`.
    RateLimitEvent {
        /// Rate-limit snapshot at the moment of the transition.
        rate_limit_info: RateLimitInfo,
        /// Unique identifier for this rate-limit event.
        uuid: String,
        /// Session id the event applies to.
        session_id: String,
    },

    /// End-of-turn or end-of-session summary with cost and usage.
    Result {
        /// Result discriminant (e.g. `"success"`, `"error_during_execution"`).
        subtype: String,
        /// Session id this turn belongs to.
        session_id: String,
        /// True when the turn ended in error.
        is_error: bool,
        /// Number of turns in this session so far.
        num_turns: u64,
        /// Total wall-clock duration in milliseconds.
        duration_ms: u64,
        /// Time spent waiting on the Anthropic API in milliseconds.
        duration_api_ms: u64,
        /// Total cost so far in USD.
        total_cost_usd: f64,
        /// Aggregate token usage.
        usage: Usage,
    },
}

/// The Anthropic-API-shaped envelope inside an `Assistant` message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantEnvelope {
    /// Message id from the Anthropic API.
    pub id: String,
    /// Fixed value `"assistant"`.
    pub role: String,
    /// Model name (e.g. `"claude-opus-4-5"`).
    pub model: String,
    /// Content blocks in order (interleaved text + tool-use).
    pub content: Vec<ContentBlock>,
    /// Why the turn ended, if it ended.
    #[serde(default)]
    pub stop_reason: Option<StopReason>,
    /// Stop sequence that triggered end-of-turn, if any.
    #[serde(default)]
    pub stop_sequence: Option<String>,
    /// Token usage for this turn.
    pub usage: Usage,
}

/// Envelope inside a `User` message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserEnvelope {
    /// Fixed value `"user"`.
    pub role: String,
    /// Content blocks — usually `ToolResult` blocks when reporting tool outputs.
    pub content: Vec<ContentBlock>,
}

/// Anthropic API's stop-reason enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// Model finished its turn naturally.
    EndTurn,
    /// Ran up against `max_tokens`.
    MaxTokens,
    /// Hit a stop sequence.
    StopSequence,
    /// Model is requesting a tool call; expect a `tool_use` block in content.
    ToolUse,
}

/// Rate-limit window status. Ported from Python's
/// `Literal["allowed", "allowed_warning", "rejected"]` in `types.py:1054`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitStatus {
    /// Within the window — no restrictions.
    Allowed,
    /// Approaching the limit; callers should warn / back off soon.
    AllowedWarning,
    /// Limit hit; requests are being refused.
    Rejected,
}

/// Which rate-limit window applies. Ported from `types.py:1055-1057`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitType {
    /// Five-hour rolling window.
    FiveHour,
    /// Seven-day rolling window (any model).
    SevenDay,
    /// Seven-day window for Opus-class models specifically.
    SevenDayOpus,
    /// Seven-day window for Sonnet-class models specifically.
    SevenDaySonnet,
    /// Pay-as-you-go overage window.
    Overage,
}

/// Rate-limit snapshot emitted inside a [`Message::RateLimitEvent`].
///
/// Mirrors Python SDK v0.1.64 `RateLimitInfo` (`types.py:1061-1088`). Inner
/// field names on the wire are camelCase (`resetsAt`, `rateLimitType`,
/// `overageStatus`, `overageResetsAt`, `overageDisabledReason`) per the CLI
/// spec, while the outer frame uses `snake_case`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimitInfo {
    /// Current rate-limit status.
    pub status: RateLimitStatus,
    /// Unix timestamp (seconds) when the rate-limit window resets.
    #[serde(default, rename = "resetsAt", skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<i64>,
    /// Which rate-limit window applies.
    #[serde(
        default,
        rename = "rateLimitType",
        skip_serializing_if = "Option::is_none"
    )]
    pub rate_limit_type: Option<RateLimitType>,
    /// Fraction of the rate limit consumed (0.0 – 1.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub utilization: Option<f64>,
    /// Status of overage / pay-as-you-go usage, if applicable.
    #[serde(
        default,
        rename = "overageStatus",
        skip_serializing_if = "Option::is_none"
    )]
    pub overage_status: Option<RateLimitStatus>,
    /// Unix timestamp (seconds) when overage window resets.
    #[serde(
        default,
        rename = "overageResetsAt",
        skip_serializing_if = "Option::is_none"
    )]
    pub overage_resets_at: Option<i64>,
    /// Why overage is unavailable when rejected.
    #[serde(
        default,
        rename = "overageDisabledReason",
        skip_serializing_if = "Option::is_none"
    )]
    pub overage_disabled_reason: Option<String>,
}

/// Token-usage accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Input tokens this turn.
    pub input_tokens: u64,
    /// Output tokens this turn.
    pub output_tokens: u64,
    /// Tokens written to the prompt cache this turn.
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    /// Tokens read from the prompt cache this turn.
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}
