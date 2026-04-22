//! Top-level stream-json message shapes.
//!
//! Every line the `claude --output-format stream-json` binary emits is one
//! of these variants. Mirrors Python SDK's `AssistantMessage`, `UserMessage`,
//! `SystemMessage` (plus task-lifecycle + mirror-error subclasses),
//! `ResultMessage`, and `RateLimitEvent`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::content::ContentBlock;
use crate::session_store::SessionKey;

/// One stream-json message.
///
/// Wire-level dispatch on `type` and, for `type="system"`, on `subtype` is
/// handled by a private shim — users never see it. Every variant here is
/// the user-facing shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(from = "MessageRepr", into = "MessageRepr")]
pub enum Message {
    /// An assistant turn (may be a partial chunk during streaming).
    Assistant {
        /// The nested Anthropic-API-shaped message envelope.
        message: AssistantEnvelope,
        /// Session id this turn belongs to.
        session_id: String,
        /// Parent tool-use id when this turn is a sub-agent spawned via `Task`.
        parent_tool_use_id: Option<String>,
    },

    /// A user turn — user prompts or tool-result envelopes.
    User {
        /// The nested user-message envelope.
        message: UserEnvelope,
        /// Session id this turn belongs to.
        session_id: String,
        /// Parent tool-use id when this is a sub-agent turn.
        parent_tool_use_id: Option<String>,
    },

    /// Out-of-band system event — `subtype` discriminates (e.g. `"init"`).
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
    /// Mirrors Python SDK v0.1.64 `TaskStartedMessage` (`types.py:951-965`).
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

    /// Periodic progress update while a sub-agent `Task` is in flight.
    /// Subtype `"task_progress"`. Mirrors Python SDK v0.1.64
    /// `TaskProgressMessage` (`types.py:967-983`).
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
    },

    /// Terminal notification when a sub-agent `Task` completes, fails, or is
    /// stopped. Subtype `"task_notification"`. Mirrors Python SDK v0.1.64
    /// `TaskNotificationMessage` (`types.py:986-1002`).
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

    /// Synthesised system frame reporting a failed
    /// [`SessionStore::append`](crate::session_store::SessionStore::append)
    /// call inside the transcript-mirror batcher. Subtype `"mirror_error"`.
    /// Non-fatal: the on-disk transcript is already durable, but the
    /// mirrored copy in the external store is missing the failed batch.
    /// Mirrors Python SDK v0.1.64 `MirrorErrorMessage` (`types.py:1005-1019`).
    MirrorError {
        /// Session key whose append failed, if known at error time.
        key: Option<SessionKey>,
        /// Human-readable reason for the failure.
        error: String,
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

    /// Streaming partial-message event emitted when
    /// [`Options::include_partial_messages`](crate::Options) is set. The
    /// CLI forwards raw Anthropic-API stream events (`message_start`,
    /// `content_block_delta`, `message_delta`, `message_stop`) without
    /// coalescing them into complete turns. Mirrors Python SDK's
    /// `StreamEvent` (`types.py:1043-1050`,
    /// `_internal/message_parser.py:229-240`).
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
    /// CLI's read loop fails. Python SDK emits this at
    /// `_internal/query.py:315` as a last-gasp signal before teardown;
    /// forge-sdk preserves the same shape so callers draining
    /// [`Client::next_event`](crate::Client::next_event) see the failure
    /// on the iterator rather than via a side channel.
    Error {
        /// The failure message as Python stringified it.
        error: String,
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

/// Usage counters reported inside task-progress and task-notification frames.
///
/// Mirrors Python SDK v0.1.64 `TaskUsage` (`types.py:939-944`).
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
/// [`Message::TaskNotification`]. Mirrors Python's
/// `Literal["completed", "failed", "stopped"]` (`types.py:948`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskNotificationStatus {
    /// Task finished successfully.
    Completed,
    /// Task exited with an error.
    Failed,
    /// Task was cancelled before it could finish.
    Stopped,
}

// ---------------------------------------------------------------------------
// Wire shim — serde sees this, users never do.
//
// `Message` has the user-facing variant layout. `MessageRepr` encodes the
// actual wire dispatch: first on `type`, then (for `type="system"`) on
// `subtype`. The cascade works because:
//
// * `MessageRepr` is internally-tagged on `type` — serde picks `System(repr)`
//   when `type="system"`, the rest via tag rename.
// * `SystemRepr` is untagged — serde tries `Typed(TypedSystemRepr)` first
//   (which is itself internally-tagged on `subtype`), then falls back to
//   `Generic(GenericSystemRepr)` for subtypes we don't recognise.
// * `TypedSystemRepr` dispatches the known task-lifecycle subtypes.
// * `GenericSystemRepr` captures any other subtype into the opaque
//   `data: Value`, which is what `Message::System` surfaces to users.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum MessageRepr {
    Assistant {
        message: AssistantEnvelope,
        session_id: String,
        #[serde(default)]
        parent_tool_use_id: Option<String>,
    },
    User {
        message: UserEnvelope,
        session_id: String,
        #[serde(default)]
        parent_tool_use_id: Option<String>,
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
        total_cost_usd: f64,
        usage: Usage,
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
    MirrorError {
        #[serde(default)]
        key: Option<SessionKey>,
        #[serde(default)]
        error: String,
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
    #[allow(clippy::too_many_lines)]
    fn from(repr: MessageRepr) -> Self {
        match repr {
            MessageRepr::Assistant {
                message,
                session_id,
                parent_tool_use_id,
            } => Message::Assistant {
                message,
                session_id,
                parent_tool_use_id,
            },
            MessageRepr::User {
                message,
                session_id,
                parent_tool_use_id,
            } => Message::User {
                message,
                session_id,
                parent_tool_use_id,
            },
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
            MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::TaskProgress {
                task_id,
                description,
                usage,
                uuid,
                session_id,
                tool_use_id,
                last_tool_name,
            })) => Message::TaskProgress {
                task_id,
                description,
                usage,
                uuid,
                session_id,
                tool_use_id,
                last_tool_name,
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
            MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::MirrorError { key, error })) => {
                Message::MirrorError { key, error }
            }
            MessageRepr::System(SystemRepr::Generic(GenericSystemRepr {
                subtype,
                session_id,
                data,
            })) => Message::System {
                subtype,
                session_id,
                data,
            },
            MessageRepr::RateLimitEvent {
                rate_limit_info,
                uuid,
                session_id,
            } => Message::RateLimitEvent {
                rate_limit_info,
                uuid,
                session_id,
            },
            MessageRepr::Result {
                subtype,
                session_id,
                is_error,
                num_turns,
                duration_ms,
                duration_api_ms,
                total_cost_usd,
                usage,
            } => Message::Result {
                subtype,
                session_id,
                is_error,
                num_turns,
                duration_ms,
                duration_api_ms,
                total_cost_usd,
                usage,
            },
            MessageRepr::StreamEvent {
                uuid,
                session_id,
                event,
                parent_tool_use_id,
            } => Message::StreamEvent {
                uuid,
                session_id,
                event,
                parent_tool_use_id,
            },
            MessageRepr::Error { error } => Message::Error { error },
        }
    }
}

impl From<Message> for MessageRepr {
    #[allow(clippy::too_many_lines)]
    fn from(msg: Message) -> Self {
        match msg {
            Message::Assistant {
                message,
                session_id,
                parent_tool_use_id,
            } => MessageRepr::Assistant {
                message,
                session_id,
                parent_tool_use_id,
            },
            Message::User {
                message,
                session_id,
                parent_tool_use_id,
            } => MessageRepr::User {
                message,
                session_id,
                parent_tool_use_id,
            },
            Message::System {
                subtype,
                session_id,
                data,
            } => MessageRepr::System(SystemRepr::Generic(GenericSystemRepr {
                subtype,
                session_id,
                data,
            })),
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
            Message::TaskProgress {
                task_id,
                description,
                usage,
                uuid,
                session_id,
                tool_use_id,
                last_tool_name,
            } => MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::TaskProgress {
                task_id,
                description,
                usage,
                uuid,
                session_id,
                tool_use_id,
                last_tool_name,
            })),
            Message::MirrorError { key, error } => {
                MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::MirrorError {
                    key,
                    error,
                }))
            }
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
            Message::RateLimitEvent {
                rate_limit_info,
                uuid,
                session_id,
            } => MessageRepr::RateLimitEvent {
                rate_limit_info,
                uuid,
                session_id,
            },
            Message::Result {
                subtype,
                session_id,
                is_error,
                num_turns,
                duration_ms,
                duration_api_ms,
                total_cost_usd,
                usage,
            } => MessageRepr::Result {
                subtype,
                session_id,
                is_error,
                num_turns,
                duration_ms,
                duration_api_ms,
                total_cost_usd,
                usage,
            },
            Message::StreamEvent {
                uuid,
                session_id,
                event,
                parent_tool_use_id,
            } => MessageRepr::StreamEvent {
                uuid,
                session_id,
                event,
                parent_tool_use_id,
            },
            Message::Error { error } => MessageRepr::Error { error },
        }
    }
}
