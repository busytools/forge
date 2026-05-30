//! Top-level stream-json message shapes.
//!
//! Every line the `claude --output-format stream-json` binary emits is one
//! of these variants. SDK's `AssistantMessage`, `UserMessage`,
//! `SystemMessage` (plus task-lifecycle + mirror-error subclasses),
//! `ResultMessage`, and `RateLimitEvent`.

use serde::de::Deserializer;
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::content::ContentBlock;
use crate::runtime::{FastModeState, TerminalReason};

/// One stream-json message.
///
/// Wire-level dispatch on `type` and, for `type="system"`, on `subtype` is
/// handled by a private shim - users never see it. Every variant here is
/// the user-facing shape.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    /// An assistant turn (may be a partial chunk during streaming).
    Assistant {
        /// The nested Anthropic-API-shaped message envelope.
        message: AssistantEnvelope,
        /// Session id this turn belongs to.
        session_id: String,
        /// Parent tool-use id when this turn is a sub-agent spawned via `Task`.
        parent_tool_use_id: Option<String>,
        /// Classification of a failure the CLI attributes to this turn
        /// (e.g. `rate_limit`, `billing_error`). `None` for successful
        /// turns. `AssistantMessage.error`
        ///
        error: Option<AssistantMessageError>,
        /// Stable identifier for this assistant turn, used for file
        /// checkpointing (`enable_file_checkpointing=true`) and as the
        /// target of `forge_sdk::Client::rewind_files`.
        /// the CLI `AssistantMessage.uuid`.
        uuid: Option<String>,
    },

    /// A user turn - user prompts or tool-result envelopes.
    User {
        /// The nested user-message envelope.
        message: UserEnvelope,
        /// Session id this turn belongs to.
        session_id: String,
        /// Parent tool-use id when this is a sub-agent turn.
        parent_tool_use_id: Option<String>,
        /// Stable identifier for this user turn - the `user_message_id`
        /// `forge_sdk::Client::rewind_files` takes.
        /// the CLI `UserMessage.uuid`. `None` unless the
        /// CLI is configured to emit them
        /// (`extra_args={"replay-user-messages": None}`).
        uuid: Option<String>,
        /// Raw tool-result payload the CLI attaches when this user turn
        /// reports a tool's output. the CLI `UserMessage.tool_use_result`
        ///; forge-sdk passes it through as a
        /// [`Value`] since the upstream type is `dict[str, Any]`.
        tool_use_result: Option<Value>,
    },

    /// Out-of-band system event - `subtype` discriminates (e.g. `"init"`).
    /// Known task-lifecycle and mirror-error subtypes get their own typed
    /// variants below; everything else lands here with the raw payload.
    System {
        /// System event discriminant (e.g. `"init"`, `"info"`).
        subtype: String,
        /// Session id when the event is session-scoped.
        session_id: Option<String>,
        /// All other fields on the original message, captured verbatim.
        data: Value,
    },

    /// A sub-agent `Task` has started running. Subtype `"task_started"`.
    /// `TaskStartedMessage`.
    TaskStarted {
        /// Stable identifier for this task instance.
        task_id: String,
        /// Human-readable description supplied when the task was spawned.
        description: String,
        /// Unique identifier for this lifecycle event.
        uuid: String,
        /// Session id the task runs in.
        session_id: String,
        /// Parent tool-use id if the task was spawned via a tool call.
        tool_use_id: Option<String>,
        /// Sub-agent type selector (e.g. `"general-purpose"`).
        task_type: Option<String>,
    },

    /// Incremental lifecycle update for any long-running tool task
    /// (backgrounded `Bash`, `Monitor`, sub-agent `Task`). Subtype
    /// `"task_updated"`. Wire captures (`backgrounded_bash_lifecycle.jsonl`,
    /// `monitor_persistent_stream.jsonl`) show the CLI emits this
    /// instead of `task_notification` for the local-bash flavour:
    /// it carries a `patch` object with status / end_time deltas.
    /// Without a typed variant, the reducer can't transition a
    /// backgrounded Bash from `running` to `completed`.
    TaskUpdated {
        /// Stable identifier for this task instance - same id surface
        /// as `task_started` / `task_progress` / `task_notification`.
        task_id: String,
        /// Incremental patch applied to the task's state. Each field
        /// is optional because patches may update only some fields.
        patch: TaskUpdatePatch,
        /// Unique identifier for this lifecycle event.
        uuid: String,
        /// Session id the task runs in.
        session_id: String,
    },

    /// Periodic progress update while a sub-agent `Task` is in flight.
    /// Subtype `"task_progress"`. v0.1.64
    /// `TaskProgressMessage`.
    TaskProgress {
        /// Stable identifier for this task instance.
        task_id: String,
        /// Human-readable description supplied when the task was spawned.
        description: String,
        /// Usage accumulated so far.
        usage: TaskUsage,
        /// Unique identifier for this lifecycle event.
        uuid: String,
        /// Session id the task runs in.
        session_id: String,
        /// Parent tool-use id if the task was spawned via a tool call.
        tool_use_id: Option<String>,
        /// Name of the last tool the sub-agent invoked, if any.
        last_tool_name: Option<String>,
        /// Workflow tool's per-event snapshot of the
        /// workflow's phase + agent state. Empty for non-Workflow
        /// task_progress events. Each event carries the FULL
        /// snapshot (not a delta), so the renderer can rebuild the
        /// per-phase tree from a single most-recent event.
        workflow_progress: Vec<WorkflowProgressEvent>,
    },

    /// Terminal notification when a sub-agent `Task` completes,
    /// fails, or is stopped. Subtype `"task_notification"`. v0.1.64
    /// `TaskNotificationMessage`.
    ///
    /// Despite the generic-sounding name, captures confirm this
    /// variant only fires for the `Task` sub-agent tool - backgrounded
    /// `Bash` and `Monitor` use the `task_started` / `task_updated`
    /// pair (see [`Self::TaskStarted`], [`Self::TaskProgress`]) and
    /// Monitor stream events arrive as `Result` frames with
    /// `origin: {kind: "task-notification"}` rather than as system
    /// notifications.
    TaskNotification {
        /// Stable identifier for this task instance.
        task_id: String,
        /// How the task ended.
        status: TaskNotificationStatus,
        /// Path on disk where the task wrote its result transcript.
        output_file: String,
        /// Short natural-language summary of the outcome.
        summary: String,
        /// Unique identifier for this lifecycle event.
        uuid: String,
        /// Session id the task ran in.
        session_id: String,
        /// Parent tool-use id if the task was spawned via a tool call.
        tool_use_id: Option<String>,
        /// Total usage accumulated over the lifetime of the task, if reported.
        usage: Option<TaskUsage>,
    },

    /// CLI-side per-turn estimated thinking-token count, fires
    /// repeatedly during model thinking. Subtype `"thinking_tokens"`
    /// (#273). The renderer consumes `estimated_tokens` for the
    /// in-progress spinner chip `⠋ thinking · N tok`; the delta field
    /// is captured for future per-tick rate analysis.
    ThinkingTokens {
        /// Cumulative estimated reasoning-token count for the current
        /// in-flight assistant turn.
        estimated_tokens: u64,
        /// Delta since the previous `thinking_tokens` event in this
        /// turn. Surfaces the per-tick growth rate; the CLI emits a
        /// new event roughly every ~50 tokens.
        estimated_tokens_delta: i64,
        /// Unique identifier for this thinking-tokens event.
        uuid: String,
        /// Session id the event applies to.
        session_id: String,
    },

    /// Assistant-turn wall-clock + message-count summary emitted at
    /// the end of each turn. Subtype `"turn_duration"` (#273). The
    /// renderer composes `Claude · N.Ns` banner-side chip from
    /// `ms`.
    TurnDuration {
        /// Total wall-clock duration of the turn in milliseconds.
        /// Wire field is `durationMs` (camelCase).
        ms: u64,
        /// Number of assistant + tool messages in the turn. Wire
        /// field is `messageCount`. Optional because older CLI
        /// versions may omit it.
        message_count: Option<u64>,
        /// Parent tool-use id when the turn is a sub-agent.
        parent_tool_use_id: Option<String>,
        /// Session id the event applies to.
        session_id: String,
        /// Unique identifier for this turn_duration event.
        uuid: String,
    },

    /// Stop-hook execution summary surfaced at end-of-turn. Subtype
    /// `"stop_hook_summary"` (#273). The renderer composes a
    /// collapsed 1-liner `↳ hook summary · N actions [▶ expand]`
    /// from `actions` (wire `hookCount`); expanded view enumerates
    /// `hook_infos`.
    StopHookSummary {
        /// Number of hooks that fired. Wire field is `hookCount`.
        /// Renderer hides the surface entirely when this is 0.
        actions: u32,
        /// Per-hook breakdown. Wire field is `hookInfos`. Each entry
        /// carries the command string + its duration in ms.
        hook_infos: Vec<StopHookInfo>,
        /// Whether any hook produced output. Wire field is
        /// `hasOutput`. Forwarded to the renderer so a zero-actions
        /// + has-output edge case can be surfaced if needed.
        has_output: bool,
        /// Suggestion vs blocking level. Wire field is `level`.
        level: String,
        /// Whether any hook prevented turn continuation. Wire field
        /// is `preventedContinuation`.
        prevented_continuation: bool,
        /// Stop reason string from the CLI; usually empty.
        stop_reason: String,
        /// `tool_use_id` the hook batch was bound to. Wire field is
        /// `toolUseID`.
        tool_use_id: String,
        /// Parent tool-use id when the hook fires in a sub-agent context.
        parent_tool_use_id: Option<String>,
        /// Session id the event applies to.
        session_id: String,
        /// Unique identifier for this stop_hook_summary event.
        uuid: String,
    },

    /// Rate-limit state transition. The CLI emits this when the current
    /// rate-limit window changes state (e.g. `allowed` → `allowed_warning`).
    /// Wire shape mirrors +
    ///.
    RateLimitEvent {
        /// Rate-limit snapshot at the moment of the transition.
        rate_limit_info: RateLimitInfo,
        /// Unique identifier for this rate-limit event.
        uuid: String,
        /// Session id the event applies to.
        session_id: String,
    },

    /// End-of-turn or end-of-session summary with cost and usage.
    ///
    /// Only six fields are required on the wire (`subtype`,
    /// `session_id`, `is_error`, `num_turns`, `duration_ms`,
    /// `duration_api_ms`); every other field is `Option<...>` because
    /// the CLI omits them silently when not applicable.
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
        /// Why the turn ended, if the CLI reported a stop reason
        /// (e.g. `"end_turn"`, `"max_turns"`, `"error"`).
        stop_reason: Option<String>,
        /// Total cost so far in USD. `None` when the CLI can't compute
        /// or doesn't report (free-tier sessions, error-path results).
        total_cost_usd: Option<f64>,
        /// Aggregate token usage for the turn. Optional - the CLI
        /// omits the field on error-path frames.
        usage: Option<Usage>,
        /// Plain-text result body when the turn produced one (e.g. the
        /// assistant's final output).
        result: Option<String>,
        /// Structured output when
        /// `forge_sdk::Options::output_format` was set to a JSON
        /// schema. Passed through verbatim.
        structured_output: Option<Value>,
        /// Per-model usage breakdown. Wire key is camelCase
        /// `modelUsage` (matches the CLI's `data.get("modelUsage")`).
        model_usage: Option<Value>,
        /// Permissions denied during the turn, surfaced so callers can
        /// audit `can_use_tool` outcomes.
        permission_denials: Option<Vec<Value>>,
        /// Non-fatal errors accumulated during the turn.
        errors: Option<Vec<String>>,
        /// Unique identifier for this result frame.
        uuid: Option<String>,
        /// Why the turn ended at the runtime layer (`completed`,
        /// `aborted_streaming`, `max_turns`, etc.). The CLI reports
        /// this on `result` frames; surfaced here so consumers don't
        /// have to re-parse the wire JSON to read it.
        terminal_reason: Option<TerminalReason>,
        /// Fast-mode state at the moment the turn ended. The CLI
        /// includes this on `result` frames as a state echo.
        fast_mode_state: Option<FastModeState>,
    },

    /// Streaming partial-message event emitted when
    /// `forge_sdk::Options::include_partial_messages` is set. The
    /// CLI forwards raw Anthropic-API stream events (`message_start`,
    /// `content_block_delta`, `message_delta`, `message_stop`) without
    /// coalescing them into complete turns. SDK's
    /// `StreamEvent`
    ///).
    StreamEvent {
        /// Unique identifier for this stream event.
        uuid: String,
        /// Session the event belongs to.
        session_id: String,
        /// Raw Anthropic API stream event payload.
        event: Value,
        /// Parent `tool_use` id when the emitting turn is a sub-agent.
        parent_tool_use_id: Option<String>,
    },

    /// Fatal transport error injected into the message stream when the
    /// CLI's read loop fails. the CLI emits this at
    /// as a last-gasp signal before teardown - emitted by the
    /// CLI's read loop. forge-sdk surfaces it via
    /// the events stream returned by `forge_sdk::Client::spawn` so callers
    /// see the failure on the iterator rather than via a side
    /// channel.
    Error {
        /// The failure message as the CLI stringified it.
        error: String,
    },

    /// Forward-compat fallback: a frame whose top-level `type` value
    /// forge-sdk doesn't recognise. Surfaced through
    /// the events stream returned by `forge_sdk::Client::spawn` when the codec
    /// produces a the SDK's `DecodedLine::Unknown`.
    /// Library consumers can match this variant to detect upstream CLI
    /// drift programmatically (telemetry, structured alerts) instead of
    /// relying on `tracing::warn!` log scraping.
    ///
    /// Mirrors the `Unknown` pattern already used by
    /// [`ContentBlock`] and `forge_sdk::control::ControlRequestKind`.
    /// Never produced by deserialization - `decode_dispatch` filters
    /// unknown types into the SDK's `DecodedLine::Unknown`
    /// before they reach serde - but `Serialize` round-trips `raw`
    /// verbatim so logs / replay capture the original bytes.
    Unknown {
        /// Raw `type` field value as the CLI sent it.
        type_str: String,
        /// Full original JSON object - preserved for inspection,
        /// replay, or rehydration once the new shape is supported.
        raw: Value,
    },
}

impl Message {
    /// Extract the `session_id` a message is tagged with, when present.
    ///
    /// Used by the events stream returned by `forge_sdk::Client::spawn` to bind
    /// the client's `session_id` field on the first frame that carries
    /// one - the CLI in stream-json interactive mode only emits
    /// `system/init` (the canonical session-id source) AFTER both an
    /// initialize `control_request` AND a user message have been seen,
    /// so the session id isn't known at spawn time.
    ///
    /// Returns `None` for the two variants that aren't session-scoped
    /// (`Error` and `RateLimitEvent`, which carries no session id of its
    /// own).
    pub fn session_id(&self) -> Option<&str> {
        match self {
            Message::Assistant { session_id, .. }
            | Message::User { session_id, .. }
            | Message::TaskStarted { session_id, .. }
            | Message::TaskUpdated { session_id, .. }
            | Message::TaskProgress { session_id, .. }
            | Message::TaskNotification { session_id, .. }
            | Message::ThinkingTokens { session_id, .. }
            | Message::TurnDuration { session_id, .. }
            | Message::StopHookSummary { session_id, .. }
            | Message::Result { session_id, .. }
            | Message::StreamEvent { session_id, .. } => Some(session_id.as_str()),
            Message::System { session_id, .. } => session_id.as_deref(),
            Message::RateLimitEvent { .. } | Message::Error { .. } | Message::Unknown { .. } => {
                None
            }
        }
    }
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
    /// Token usage for this turn. Optional - error-path frames
    /// don't carry a usage block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

/// Classification of a failure the CLI attributes to an assistant turn.
/// `AssistantMessageError` union.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssistantMessageError {
    /// The model couldn't be reached because authentication failed.
    AuthenticationFailed,
    /// The account hit a billing problem (e.g. no active credits).
    BillingError,
    /// Rate-limit rejection - retry after the window resets.
    RateLimit,
    /// The request was rejected as malformed.
    InvalidRequest,
    /// Generic server-side error.
    ServerError,
    /// Fallback for error classes forge-sdk doesn't yet recognise.
    #[serde(other)]
    Unknown,
}

/// Envelope inside a `User` message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserEnvelope {
    /// Fixed value `"user"`.
    pub role: String,
    /// Content blocks - usually `ToolResult` blocks when reporting
    /// tool outputs. Wire shape is `list | str`: a bare string is
    /// accepted on the way in and normalised into a single
    /// [`ContentBlock::Text`] block; serialising always emits the
    /// list form.
    #[serde(deserialize_with = "deserialize_user_content")]
    pub content: Vec<ContentBlock>,
}

fn deserialize_user_content<'de, D>(de: D) -> Result<Vec<ContentBlock>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;
    let value = Value::deserialize(de)?;
    match value {
        Value::String(s) => Ok(vec![ContentBlock::Text { text: s }]),
        Value::Array(_) => serde_json::from_value(value).map_err(D::Error::custom),
        other => {
            Err(D::Error::custom(format!("user message content must be str or list, got: {other}")))
        }
    }
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
    /// A stop reason forge-sdk doesn't yet recognise.
    #[serde(other)]
    Unknown,
}

/// Rate-limit window status. Wire literal:
/// `"allowed" | "allowed_warning" | "rejected"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitStatus {
    /// Within the window - no restrictions.
    Allowed,
    /// Approaching the limit; callers should warn / back off soon.
    AllowedWarning,
    /// Limit hit; requests are being refused.
    Rejected,
    /// A status forge-sdk doesn't yet recognise.
    #[serde(other)]
    Unknown,
}

/// Which rate-limit window applies.
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
    /// A rate-limit window forge-sdk doesn't yet recognise.
    #[serde(other)]
    Unknown,
}

/// Rate-limit snapshot emitted inside a [`Message::RateLimitEvent`].
///
/// `RateLimitInfo`. Inner
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
    #[serde(default, rename = "rateLimitType", skip_serializing_if = "Option::is_none")]
    pub rate_limit_type: Option<RateLimitType>,
    /// Fraction of the rate limit consumed (0.0 - 1.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub utilization: Option<f64>,
    /// Status of overage / pay-as-you-go usage, if applicable.
    #[serde(default, rename = "overageStatus", skip_serializing_if = "Option::is_none")]
    pub overage_status: Option<RateLimitStatus>,
    /// Unix timestamp (seconds) when overage window resets.
    #[serde(default, rename = "overageResetsAt", skip_serializing_if = "Option::is_none")]
    pub overage_resets_at: Option<i64>,
    /// Why overage is unavailable when rejected.
    #[serde(default, rename = "overageDisabledReason", skip_serializing_if = "Option::is_none")]
    pub overage_disabled_reason: Option<String>,
    /// Echo of the raw CLI payload so callers can introspect fields
    /// forge-sdk doesn't yet type. serde's `flatten` makes this the
    /// catch-all bucket for unknown keys on the wire.
    #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub raw: serde_json::Map<String, serde_json::Value>,
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

/// Per-hook entry inside a `stop_hook_summary` system event (#273).
/// Wire fields are camelCase (`durationMs`); the renderer reads
/// the `command` text for the expanded body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopHookInfo {
    /// Command line that fired.
    pub command: String,
    /// How long the hook took, in milliseconds. Wire field is
    /// `durationMs`.
    #[serde(rename = "durationMs")]
    pub duration_ms: u64,
}

/// Usage counters reported inside task-progress and task-notification frames.
///
/// `TaskUsage`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskUsage {
    /// Tokens consumed across all model calls in this task so far.
    pub total_tokens: u64,
    /// Number of tool invocations the sub-agent has made.
    pub tool_uses: u64,
    /// Wall-clock time the task has spent running, in milliseconds.
    pub duration_ms: u64,
}

/// Terminal status of a sub-agent `Task` reported via
/// [`Message::TaskNotification`]. Wire literal:
/// `"completed" | "failed" | "stopped"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskNotificationStatus {
    /// Task finished successfully.
    Completed,
    /// Task exited with an error.
    Failed,
    /// Task was cancelled before it could finish.
    Stopped,
    /// A status forge-sdk doesn't yet recognise.
    #[serde(other)]
    Unknown,
}

/// Patch payload carried by [`Message::TaskUpdated`]. Fields are
/// optional because the CLI emits patches that update only the
/// changed fields. `status` is the free-form wire string (e.g.
/// `"completed"`, `"running"`, `"killed"`) - keeping it as `String`
/// rather than an enum lets the reducer accept future statuses
/// without a forge-primitives bump.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TaskUpdatePatch {
    /// New status the task transitioned to, if the patch carries one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Unix-millis wall-clock end time the CLI stamped on the task,
    /// if applicable (terminal transitions usually set this).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_time: Option<u64>,
}

/// Workflow's per-event snapshot of the workflow's
/// phase + agent state, ridden via `Message::TaskProgress`'s
/// `workflow_progress` field.
///
/// The CLI fires one `system/task_progress` per workflow-internal
/// state change; each event carries the FULL workflow snapshot at
/// that instant (not a delta). Two flavours of entry are observed in
/// captured wire (see `~/Projects/forge/.claude/skills/claude-cli-upgrade/reference-captures/workflow.jsonl`):
///
/// 1. `workflow_phase` - phase-level marker emitted when the
///    workflow's `phase()` call fires. Carries the phase index +
///    title.
/// 2. `workflow_agent` - agent-level event tracking an agent call's
///    state transition (`start` → `progress` → `done`). Carries the
///    parent phase index, the agent's queued model, the running
///    tool name (when known), a short prompt preview, and (on
///    `done`) the result preview.
///
/// Both share the same wire envelope discriminated by `type`. The
/// type stays open via `#[serde(other)]` on the trailing variant
/// so future CLI additions decode cleanly without a primitives
/// bump.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowProgressEvent {
    /// Phase-level marker - emitted when the workflow's `phase()`
    /// call fires.
    WorkflowPhase { index: u32, title: String },
    /// Agent-level event - emitted on each state transition for an
    /// agent call inside a phase. Only the fields the renderer
    /// actually surfaces are decoded; the wire also carries
    /// `queuedAt`, `startedAt`, `lastProgressAt`, `attempt`,
    /// `tokens`, `toolCalls`, `durationMs`, `model`, `agentId`,
    /// `promptPreview` etc. which are skipped here to keep the
    /// rendered surface focused.
    WorkflowAgent {
        index: u32,
        label: String,
        #[serde(rename = "phaseIndex")]
        phase_index: u32,
        #[serde(rename = "phaseTitle")]
        phase_title: String,
        /// Current agent state on the wire: `start`, `progress`,
        /// `done`. Free-form string so future states decode
        /// without a primitives bump.
        state: String,
        /// Latest tool the agent invoked, when known. `None` on
        /// initial `start` events.
        #[serde(rename = "lastToolName", default, skip_serializing_if = "Option::is_none")]
        last_tool_name: Option<String>,
        /// Short summary of the latest tool's output.
        #[serde(rename = "lastToolSummary", default, skip_serializing_if = "Option::is_none")]
        last_tool_summary: Option<String>,
        /// Final structured-output preview emitted with the
        /// `state: done` event. JSON-stringified per the CLI's
        /// `resultPreview` field.
        #[serde(rename = "resultPreview", default, skip_serializing_if = "Option::is_none")]
        result_preview: Option<String>,
    },
    /// Unrecognised workflow event - preserved across decode to
    /// avoid serde refusing the surrounding `task_progress`. Not
    /// surfaced anywhere; the renderer treats unknown event types
    /// as no-ops.
    #[serde(other)]
    Other,
}

// ---------------------------------------------------------------------------
// Wire shim - serde sees this, users never do.
//
// `Message` has the user-facing variant layout. `MessageRepr` encodes the
// actual wire dispatch: first on `type`, then (for `type="system"`) on
// `subtype`. The cascade works because:
//
// * `MessageRepr` is internally-tagged on `type` - serde picks `System(repr)`
//   when `type="system"`, the rest via tag rename.
// * `SystemRepr` is untagged - serde tries `Typed(TypedSystemRepr)` first
//   (which is itself internally-tagged on `subtype`), then falls back to
//   `Generic(GenericSystemRepr)` for subtypes we don't recognise.
// * `TypedSystemRepr` dispatches the known task-lifecycle subtypes.
// * `GenericSystemRepr` captures any other subtype into the opaque
//   `data: Value`, which is what `Message::System` surfaces to users.
//
// `Message::Unknown` is the only variant Serialize/Deserialize don't route
// through `MessageRepr` - its `raw` field already carries the original
// JSON, so we emit it verbatim instead of fabricating a synthetic wire
// shape. Deserialize never produces it; `decode_dispatch` filters unknown
// types into `DecodedLine::Unknown` before serde sees them.
// ---------------------------------------------------------------------------

impl Serialize for Message {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Message::Unknown { raw, .. } => raw.serialize(serializer),
            other => MessageRepr::from(other.clone()).serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Message {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        MessageRepr::deserialize(deserializer).map(Message::from)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum MessageRepr {
    Assistant {
        message: AssistantEnvelope,
        session_id: String,
        #[serde(default)]
        parent_tool_use_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<AssistantMessageError>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uuid: Option<String>,
    },
    User {
        message: UserEnvelope,
        session_id: String,
        #[serde(default)]
        parent_tool_use_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uuid: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_use_result: Option<Value>,
    },
    System(SystemRepr),
    RateLimitEvent {
        rate_limit_info: RateLimitInfo,
        uuid: String,
        session_id: String,
    },
    Result {
        subtype: String,
        session_id: String,
        is_error: bool,
        num_turns: u64,
        duration_ms: u64,
        duration_api_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        total_cost_usd: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        structured_output: Option<Value>,
        #[serde(default, rename = "modelUsage", skip_serializing_if = "Option::is_none")]
        model_usage: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        permission_denials: Option<Vec<Value>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        errors: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uuid: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        terminal_reason: Option<TerminalReason>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fast_mode_state: Option<FastModeState>,
    },
    StreamEvent {
        uuid: String,
        session_id: String,
        event: Value,
        #[serde(default)]
        parent_tool_use_id: Option<String>,
    },
    Error {
        error: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum SystemRepr {
    Typed(TypedSystemRepr),
    Generic(GenericSystemRepr),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
// Wire-shape enum - variants share the `Task` prefix to match the CLI's wire-tag scheme.
#[allow(clippy::enum_variant_names)]
enum TypedSystemRepr {
    TaskStarted {
        task_id: String,
        description: String,
        uuid: String,
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_type: Option<String>,
    },
    TaskUpdated {
        task_id: String,
        #[serde(default)]
        patch: TaskUpdatePatch,
        uuid: String,
        session_id: String,
    },
    TaskProgress {
        task_id: String,
        description: String,
        usage: TaskUsage,
        uuid: String,
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_tool_name: Option<String>,
        /// Workflow tool's per-event snapshot of the
        /// workflow's phase + agent state. Present only when the
        /// originating tool is `Workflow`; otherwise omitted. Each
        /// event is the FULL snapshot (not a delta) of every phase
        /// + agent currently known to the workflow.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        workflow_progress: Vec<WorkflowProgressEvent>,
    },
    TaskNotification {
        task_id: String,
        status: TaskNotificationStatus,
        output_file: String,
        summary: String,
        uuid: String,
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<TaskUsage>,
    },
    ThinkingTokens {
        estimated_tokens: u64,
        estimated_tokens_delta: i64,
        uuid: String,
        session_id: String,
    },
    TurnDuration {
        #[serde(rename = "durationMs")]
        ms: u64,
        #[serde(default, rename = "messageCount", skip_serializing_if = "Option::is_none")]
        message_count: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_tool_use_id: Option<String>,
        session_id: String,
        uuid: String,
    },
    StopHookSummary {
        #[serde(rename = "hookCount")]
        actions: u32,
        #[serde(default, rename = "hookInfos")]
        hook_infos: Vec<StopHookInfo>,
        #[serde(rename = "hasOutput")]
        has_output: bool,
        level: String,
        #[serde(rename = "preventedContinuation")]
        prevented_continuation: bool,
        #[serde(rename = "stopReason")]
        stop_reason: String,
        #[serde(rename = "toolUseID")]
        tool_use_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_tool_use_id: Option<String>,
        session_id: String,
        uuid: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GenericSystemRepr {
    subtype: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(flatten)]
    data: Value,
}

impl From<MessageRepr> for Message {
    fn from(repr: MessageRepr) -> Self {
        match repr {
            MessageRepr::Assistant { message, session_id, parent_tool_use_id, error, uuid } => {
                Message::Assistant { message, session_id, parent_tool_use_id, error, uuid }
            }
            MessageRepr::User {
                message,
                session_id,
                parent_tool_use_id,
                uuid,
                tool_use_result,
            } => Message::User { message, session_id, parent_tool_use_id, uuid, tool_use_result },
            MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::TaskStarted {
                task_id,
                description,
                uuid,
                session_id,
                tool_use_id,
                task_type,
            })) => Message::TaskStarted {
                task_id,
                description,
                uuid,
                session_id,
                tool_use_id,
                task_type,
            },
            MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::TaskUpdated {
                task_id,
                patch,
                uuid,
                session_id,
            })) => Message::TaskUpdated { task_id, patch, uuid, session_id },
            MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::TaskProgress {
                task_id,
                description,
                usage,
                uuid,
                session_id,
                tool_use_id,
                last_tool_name,
                workflow_progress,
            })) => Message::TaskProgress {
                task_id,
                description,
                usage,
                uuid,
                session_id,
                tool_use_id,
                last_tool_name,
                workflow_progress,
            },
            MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::TaskNotification {
                task_id,
                status,
                output_file,
                summary,
                uuid,
                session_id,
                tool_use_id,
                usage,
            })) => Message::TaskNotification {
                task_id,
                status,
                output_file,
                summary,
                uuid,
                session_id,
                tool_use_id,
                usage,
            },
            MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::ThinkingTokens {
                estimated_tokens,
                estimated_tokens_delta,
                uuid,
                session_id,
            })) => Message::ThinkingTokens {
                estimated_tokens,
                estimated_tokens_delta,
                uuid,
                session_id,
            },
            MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::TurnDuration {
                ms,
                message_count,
                parent_tool_use_id,
                session_id,
                uuid,
            })) => {
                Message::TurnDuration { ms, message_count, parent_tool_use_id, session_id, uuid }
            }
            MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::StopHookSummary {
                actions,
                hook_infos,
                has_output,
                level,
                prevented_continuation,
                stop_reason,
                tool_use_id,
                parent_tool_use_id,
                session_id,
                uuid,
            })) => Message::StopHookSummary {
                actions,
                hook_infos,
                has_output,
                level,
                prevented_continuation,
                stop_reason,
                tool_use_id,
                parent_tool_use_id,
                session_id,
                uuid,
            },
            MessageRepr::System(SystemRepr::Generic(GenericSystemRepr {
                subtype,
                session_id,
                data,
            })) => {
                // The CLI's system message wire shape carries the
                // FULL original dict in `data` - including `type`,
                // `subtype`, and `session_id`. Rust's serde
                // `#[flatten]` on the private `GenericSystemRepr`
                // strips those fields because they're claimed by
                // explicit sibling fields and the outer tag dispatch.
                // Rehydrate so callers reading `data["subtype"]` see
                // the unified shape.
                let mut full_data = data;
                if let Value::Object(map) = &mut full_data {
                    map.insert("type".into(), Value::String("system".into()));
                    map.insert("subtype".into(), Value::String(subtype.clone()));
                    if let Some(sid) = &session_id {
                        map.insert("session_id".into(), Value::String(sid.clone()));
                    }
                }
                Message::System { subtype, session_id, data: full_data }
            }
            MessageRepr::RateLimitEvent { rate_limit_info, uuid, session_id } => {
                Message::RateLimitEvent { rate_limit_info, uuid, session_id }
            }
            MessageRepr::Result {
                subtype,
                session_id,
                is_error,
                num_turns,
                duration_ms,
                duration_api_ms,
                stop_reason,
                total_cost_usd,
                usage,
                result,
                structured_output,
                model_usage,
                permission_denials,
                errors,
                uuid,
                terminal_reason,
                fast_mode_state,
            } => Message::Result {
                subtype,
                session_id,
                is_error,
                num_turns,
                duration_ms,
                duration_api_ms,
                stop_reason,
                total_cost_usd,
                usage,
                result,
                structured_output,
                model_usage,
                permission_denials,
                errors,
                uuid,
                terminal_reason,
                fast_mode_state,
            },
            MessageRepr::StreamEvent { uuid, session_id, event, parent_tool_use_id } => {
                Message::StreamEvent { uuid, session_id, event, parent_tool_use_id }
            }
            MessageRepr::Error { error } => Message::Error { error },
        }
    }
}

impl From<Message> for MessageRepr {
    fn from(msg: Message) -> Self {
        match msg {
            Message::Assistant { message, session_id, parent_tool_use_id, error, uuid } => {
                MessageRepr::Assistant { message, session_id, parent_tool_use_id, error, uuid }
            }
            Message::User { message, session_id, parent_tool_use_id, uuid, tool_use_result } => {
                MessageRepr::User { message, session_id, parent_tool_use_id, uuid, tool_use_result }
            }
            Message::System { subtype, session_id, data } => {
                // `data` now carries the full shape (including `type`,
                // `subtype`, `session_id`). On the way back out, strip
                // those keys from the flatten payload so the outer
                // tag-dispatch + explicit sibling fields don't produce
                // duplicates on the wire.
                let mut flat = data;
                if let Value::Object(map) = &mut flat {
                    map.remove("type");
                    map.remove("subtype");
                    map.remove("session_id");
                }
                MessageRepr::System(SystemRepr::Generic(GenericSystemRepr {
                    subtype,
                    session_id,
                    data: flat,
                }))
            }
            Message::TaskStarted {
                task_id,
                description,
                uuid,
                session_id,
                tool_use_id,
                task_type,
            } => MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::TaskStarted {
                task_id,
                description,
                uuid,
                session_id,
                tool_use_id,
                task_type,
            })),
            Message::TaskUpdated { task_id, patch, uuid, session_id } => {
                MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::TaskUpdated {
                    task_id,
                    patch,
                    uuid,
                    session_id,
                }))
            }
            Message::TaskProgress {
                task_id,
                description,
                usage,
                uuid,
                session_id,
                tool_use_id,
                last_tool_name,
                workflow_progress,
            } => MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::TaskProgress {
                task_id,
                description,
                usage,
                uuid,
                session_id,
                tool_use_id,
                last_tool_name,
                workflow_progress,
            })),
            Message::TaskNotification {
                task_id,
                status,
                output_file,
                summary,
                uuid,
                session_id,
                tool_use_id,
                usage,
            } => MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::TaskNotification {
                task_id,
                status,
                output_file,
                summary,
                uuid,
                session_id,
                tool_use_id,
                usage,
            })),
            Message::ThinkingTokens {
                estimated_tokens,
                estimated_tokens_delta,
                uuid,
                session_id,
            } => MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::ThinkingTokens {
                estimated_tokens,
                estimated_tokens_delta,
                uuid,
                session_id,
            })),
            Message::TurnDuration { ms, message_count, parent_tool_use_id, session_id, uuid } => {
                MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::TurnDuration {
                    ms,
                    message_count,
                    parent_tool_use_id,
                    session_id,
                    uuid,
                }))
            }
            Message::StopHookSummary {
                actions,
                hook_infos,
                has_output,
                level,
                prevented_continuation,
                stop_reason,
                tool_use_id,
                parent_tool_use_id,
                session_id,
                uuid,
            } => MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::StopHookSummary {
                actions,
                hook_infos,
                has_output,
                level,
                prevented_continuation,
                stop_reason,
                tool_use_id,
                parent_tool_use_id,
                session_id,
                uuid,
            })),
            Message::RateLimitEvent { rate_limit_info, uuid, session_id } => {
                MessageRepr::RateLimitEvent { rate_limit_info, uuid, session_id }
            }
            Message::Result {
                subtype,
                session_id,
                is_error,
                num_turns,
                duration_ms,
                duration_api_ms,
                stop_reason,
                total_cost_usd,
                usage,
                result,
                structured_output,
                model_usage,
                permission_denials,
                errors,
                uuid,
                terminal_reason,
                fast_mode_state,
            } => MessageRepr::Result {
                subtype,
                session_id,
                is_error,
                num_turns,
                duration_ms,
                duration_api_ms,
                stop_reason,
                total_cost_usd,
                usage,
                result,
                structured_output,
                model_usage,
                permission_denials,
                errors,
                uuid,
                terminal_reason,
                fast_mode_state,
            },
            Message::StreamEvent { uuid, session_id, event, parent_tool_use_id } => {
                MessageRepr::StreamEvent { uuid, session_id, event, parent_tool_use_id }
            }
            Message::Error { error } => MessageRepr::Error { error },
            // Defensive sentinel - `Serialize` for `Message` special-cases
            // `Unknown` to emit `raw` verbatim, so this branch is dead code
            // at runtime. Kept to keep the `From` impl total without
            // `unreachable!()` (banned by the workspace lint set).
            Message::Unknown { type_str, .. } => MessageRepr::Error {
                error: format!(
                    "Message::Unknown {{ type_str: {type_str:?} }} \
                     cannot be encoded via MessageRepr"
                ),
            },
        }
    }
}

#[cfg(test)]
mod tests_result_message_fields {
    // Test-mod `use super::*;` brings the parent's full surface in; not every test consumes every item.
    #[allow(unused_imports)]
    use super::*;

    use crate::Message;
    use serde_json::json;

    /// Minimum-viable result frame - only the six required fields.
    /// The CLI emits this on error-path turns; forge-sdk must accept
    /// it.
    #[test]
    fn minimal_result_parses_without_cost_or_usage() {
        let raw = json!({
            "type": "result",
            "subtype": "success",
            "duration_ms": 10,
            "duration_api_ms": 8,
            "is_error": false,
            "num_turns": 1,
            "session_id": "sess-min",
        });
        let msg: Message = serde_json::from_value(raw).expect("parse");
        match msg {
            Message::Result {
                total_cost_usd,
                usage,
                stop_reason,
                result,
                structured_output,
                model_usage,
                permission_denials,
                errors,
                uuid,
                ..
            } => {
                assert!(total_cost_usd.is_none());
                assert!(usage.is_none());
                assert!(stop_reason.is_none());
                assert!(result.is_none());
                assert!(structured_output.is_none());
                assert!(model_usage.is_none());
                assert!(permission_denials.is_none());
                assert!(errors.is_none());
                assert!(uuid.is_none());
            }
            other => panic!("expected Result, got {other:?}"),
        }
    }

    /// Full payload - every optional field populated. Exercises the
    /// `modelUsage` camelCase wire key and captures the result body, the
    /// permission-denial vector, and the error vector.
    #[test]
    fn full_result_parses_and_surfaces_every_field() {
        let raw = json!({
            "type": "result",
            "subtype": "success",
            "duration_ms": 5000,
            "duration_api_ms": 4200,
            "is_error": false,
            "num_turns": 7,
            "session_id": "sess-full",
            "stop_reason": "end_turn",
            "total_cost_usd": 0.123,
            "usage": {
                "input_tokens": 100,
                "output_tokens": 50,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0
            },
            "result": "hello world",
            "structured_output": { "answer": 42 },
            "modelUsage": {
                "claude-sonnet-4-6": { "input_tokens": 60, "output_tokens": 30 }
            },
            "permission_denials": [
                { "tool_name": "Bash", "reason": "dry run" }
            ],
            "errors": ["warning: slow"],
            "uuid": "res-1"
        });
        let msg: Message = serde_json::from_value(raw).expect("parse");
        let Message::Result {
            stop_reason,
            total_cost_usd,
            usage,
            result,
            structured_output,
            model_usage,
            permission_denials,
            errors,
            uuid,
            ..
        } = msg
        else {
            panic!("expected Result");
        };
        assert_eq!(stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(total_cost_usd, Some(0.123));
        assert!(usage.is_some());
        assert_eq!(result.as_deref(), Some("hello world"));
        assert_eq!(structured_output, Some(json!({ "answer": 42 })));
        assert!(model_usage.is_some(), "modelUsage should decode");
        assert_eq!(model_usage.as_ref().unwrap()["claude-sonnet-4-6"]["input_tokens"], 60);
        assert_eq!(
            permission_denials.as_deref().map(<[_]>::len),
            Some(1),
            "permission_denials should surface"
        );
        assert_eq!(errors.as_deref(), Some(vec!["warning: slow".to_string()]).as_deref());
        assert_eq!(uuid.as_deref(), Some("res-1"));
    }

    /// modelUsage must serialize back out as camelCase on the wire - the
    /// typical caller-side scenario is round-tripping a decoded result
    /// through session-store persistence.
    #[test]
    fn result_model_usage_roundtrips_as_camel_case() {
        let raw = json!({
            "type": "result",
            "subtype": "success",
            "duration_ms": 1,
            "duration_api_ms": 1,
            "is_error": false,
            "num_turns": 1,
            "session_id": "sess-rt",
            "modelUsage": { "claude-sonnet-4-6": { "input_tokens": 1 } }
        });
        let msg: Message = serde_json::from_value(raw.clone()).expect("parse");
        let re = serde_json::to_value(&msg).expect("serialize");
        assert_eq!(
            re["modelUsage"]["claude-sonnet-4-6"]["input_tokens"], 1,
            "must preserve camelCase on the way out"
        );
        assert!(re.get("model_usage").is_none(), "snake_case key must NOT leak");
    }
}

#[cfg(test)]
mod tests_message_extras {
    // Test-mod `use super::*;` brings the parent's full surface in; not every test consumes every item.
    #[allow(unused_imports)]
    use super::*;

    use crate::{AssistantMessageError, Message};
    use serde_json::json;

    #[test]
    fn assistant_error_enum_wire_names() {
        // Each variant must serialize to its wire literal.
        for (variant, wire) in [
            (AssistantMessageError::AuthenticationFailed, "authentication_failed"),
            (AssistantMessageError::BillingError, "billing_error"),
            (AssistantMessageError::RateLimit, "rate_limit"),
            (AssistantMessageError::InvalidRequest, "invalid_request"),
            (AssistantMessageError::ServerError, "server_error"),
            (AssistantMessageError::Unknown, "unknown"),
        ] {
            let encoded = serde_json::to_value(variant).expect("serialize");
            assert_eq!(encoded, json!(wire), "{variant:?} must wire as '{wire}'");
            let decoded: AssistantMessageError =
                serde_json::from_value(json!(wire)).expect("deserialize");
            assert_eq!(decoded, variant);
        }
    }

    #[test]
    fn assistant_frame_decodes_error_and_uuid_outer_fields() {
        let raw = json!({
            "type": "assistant",
            "session_id": "sess-err",
            "uuid": "asst-uuid-1",
            "error": "rate_limit",
            "message": {
                "id": "msg_01",
                "role": "assistant",
                "model": "claude-opus-4-5",
                "content": [{"type": "text", "text": "throttled"}],
                "usage": {
                    "input_tokens": 0,
                    "output_tokens": 0,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0
                }
            }
        });
        let msg: Message = serde_json::from_value(raw).expect("parse");
        match msg {
            Message::Assistant { error, uuid, .. } => {
                assert_eq!(error, Some(AssistantMessageError::RateLimit));
                assert_eq!(uuid.as_deref(), Some("asst-uuid-1"));
            }
            other => panic!("expected Assistant, got {other:?}"),
        }
    }

    #[test]
    fn assistant_frame_without_usage_now_parses() {
        // Usage is optional on the wire - error-path assistant
        // frames omit it. forge-sdk must parse them (regression
        // guard against the pre-2026-04-22 required-`usage` shape).
        let raw = json!({
            "type": "assistant",
            "session_id": "sess",
            "message": {
                "id": "msg_err",
                "role": "assistant",
                "model": "claude-opus-4-5",
                "content": []
            }
        });
        let msg: Message = serde_json::from_value(raw).expect("parse");
        match msg {
            Message::Assistant { message, .. } => {
                assert!(message.usage.is_none());
            }
            other => panic!("expected Assistant, got {other:?}"),
        }
    }

    #[test]
    fn user_frame_decodes_uuid_and_tool_use_result() {
        let raw = json!({
            "type": "user",
            "session_id": "sess-usr",
            "uuid": "user-uuid-1",
            "tool_use_result": {"stdout": "ok"},
            "message": {
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_01", "content": "ok", "is_error": false}
                ]
            }
        });
        let msg: Message = serde_json::from_value(raw).expect("parse");
        match msg {
            Message::User { uuid, tool_use_result, .. } => {
                assert_eq!(uuid.as_deref(), Some("user-uuid-1"));
                assert_eq!(tool_use_result, Some(json!({"stdout": "ok"})));
            }
            other => panic!("expected User, got {other:?}"),
        }
    }

    #[test]
    fn unknown_wire_values_decode_to_the_catch_all_variant() {
        // serde(other) on these wire enums must absorb a value the CLI
        // adds later so a new literal degrades to Unknown instead of
        // failing the whole frame decode. Feed a genuinely-foreign
        // string: a tautological `"unknown"` would round-trip by name
        // even without the catch-all, so it wouldn't guard the attribute.
        let foreign = || json!("some_future_value_forge_does_not_know");
        assert_eq!(
            serde_json::from_value::<AssistantMessageError>(foreign()).expect("decode"),
            AssistantMessageError::Unknown
        );
        assert_eq!(
            serde_json::from_value::<StopReason>(foreign()).expect("decode"),
            StopReason::Unknown
        );
        assert_eq!(
            serde_json::from_value::<RateLimitStatus>(foreign()).expect("decode"),
            RateLimitStatus::Unknown
        );
        assert_eq!(
            serde_json::from_value::<RateLimitType>(foreign()).expect("decode"),
            RateLimitType::Unknown
        );
        assert_eq!(
            serde_json::from_value::<TaskNotificationStatus>(foreign()).expect("decode"),
            TaskNotificationStatus::Unknown
        );
        assert_eq!(
            serde_json::from_value::<crate::runtime::TerminalReason>(foreign()).expect("decode"),
            crate::runtime::TerminalReason::Unknown
        );
    }

    // ----------------------------------------------------------------
    // #273: CLI 2.1.156 system events (thinking_tokens / turn_duration
    // / stop_hook_summary). Wire shapes captured in
    // crates/forge-test-harness/baselines/sdk/2.1.156/* and pinned by
    // the roundtrip tests below.
    // ----------------------------------------------------------------

    #[test]
    fn thinking_tokens_decodes_from_wire_shape() {
        // Wire shape (verbatim from
        // `baselines/sdk/2.1.156/subagent_*` captures): fields are
        // snake_case (NOT camelCase as the EPIC body suggested).
        let raw = json!({
            "type": "system",
            "subtype": "thinking_tokens",
            "estimated_tokens": 1234,
            "estimated_tokens_delta": 56,
            "uuid": "tt-uuid",
            "session_id": "sess-tt",
        });
        let msg: Message = serde_json::from_value(raw.clone()).expect("decode");
        let Message::ThinkingTokens { estimated_tokens, estimated_tokens_delta, uuid, session_id } =
            msg
        else {
            panic!("expected ThinkingTokens, got {msg:?}");
        };
        assert_eq!(estimated_tokens, 1234);
        assert_eq!(estimated_tokens_delta, 56);
        assert_eq!(uuid, "tt-uuid");
        assert_eq!(session_id, "sess-tt");
        // Roundtrip: re-serialize and confirm the wire-shape JSON matches.
        let encoded = serde_json::to_value(&Message::ThinkingTokens {
            estimated_tokens: 1234,
            estimated_tokens_delta: 56,
            uuid: "tt-uuid".to_owned(),
            session_id: "sess-tt".to_owned(),
        })
        .expect("encode");
        assert_eq!(encoded, raw);
    }

    #[test]
    fn turn_duration_decodes_from_wire_shape() {
        // Wire shape: `durationMs` camelCase, plus optional
        // `message_count` and `parent_tool_use_id`. Variant field
        // `ms: u64` maps via #[serde(rename = "durationMs")].
        let raw = json!({
            "type": "system",
            "subtype": "turn_duration",
            "durationMs": 31051,
            "messageCount": 29,
            "parent_tool_use_id": "uuid_25",
            "session_id": "sess-td",
            "uuid": "td-uuid",
        });
        let msg: Message = serde_json::from_value(raw).expect("decode");
        let Message::TurnDuration { ms, message_count, parent_tool_use_id, session_id, uuid } = msg
        else {
            panic!("expected TurnDuration, got {msg:?}");
        };
        assert_eq!(ms, 31051);
        assert_eq!(message_count, Some(29));
        assert_eq!(parent_tool_use_id.as_deref(), Some("uuid_25"));
        assert_eq!(session_id, "sess-td");
        assert_eq!(uuid, "td-uuid");
    }

    #[test]
    fn turn_duration_roundtrip_emits_wire_camel_case_keys() {
        let msg = Message::TurnDuration {
            ms: 1500,
            message_count: Some(3),
            parent_tool_use_id: None,
            session_id: "sess-rt".to_owned(),
            uuid: "rt-uuid".to_owned(),
        };
        let encoded = serde_json::to_value(&msg).expect("encode");
        // durationMs / messageCount preserved.
        assert_eq!(encoded.get("durationMs").and_then(serde_json::Value::as_u64), Some(1500));
        assert_eq!(encoded.get("messageCount").and_then(serde_json::Value::as_u64), Some(3));
        // parent_tool_use_id: None skipped from the output entirely.
        assert!(
            encoded.get("parent_tool_use_id").is_none(),
            "None-valued parent_tool_use_id must NOT serialize",
        );
    }

    #[test]
    fn stop_hook_summary_decodes_from_wire_shape() {
        // Wire shape (verbatim from `baselines/sdk/2.1.156/...`):
        // rich object with `hookCount` + `hookInfos` array + optional
        // `parent_tool_use_id`. The renderer reads `hookCount`
        // (mapped to `actions: u32`) for the collapsed 1-liner and
        // `hookInfos` for the expanded body. No top-level `summary`
        // string in the wire; the EPIC's nominal `summary: Option<String>`
        // is preserved as an optional defensive field for future shapes
        // that might add one.
        let raw = json!({
            "type": "system",
            "subtype": "stop_hook_summary",
            "hasOutput": true,
            "hookCount": 2,
            "hookErrors": [],
            "hookInfos": [
                {"command": "bash ~/.claude/hooks/cmux-notify.sh", "durationMs": 980},
                {"command": "${CLAUDE_PLUGIN_ROOT}/hooks/stop-hook.sh", "durationMs": 17},
            ],
            "level": "suggestion",
            "parent_tool_use_id": "uuid_2",
            "preventedContinuation": false,
            "session_id": "session_0",
            "stopReason": "",
            "toolUseID": "5e586a7f",
            "uuid": "uuid_3",
        });
        let msg: Message = serde_json::from_value(raw).expect("decode");
        let Message::StopHookSummary {
            actions, hook_infos, parent_tool_use_id, session_id, ..
        } = msg
        else {
            panic!("expected StopHookSummary, got {msg:?}");
        };
        assert_eq!(actions, 2, "hookCount -> actions");
        assert_eq!(hook_infos.len(), 2);
        assert_eq!(hook_infos[0].command, "bash ~/.claude/hooks/cmux-notify.sh");
        assert_eq!(hook_infos[0].duration_ms, 980);
        assert_eq!(parent_tool_use_id.as_deref(), Some("uuid_2"));
        assert_eq!(session_id, "session_0");
    }

    #[test]
    fn workflow_task_progress_decodes_workflow_progress_array() {
        // Wire shape from `~/Projects/forge/.claude/skills/claude-cli-upgrade/reference-captures/workflow.jsonl`.
        // Each `system/task_progress` for a Workflow tool carries a
        // FULL `workflow_progress` snapshot - phase markers + the
        // currently active agent's state.
        let raw = json!({
            "type": "system",
            "subtype": "task_progress",
            "task_id": "woc6i1sab",
            "tool_use_id": "toolu_01XapnWmqm6an1tJYxJn72xs",
            "description": "Ping: ping",
            "usage": {"total_tokens": 54707, "tool_uses": 1, "duration_ms": 4826},
            "last_tool_name": "ping",
            "summary": "Minimal one-agent workflow: ask for a fixed structured fact",
            "workflow_progress": [
                {"type": "workflow_phase", "index": 1, "title": "Ping"},
                {
                    "type": "workflow_agent",
                    "index": 1,
                    "label": "ping",
                    "phaseIndex": 1,
                    "phaseTitle": "Ping",
                    "state": "done",
                    "lastToolName": "StructuredOutput",
                    "lastToolSummary": "pong",
                    "resultPreview": "{\"answer\":\"pong\",\"confidence\":1}",
                    "agentId": "abf",
                    "model": "claude-opus-4-7",
                    "queuedAt": 1_780_047_148_162_u64,
                    "startedAt": 1_780_047_148_173_u64,
                    "attempt": 1,
                    "promptPreview": "Reply with the single word pong.",
                    "lastProgressAt": 1_780_047_153_041_u64,
                    "tokens": 54707,
                    "toolCalls": 1,
                    "durationMs": 4868,
                },
            ],
            "uuid": "8f030768-6fda-4ea9-a122-9beb16e8d6e3",
            "session_id": "session_z",
        });
        let msg: Message = serde_json::from_value(raw).expect("decode");
        let Message::TaskProgress { workflow_progress, task_id, tool_use_id, .. } = msg else {
            panic!("expected TaskProgress");
        };
        assert_eq!(task_id, "woc6i1sab");
        assert_eq!(tool_use_id.as_deref(), Some("toolu_01XapnWmqm6an1tJYxJn72xs"));
        assert_eq!(workflow_progress.len(), 2);
        let WorkflowProgressEvent::WorkflowPhase { index, title } = &workflow_progress[0] else {
            panic!("first event must be WorkflowPhase, got {:?}", workflow_progress[0]);
        };
        assert_eq!(*index, 1);
        assert_eq!(title, "Ping");
        let WorkflowProgressEvent::WorkflowAgent {
            phase_index,
            phase_title,
            state,
            last_tool_name,
            last_tool_summary,
            result_preview,
            ..
        } = &workflow_progress[1]
        else {
            panic!("second event must be WorkflowAgent, got {:?}", workflow_progress[1]);
        };
        assert_eq!(*phase_index, 1);
        assert_eq!(phase_title, "Ping");
        assert_eq!(state, "done");
        assert_eq!(last_tool_name.as_deref(), Some("StructuredOutput"));
        assert_eq!(last_tool_summary.as_deref(), Some("pong"));
        assert_eq!(result_preview.as_deref(), Some("{\"answer\":\"pong\",\"confidence\":1}"));
    }

    #[test]
    fn workflow_task_progress_without_workflow_progress_decodes_with_empty_default() {
        // Non-Workflow task_progress events omit the `workflow_progress`
        // field; serde default fills with empty vec so the existing
        // sub-agent task_progress handler stays untouched.
        let raw = json!({
            "type": "system",
            "subtype": "task_progress",
            "task_id": "t-sub",
            "description": "subagent halfway",
            "usage": {"total_tokens": 10, "tool_uses": 1, "duration_ms": 100},
            "uuid": "u-sub",
            "session_id": "sess",
        });
        let msg: Message = serde_json::from_value(raw).expect("decode");
        let Message::TaskProgress { workflow_progress, .. } = msg else {
            panic!("expected TaskProgress");
        };
        assert!(workflow_progress.is_empty());
    }

    #[test]
    fn workflow_progress_unknown_type_decodes_as_other() {
        // Future CLI may add new workflow event types. Confirm they
        // decode as the `Other` variant rather than failing the
        // surrounding `task_progress` decode.
        let raw = json!({"type": "workflow_future", "anything": 42});
        let event: WorkflowProgressEvent = serde_json::from_value(raw).expect("decode");
        assert!(matches!(event, WorkflowProgressEvent::Other));
    }

    #[test]
    fn stop_hook_summary_with_zero_hooks_decodes_cleanly() {
        // The renderer hides the surface when `actions == 0`. Confirm
        // the decoder still produces a clean variant with empty
        // `hook_infos`.
        let raw = json!({
            "type": "system",
            "subtype": "stop_hook_summary",
            "hasOutput": false,
            "hookCount": 0,
            "hookErrors": [],
            "hookInfos": [],
            "level": "suggestion",
            "preventedContinuation": false,
            "session_id": "session_z",
            "stopReason": "",
            "toolUseID": "tu-z",
            "uuid": "uuid_z",
        });
        let msg: Message = serde_json::from_value(raw).expect("decode");
        let Message::StopHookSummary { actions, hook_infos, .. } = msg else {
            panic!("expected StopHookSummary");
        };
        assert_eq!(actions, 0);
        assert!(hook_infos.is_empty());
    }
}
