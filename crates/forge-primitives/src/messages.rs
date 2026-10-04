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
use crate::runtime::TerminalReason;

/// Wire fields forge does not model, kept verbatim so a decode is lossless:
/// what the CLI sent crosses to whoever draws the raw frame even when
/// nothing here reads it. Reachable through `Message`'s variants and the
/// values they nest.
pub type Extras = serde_json::Map<String, Value>;

/// Whether a `parent_tool_use_id` names the dispatch a frame ran under. Set
/// and non-empty: the wire spells "no dispatch" as null, and every fold that
/// reads the field guards on the same thing.
pub fn names_a_dispatch(parent_tool_use_id: Option<&str>) -> bool {
    parent_tool_use_id.is_some_and(|parent| !parent.trim().is_empty())
}

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
        /// Stable identifier for this assistant turn - the CLI
        /// `AssistantMessage.uuid`.
        uuid: Option<String>,
        /// When the CLI wrote this frame, RFC 3339. The only record of when
        /// a turn ran that a transcript holds: no result frame reaches one.
        timestamp: Option<String>,
        /// Fields of the frame forge does not model, kept verbatim so a
        /// decode is lossless for whatever draws the raw frame.
        extras: Extras,
    },

    /// A user turn - user prompts or tool-result envelopes.
    User {
        /// The nested user-message envelope.
        message: UserEnvelope,
        /// Session id this turn belongs to.
        session_id: String,
        /// Parent tool-use id when this is a sub-agent turn.
        parent_tool_use_id: Option<String>,
        /// Stable identifier for this user turn - the CLI
        /// `UserMessage.uuid`. `None` unless the CLI is configured to
        /// emit them (`extra_args={"replay-user-messages": None}`).
        uuid: Option<String>,
        /// Raw tool-result payload the CLI attaches when this user turn
        /// reports a tool's output. The CLI `UserMessage.tool_use_result`;
        /// forge-sdk passes it through as a
        /// [`Value`] since the upstream type is `dict[str, Any]`.
        tool_use_result: Option<Value>,
        /// When the CLI wrote this frame, RFC 3339. The only record of when
        /// a turn ran that a transcript holds: no result frame reaches one.
        timestamp: Option<String>,
        /// The CLI's own mark that nobody typed this turn, folded from every
        /// spelling the CLI gives it: the wire's `isSynthetic` (itself the
        /// union of `isMeta`, `isVisibleInTranscriptOnly` and
        /// `isCompactSummary`), and the transcript row's `isMeta`,
        /// `isCompactSummary`, `isVisibleInTranscriptOnly` or
        /// `turnCompanion`. A view that reads it draws the harness speaking
        /// rather than the reader.
        synthetic: bool,
        /// Fields of the frame forge does not model, kept verbatim so a
        /// decode is lossless for whatever draws the raw frame.
        extras: Extras,
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
        /// Fields of the frame forge does not model, kept verbatim so a
        /// decode is lossless for whatever draws the raw frame.
        extras: Extras,
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
        /// Fields of the frame forge does not model, kept verbatim so a
        /// decode is lossless for whatever draws the raw frame.
        extras: Extras,
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
        /// Fields of the frame forge does not model, kept verbatim so a
        /// decode is lossless for whatever draws the raw frame.
        extras: Extras,
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
        /// Fields of the frame forge does not model, kept verbatim so a
        /// decode is lossless for whatever draws the raw frame.
        extras: Extras,
    },

    /// The CLI's heartbeat for a long-running tool call, emitted every
    /// 30 seconds while the call is in flight. Top-level `tool_progress`.
    ///
    /// `tool_use_id` appends `-heartbeat-<n>` to the running call's own
    /// id, and `parent_tool_use_id` names that call. An informational
    /// frame the SDK used to drop at the reader; it crosses like any
    /// other so whoever draws the raw stream can surface liveness on the
    /// call it belongs to.
    ToolProgress {
        /// The running call's id with the CLI's `-heartbeat-<n>` suffix.
        tool_use_id: String,
        /// Name of the tool in flight (e.g. `"Bash"`).
        tool_name: String,
        /// Seconds since the tool call started.
        elapsed_time_seconds: f64,
        /// True on the 30-second cadence heartbeats.
        heartbeat: bool,
        /// The tool call this heartbeat belongs to, when it is one the
        /// CLI parent-stamped (a sub-agent's own call).
        parent_tool_use_id: Option<String>,
        /// Session id, when the frame carries one.
        session_id: Option<String>,
        /// The frame's own id, when the CLI attaches one.
        uuid: Option<String>,
        /// Fields of the frame forge does not model, kept verbatim so a
        /// decode is lossless for whatever draws the raw frame.
        extras: Extras,
    },

    /// CLI-side estimated thinking-token count, fires repeatedly
    /// during model thinking. Subtype `"thinking_tokens"` (#273).
    ///
    /// Both counters are scoped to one **thinking block**, not to the
    /// turn: the CLI restarts them at each agentic iteration, so a
    /// turn with two thinking blocks emits a second run beginning at
    /// 50 again. A per-turn total is therefore the sum of the deltas,
    /// which is what the turn info row renders.
    ThinkingTokens {
        /// Estimated reasoning tokens so far in the current thinking
        /// block. Restarts at each block within a turn, so this is not
        /// a turn total and reading it as one understates a multi-block
        /// turn.
        estimated_tokens: u64,
        /// Growth since the previous event in the same thinking block,
        /// and equal to `estimated_tokens` on a block's first event
        /// rather than stepping backwards. The CLI emits roughly every
        /// 50 tokens.
        estimated_tokens_delta: i64,
        /// Unique identifier for this thinking-tokens event.
        uuid: String,
        /// Session id the event applies to.
        session_id: String,
        /// Fields of the frame forge does not model, kept verbatim so a
        /// decode is lossless for whatever draws the raw frame.
        extras: Extras,
    },

    /// Assistant-turn wall-clock + message-count summary emitted at
    /// the end of each turn. Subtype `"turn_duration"` (#273); decoded
    /// for wire conformance, and no renderer composes a chip from `ms`.
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
        /// Fields of the frame forge does not model, kept verbatim so a
        /// decode is lossless for whatever draws the raw frame.
        extras: Extras,
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
        /// Errors the hook batch reported, empty when nothing failed.
        /// Wire field is `hookErrors`; the client's hooks chip draws
        /// these against the batch rather than against one entry.
        hook_errors: Vec<String>,
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
        /// Fields of the frame forge does not model, kept verbatim so a
        /// decode is lossless for whatever draws the raw frame.
        extras: Extras,
    },

    /// Full background-task set after it changed. Subtype
    /// `"background_tasks_changed"` (2.1.204); carries the whole list,
    /// not a delta.
    BackgroundTasksChanged {
        /// Every background task after the change; each entry carries
        /// `task_id` / `task_type` / `description`.
        tasks: Vec<Value>,
        /// Unique identifier for this event.
        uuid: String,
        /// Session id the event applies to.
        session_id: String,
        /// Fields of the frame forge does not model, kept verbatim so a
        /// decode is lossless for whatever draws the raw frame.
        extras: Extras,
    },

    /// Available slash-command set after it changed. Subtype
    /// `"commands_changed"` (2.1.204), emitted on plugin reload.
    CommandsChanged {
        /// Every slash command after the change.
        commands: Vec<Value>,
        /// Unique identifier for this event.
        uuid: String,
        /// Session id the event applies to.
        session_id: String,
        /// Fields of the frame forge does not model, kept verbatim so a
        /// decode is lossless for whatever draws the raw frame.
        extras: Extras,
    },

    /// A hook began executing. Subtype `"hook_started"` (2.1.204).
    HookStarted {
        /// Stable id for this hook run, paired with [`Self::HookResponse`].
        hook_id: String,
        /// Hook matcher name (e.g. `"SessionStart:startup"`).
        hook_name: String,
        /// Hook event that fired it (e.g. `"SessionStart"`).
        hook_event: String,
        /// Unique identifier for this event.
        uuid: String,
        /// Session id the event applies to.
        session_id: String,
        /// Fields of the frame forge does not model, kept verbatim so a
        /// decode is lossless for whatever draws the raw frame.
        extras: Extras,
    },

    /// A hook finished executing. Subtype `"hook_response"` (2.1.204).
    HookResponse {
        /// Stable id tying this back to its [`Self::HookStarted`].
        hook_id: String,
        /// Hook matcher name (e.g. `"SessionStart:startup"`).
        hook_name: String,
        /// Hook event that fired it (e.g. `"SessionStart"`).
        hook_event: String,
        /// Outcome tag reported by the CLI (e.g. `"success"`).
        outcome: String,
        /// Process exit code of the hook command.
        exit_code: i64,
        /// Combined output surfaced to the session.
        output: String,
        /// Raw stdout of the hook command.
        stdout: String,
        /// Raw stderr of the hook command.
        stderr: String,
        /// Unique identifier for this event.
        uuid: String,
        /// Session id the event applies to.
        session_id: String,
        /// Fields of the frame forge does not model, kept verbatim so a
        /// decode is lossless for whatever draws the raw frame.
        extras: Extras,
    },

    /// A long-running hook emitted interim output. Subtype
    /// `"hook_progress"` (2.1.263).
    HookProgress {
        /// Stable id for this hook run, paired with [`Self::HookResponse`].
        hook_id: String,
        /// Hook matcher name (e.g. `"SessionStart:startup"`).
        hook_name: String,
        /// Hook event that fired it (e.g. `"SessionStart"`).
        hook_event: String,
        /// Raw stdout of the hook command so far.
        stdout: String,
        /// Raw stderr of the hook command so far.
        stderr: String,
        /// Combined output surfaced to the session so far.
        output: String,
        /// Unique identifier for this event.
        uuid: String,
        /// Session id the event applies to.
        session_id: String,
        /// Fields of the frame forge does not model, kept verbatim so a
        /// decode is lossless for whatever draws the raw frame.
        extras: Extras,
    },

    /// The CLI surfaced a notification to the SDK host. Subtype
    /// `"notification"` (2.1.263).
    Notification {
        /// Notification kind (e.g. `"stop-hook-error"`).
        key: Option<String>,
        /// Human-readable notification body.
        text: String,
        /// Delivery priority tag (e.g. `"immediate"`).
        priority: Option<String>,
        /// Unique identifier for this event.
        uuid: String,
        /// Session id the event applies to.
        session_id: String,
        /// Fields of the frame forge does not model, kept verbatim so a
        /// decode is lossless for whatever draws the raw frame.
        extras: Extras,
    },

    /// The CLI refused a tool call on permission grounds. Subtype
    /// `"permission_denied"` (first observed on 2.1.280).
    PermissionDenied {
        /// Name of the tool the CLI refused (e.g. `"Bash"`).
        tool_name: String,
        /// Id of the `tool_use` block that was refused.
        tool_use_id: String,
        /// Machine-readable reason category (e.g. `"other"`).
        decision_reason_type: String,
        /// Reason token the CLI recorded (e.g. `"Contains simple_expansion"`).
        decision_reason: String,
        /// Text of the refusal shown to the session.
        message: String,
        /// Unique identifier for this event.
        uuid: String,
        /// Session id the event applies to.
        session_id: String,
        /// Fields of the frame forge does not model, kept verbatim so a
        /// decode is lossless for whatever draws the raw frame.
        extras: Extras,
    },

    /// A compaction finished and the transcript was replaced. Subtype
    /// `"compact_boundary"`.
    ///
    /// The wire nests the counts and the trigger under `compact_metadata`,
    /// next to preserved-message uuids nothing reads. Only what forge
    /// reacts to is modelled: every field modelled here is one a later
    /// CLI can drop the whole frame to the generic bucket over.
    CompactBoundary {
        /// What started the compaction: `"manual"` for `/compact`,
        /// `"auto"` when the context window forced it.
        trigger: String,
        /// Context tokens in use immediately before the compaction.
        pre_tokens: u64,
        /// Context tokens the session carried after the cut.
        post_tokens: u64,
        /// Unique identifier for this event.
        uuid: String,
        /// Session id the event applies to.
        session_id: String,
        /// Fields of the `compact_metadata` object forge does not model
        /// (`duration_ms`, `preserved_segment` and friends), kept verbatim
        /// under their own object on the way back out.
        metadata_extras: Extras,
        /// Fields of the frame forge does not model, kept verbatim so a
        /// decode is lossless for whatever draws the raw frame.
        extras: Extras,
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
        /// Fields of the frame forge does not model, kept verbatim so a
        /// decode is lossless for whatever draws the raw frame.
        extras: Extras,
    },

    /// The CLI's own lifecycle for one prompt, keyed by the uuid forge
    /// stamped on the frame that carried it. Top-level `command_lifecycle`.
    ///
    /// Emitted only for prompts that carried a `uuid` when written (measured:
    /// zero frames without one), and it is what lets a view say queued versus
    /// taken: `queued` lands 1-5 ms after the write, `started` 2-3 ms after
    /// the turn boundary that delivers the prompt.
    ///
    /// `state` stays a free-form string so a state this build has not seen
    /// decodes without a primitives bump; the known set is `queued`,
    /// `started`, `completed`, `cancelled`, `discarded`, `refused`.
    CommandLifecycle {
        /// The uuid the prompt was sent under - the reader's own, or the
        /// one forge minted for it.
        command_uuid: String,
        /// Where the prompt is in the CLI's queue.
        state: String,
        /// Unique identifier for this lifecycle event (the CLI's own).
        uuid: String,
        /// Session id the prompt belongs to.
        session_id: String,
        /// Fields of the frame forge does not model, kept verbatim so a
        /// decode is lossless for whatever draws the raw frame.
        extras: Extras,
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
        /// Fields of the frame forge does not model (`subagent_stats` for
        /// one), kept verbatim so a decode is lossless.
        extras: Extras,
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
        /// Fields of the frame forge does not model, kept verbatim so a
        /// decode is lossless for whatever draws the raw frame.
        extras: Extras,
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
        /// Fields of the frame forge does not model, kept verbatim so a
        /// decode is lossless for whatever draws the raw frame.
        extras: Extras,
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
            | Message::BackgroundTasksChanged { session_id, .. }
            | Message::CommandsChanged { session_id, .. }
            | Message::HookStarted { session_id, .. }
            | Message::HookProgress { session_id, .. }
            | Message::PermissionDenied { session_id, .. }
            | Message::HookResponse { session_id, .. }
            | Message::Notification { session_id, .. }
            | Message::CompactBoundary { session_id, .. }
            | Message::Result { session_id, .. }
            | Message::CommandLifecycle { session_id, .. }
            | Message::StreamEvent { session_id, .. } => Some(session_id.as_str()),
            Message::System { session_id, .. } | Message::ToolProgress { session_id, .. } => {
                session_id.as_deref()
            }
            Message::RateLimitEvent { .. } | Message::Error { .. } | Message::Unknown { .. } => {
                None
            }
        }
    }

    /// A user turn forged rather than read off the wire, for prose the model
    /// received and the CLI does not echo back.
    ///
    /// **The id is the prompt's own**, the one its `command_lifecycle` frames
    /// carry: it is what lets a view hold this row while the prompt waits in
    /// the queue, and pair it with the page's later copy of the same message -
    /// which carries the same id, the CLI writing the client's uuid into the
    /// transcript it delivers. Nothing routes on the empty `session_id`.
    pub fn display_only_user(text: String, uuid: String) -> Self {
        Message::User {
            message: UserEnvelope {
                role: "user".to_owned(),
                content: vec![ContentBlock::Text { text, extras: Extras::new() }],
                extras: Extras::new(),
            },
            extras: Extras::new(),
            session_id: String::new(),
            parent_tool_use_id: None,
            uuid: Some(uuid),
            tool_use_result: None,
            timestamp: None,
            synthetic: false,
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
    /// Fields of the envelope forge does not model (`provider`, a
    /// `container`, context management diagnostics), kept verbatim.
    #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extras: Extras,
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
    /// Fields of the envelope forge does not model, kept verbatim.
    #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extras: Extras,
}

fn deserialize_user_content<'de, D>(de: D) -> Result<Vec<ContentBlock>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;
    let value = Value::deserialize(de)?;
    match value {
        Value::String(s) => Ok(vec![ContentBlock::Text { text: s, extras: Extras::new() }]),
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Input tokens this turn.
    #[serde(default, deserialize_with = "null_as_zero")]
    pub input_tokens: u64,
    /// Output tokens this turn.
    #[serde(default, deserialize_with = "null_as_zero")]
    pub output_tokens: u64,
    /// Tokens written to the prompt cache this turn.
    #[serde(default, deserialize_with = "null_as_zero")]
    pub cache_creation_input_tokens: u64,
    /// Tokens read from the prompt cache this turn.
    #[serde(default, deserialize_with = "null_as_zero")]
    pub cache_read_input_tokens: u64,
    /// Fields of the usage block forge does not count (`service_tier`,
    /// `inference_geo`, the thinking-token detail), kept verbatim.
    #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extras: Extras,
}

/// Read a counter that may arrive as an explicit `null`.
///
/// `serde(default)` covers an absent key and not a present-but-null
/// one, so a backend that reports "no caching happened" as `null`
/// rather than `0` fails the whole frame - and a frame that fails to
/// decode ends the session's event stream, not just that message.
/// Measured: OpenRouter sends `"cache_creation_input_tokens": null`
/// for a model with no prompt cache.
fn null_as_zero<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<u64>::deserialize(deserializer)?.unwrap_or(0))
}

/// Per-hook entry inside a `stop_hook_summary` system event (#273).
/// Wire fields are camelCase (`durationMs`); the renderer reads
/// the `command` text for the expanded body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopHookInfo {
    /// Command line that fired.
    pub command: String,
    /// How long the hook took, in milliseconds. Wire field is
    /// `durationMs`; absent on 2.1.263's plugin-injected entries.
    #[serde(rename = "durationMs", default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Fields of the entry forge does not model, kept verbatim.
    #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extras: Extras,
}

/// Usage counters reported inside task-progress and task-notification frames.
///
/// `TaskUsage`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskUsage {
    /// Tokens consumed across all model calls in this task so far.
    pub total_tokens: u64,
    /// Number of tool invocations the sub-agent has made.
    pub tool_uses: u64,
    /// Wall-clock time the task has spent running, in milliseconds.
    pub duration_ms: u64,
    /// Fields of the usage block forge does not count, kept verbatim.
    #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extras: Extras,
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
    /// Fields of the patch forge does not model, kept verbatim.
    #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extras: Extras,
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
/// bump - and an unrecognised TYPE is the one loss stated here: the
/// `Other` variant is a unit, so that event's payload does not
/// cross.
///
/// Every modelled field a view does not read stays in `extras`, so
/// `queuedAt`, `startedAt`, `lastProgressAt`, `attempt`, `tokens`,
/// `toolCalls`, `durationMs`, `model`, `agentId` and `promptPreview`
/// all cross with the entry they arrived in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowProgressEvent {
    /// Phase-level marker - emitted when the workflow's `phase()`
    /// call fires.
    WorkflowPhase {
        index: u32,
        title: String,
        #[serde(flatten)]
        extras: Extras,
    },
    /// Agent-level event - emitted on each state transition for an
    /// agent call inside a phase. The modelled fields are the ones a
    /// view reads; everything else the CLI sends crosses in `extras`.
    WorkflowAgent {
        index: u32,
        label: String,
        /// Phase the agent belongs to. The fields became optional:
        /// phase-less agent entries were observed outside the pinned
        /// corpus (proxy-routed capture), while the committed corpus
        /// still tags every entry. Phase grouping then relies on
        /// `workflow_phase` markers alone.
        #[serde(rename = "phaseIndex", default, skip_serializing_if = "Option::is_none")]
        phase_index: Option<u32>,
        #[serde(rename = "phaseTitle", default, skip_serializing_if = "Option::is_none")]
        phase_title: Option<String>,
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
        #[serde(flatten)]
        extras: Extras,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timestamp: Option<String>,
        #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
        extras: Extras,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timestamp: Option<String>,
        // The predicate is `not`, so only a set stamp is written: an unmarked
        // frame keeps the shape every existing fixture and relayed payload was
        // written against.
        #[serde(default, rename = "isSynthetic", skip_serializing_if = "std::ops::Not::not")]
        synthetic: bool,
        #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
        extras: Extras,
    },
    System(SystemRepr),
    RateLimitEvent {
        rate_limit_info: RateLimitInfo,
        uuid: String,
        session_id: String,
        #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
        extras: Extras,
    },
    CommandLifecycle {
        command_uuid: String,
        state: String,
        uuid: String,
        session_id: String,
        #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
        extras: Extras,
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
        #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
        extras: Extras,
    },
    StreamEvent {
        uuid: String,
        session_id: String,
        event: Value,
        #[serde(default)]
        parent_tool_use_id: Option<String>,
        #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
        extras: Extras,
    },
    /// The CLI's heartbeat for a running tool call. A top-level type of
    /// its own, not a `system` subtype.
    ToolProgress {
        tool_use_id: String,
        tool_name: String,
        elapsed_time_seconds: f64,
        #[serde(default)]
        heartbeat: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_tool_use_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uuid: Option<String>,
        #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
        extras: Extras,
    },
    Error {
        error: String,
        #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
        extras: Extras,
    },
}

/// A transient decode shim: serde tries the typed shape, then the generic
/// one, and the value is moved once into a [`Message`]. Never stored or
/// cloned, so the byte gap between its arms - what `large_enum_variant`
/// measures - is paid nowhere, and boxing the typed arm instead would add
/// an allocation per lifecycle frame.
#[expect(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum SystemRepr {
    Typed(TypedSystemRepr),
    Generic(GenericSystemRepr),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
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
        #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
        extras: Extras,
    },
    TaskUpdated {
        task_id: String,
        #[serde(default)]
        patch: TaskUpdatePatch,
        uuid: String,
        session_id: String,
        #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
        extras: Extras,
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
        #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
        extras: Extras,
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
        #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
        extras: Extras,
    },
    ThinkingTokens {
        estimated_tokens: u64,
        estimated_tokens_delta: i64,
        uuid: String,
        session_id: String,
        #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
        extras: Extras,
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
        #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
        extras: Extras,
    },
    StopHookSummary {
        #[serde(rename = "hookCount")]
        actions: u32,
        #[serde(default, rename = "hookInfos")]
        hook_infos: Vec<StopHookInfo>,
        #[serde(default, rename = "hookErrors")]
        hook_errors: Vec<String>,
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
        #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
        extras: Extras,
    },
    BackgroundTasksChanged {
        tasks: Vec<Value>,
        uuid: String,
        session_id: String,
        #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
        extras: Extras,
    },
    CommandsChanged {
        commands: Vec<Value>,
        uuid: String,
        session_id: String,
        #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
        extras: Extras,
    },
    HookStarted {
        hook_id: String,
        hook_name: String,
        hook_event: String,
        uuid: String,
        session_id: String,
        #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
        extras: Extras,
    },
    HookResponse {
        hook_id: String,
        hook_name: String,
        hook_event: String,
        outcome: String,
        exit_code: i64,
        output: String,
        stdout: String,
        stderr: String,
        uuid: String,
        session_id: String,
        #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
        extras: Extras,
    },
    HookProgress {
        hook_id: String,
        hook_name: String,
        hook_event: String,
        stdout: String,
        stderr: String,
        output: String,
        uuid: String,
        session_id: String,
        #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
        extras: Extras,
    },
    Notification {
        key: Option<String>,
        text: String,
        priority: Option<String>,
        uuid: String,
        session_id: String,
        #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
        extras: Extras,
    },
    PermissionDenied {
        tool_name: String,
        tool_use_id: String,
        decision_reason_type: String,
        decision_reason: String,
        message: String,
        uuid: String,
        session_id: String,
        #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
        extras: Extras,
    },
    CompactBoundary {
        compact_metadata: CompactMetadataRepr,
        uuid: String,
        session_id: String,
        #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
        extras: Extras,
    },
}

/// The subset of `compact_boundary`'s `compact_metadata` forge reads,
/// plus every sibling field it does not: the counts and the preserved
/// segment cross verbatim so a lossless read reaches whoever draws them.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CompactMetadataRepr {
    trigger: String,
    pre_tokens: u64,
    post_tokens: u64,
    #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
    extras: Extras,
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
            MessageRepr::Assistant {
                message,
                session_id,
                parent_tool_use_id,
                error,
                uuid,
                timestamp,
                extras,
            } => Message::Assistant {
                message,
                session_id,
                parent_tool_use_id,
                error,
                uuid,
                timestamp,
                extras,
            },
            MessageRepr::User {
                message,
                session_id,
                parent_tool_use_id,
                uuid,
                tool_use_result,
                timestamp,
                synthetic,
                extras,
            } => Message::User {
                message,
                session_id,
                parent_tool_use_id,
                uuid,
                tool_use_result,
                timestamp,
                synthetic,
                extras,
            },
            MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::TaskStarted {
                task_id,
                description,
                uuid,
                session_id,
                tool_use_id,
                task_type,
                extras,
            })) => Message::TaskStarted {
                task_id,
                description,
                uuid,
                session_id,
                tool_use_id,
                task_type,
                extras,
            },
            MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::TaskUpdated {
                task_id,
                patch,
                uuid,
                session_id,
                extras,
            })) => Message::TaskUpdated { task_id, patch, uuid, session_id, extras },
            MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::TaskProgress {
                task_id,
                description,
                usage,
                uuid,
                session_id,
                tool_use_id,
                last_tool_name,
                workflow_progress,
                extras,
            })) => Message::TaskProgress {
                task_id,
                description,
                usage,
                uuid,
                session_id,
                tool_use_id,
                last_tool_name,
                workflow_progress,
                extras,
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
                extras,
            })) => Message::TaskNotification {
                task_id,
                status,
                output_file,
                summary,
                uuid,
                session_id,
                tool_use_id,
                usage,
                extras,
            },
            MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::ThinkingTokens {
                estimated_tokens,
                estimated_tokens_delta,
                uuid,
                session_id,
                extras,
            })) => Message::ThinkingTokens {
                estimated_tokens,
                estimated_tokens_delta,
                uuid,
                session_id,
                extras,
            },
            MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::TurnDuration {
                ms,
                message_count,
                parent_tool_use_id,
                session_id,
                uuid,
                extras,
            })) => Message::TurnDuration {
                ms,
                message_count,
                parent_tool_use_id,
                session_id,
                uuid,
                extras,
            },
            MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::StopHookSummary {
                actions,
                hook_infos,
                hook_errors,
                has_output,
                level,
                prevented_continuation,
                stop_reason,
                tool_use_id,
                parent_tool_use_id,
                session_id,
                uuid,
                extras,
            })) => Message::StopHookSummary {
                actions,
                hook_infos,
                hook_errors,
                has_output,
                level,
                prevented_continuation,
                stop_reason,
                tool_use_id,
                parent_tool_use_id,
                session_id,
                uuid,
                extras,
            },
            MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::BackgroundTasksChanged {
                tasks,
                uuid,
                session_id,
                extras,
            })) => Message::BackgroundTasksChanged { tasks, uuid, session_id, extras },
            MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::CommandsChanged {
                commands,
                uuid,
                session_id,
                extras,
            })) => Message::CommandsChanged { commands, uuid, session_id, extras },
            MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::HookStarted {
                hook_id,
                hook_name,
                hook_event,
                uuid,
                session_id,
                extras,
            })) => {
                Message::HookStarted { hook_id, hook_name, hook_event, uuid, session_id, extras }
            }
            MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::HookResponse {
                hook_id,
                hook_name,
                hook_event,
                outcome,
                exit_code,
                output,
                stdout,
                stderr,
                uuid,
                session_id,
                extras,
            })) => Message::HookResponse {
                hook_id,
                hook_name,
                hook_event,
                outcome,
                exit_code,
                output,
                stdout,
                stderr,
                uuid,
                session_id,
                extras,
            },
            MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::HookProgress {
                hook_id,
                hook_name,
                hook_event,
                stdout,
                stderr,
                output,
                uuid,
                session_id,
                extras,
            })) => Message::HookProgress {
                hook_id,
                hook_name,
                hook_event,
                stdout,
                stderr,
                output,
                uuid,
                session_id,
                extras,
            },
            MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::Notification {
                key,
                text,
                priority,
                uuid,
                session_id,
                extras,
            })) => Message::Notification { key, text, priority, uuid, session_id, extras },
            MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::PermissionDenied {
                tool_name,
                tool_use_id,
                decision_reason_type,
                decision_reason,
                message,
                uuid,
                session_id,
                extras,
            })) => Message::PermissionDenied {
                tool_name,
                tool_use_id,
                decision_reason_type,
                decision_reason,
                message,
                uuid,
                session_id,
                extras,
            },
            MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::CompactBoundary {
                compact_metadata:
                    CompactMetadataRepr { trigger, pre_tokens, post_tokens, extras: metadata_extras },
                uuid,
                session_id,
                extras,
            })) => Message::CompactBoundary {
                trigger,
                pre_tokens,
                post_tokens,
                uuid,
                session_id,
                metadata_extras,
                extras,
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
            MessageRepr::RateLimitEvent { rate_limit_info, uuid, session_id, extras } => {
                Message::RateLimitEvent { rate_limit_info, uuid, session_id, extras }
            }
            MessageRepr::CommandLifecycle { command_uuid, state, uuid, session_id, extras } => {
                Message::CommandLifecycle { command_uuid, state, uuid, session_id, extras }
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
                extras,
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
                extras,
            },
            MessageRepr::StreamEvent { uuid, session_id, event, parent_tool_use_id, extras } => {
                Message::StreamEvent { uuid, session_id, event, parent_tool_use_id, extras }
            }
            MessageRepr::ToolProgress {
                tool_use_id,
                tool_name,
                elapsed_time_seconds,
                heartbeat,
                parent_tool_use_id,
                session_id,
                uuid,
                extras,
            } => Message::ToolProgress {
                tool_use_id,
                tool_name,
                elapsed_time_seconds,
                heartbeat,
                parent_tool_use_id,
                session_id,
                uuid,
                extras,
            },
            MessageRepr::Error { error, extras } => Message::Error { error, extras },
        }
    }
}

impl From<Message> for MessageRepr {
    fn from(msg: Message) -> Self {
        match msg {
            Message::Assistant {
                message,
                session_id,
                parent_tool_use_id,
                error,
                uuid,
                timestamp,
                extras,
            } => MessageRepr::Assistant {
                message,
                session_id,
                parent_tool_use_id,
                error,
                uuid,
                timestamp,
                extras,
            },
            Message::User {
                message,
                session_id,
                parent_tool_use_id,
                uuid,
                tool_use_result,
                timestamp,
                synthetic,
                extras,
            } => MessageRepr::User {
                message,
                session_id,
                parent_tool_use_id,
                uuid,
                tool_use_result,
                timestamp,
                synthetic,
                extras,
            },
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
                extras,
            } => MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::TaskStarted {
                task_id,
                description,
                uuid,
                session_id,
                tool_use_id,
                task_type,
                extras,
            })),
            Message::TaskUpdated { task_id, patch, uuid, session_id, extras } => {
                MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::TaskUpdated {
                    task_id,
                    patch,
                    uuid,
                    session_id,
                    extras,
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
                extras,
            } => MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::TaskProgress {
                task_id,
                description,
                usage,
                uuid,
                session_id,
                tool_use_id,
                last_tool_name,
                workflow_progress,
                extras,
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
                extras,
            } => MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::TaskNotification {
                task_id,
                status,
                output_file,
                summary,
                uuid,
                session_id,
                tool_use_id,
                usage,
                extras,
            })),
            Message::ThinkingTokens {
                estimated_tokens,
                estimated_tokens_delta,
                uuid,
                session_id,
                extras,
            } => MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::ThinkingTokens {
                estimated_tokens,
                estimated_tokens_delta,
                uuid,
                session_id,
                extras,
            })),
            Message::TurnDuration {
                ms,
                message_count,
                parent_tool_use_id,
                session_id,
                uuid,
                extras,
            } => MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::TurnDuration {
                ms,
                message_count,
                parent_tool_use_id,
                session_id,
                uuid,
                extras,
            })),
            Message::StopHookSummary {
                actions,
                hook_infos,
                hook_errors,
                has_output,
                level,
                prevented_continuation,
                stop_reason,
                tool_use_id,
                parent_tool_use_id,
                session_id,
                uuid,
                extras,
            } => MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::StopHookSummary {
                actions,
                hook_infos,
                hook_errors,
                has_output,
                level,
                prevented_continuation,
                stop_reason,
                tool_use_id,
                parent_tool_use_id,
                session_id,
                uuid,
                extras,
            })),
            Message::BackgroundTasksChanged { tasks, uuid, session_id, extras } => {
                MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::BackgroundTasksChanged {
                    tasks,
                    uuid,
                    session_id,
                    extras,
                }))
            }
            Message::CommandsChanged { commands, uuid, session_id, extras } => {
                MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::CommandsChanged {
                    commands,
                    uuid,
                    session_id,
                    extras,
                }))
            }
            Message::HookStarted { hook_id, hook_name, hook_event, uuid, session_id, extras } => {
                MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::HookStarted {
                    hook_id,
                    hook_name,
                    hook_event,
                    uuid,
                    session_id,
                    extras,
                }))
            }
            Message::HookResponse {
                hook_id,
                hook_name,
                hook_event,
                outcome,
                exit_code,
                output,
                stdout,
                stderr,
                uuid,
                session_id,
                extras,
            } => MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::HookResponse {
                hook_id,
                hook_name,
                hook_event,
                outcome,
                exit_code,
                output,
                stdout,
                stderr,
                uuid,
                session_id,
                extras,
            })),
            Message::HookProgress {
                hook_id,
                hook_name,
                hook_event,
                stdout,
                stderr,
                output,
                uuid,
                session_id,
                extras,
            } => MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::HookProgress {
                hook_id,
                hook_name,
                hook_event,
                stdout,
                stderr,
                output,
                uuid,
                session_id,
                extras,
            })),
            Message::Notification { key, text, priority, uuid, session_id, extras } => {
                MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::Notification {
                    key,
                    text,
                    priority,
                    uuid,
                    session_id,
                    extras,
                }))
            }
            Message::PermissionDenied {
                tool_name,
                tool_use_id,
                decision_reason_type,
                decision_reason,
                message,
                uuid,
                session_id,
                extras,
            } => MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::PermissionDenied {
                tool_name,
                tool_use_id,
                decision_reason_type,
                decision_reason,
                message,
                uuid,
                session_id,
                extras,
            })),
            Message::CompactBoundary {
                trigger,
                pre_tokens,
                post_tokens,
                uuid,
                session_id,
                metadata_extras,
                extras,
            } => MessageRepr::System(SystemRepr::Typed(TypedSystemRepr::CompactBoundary {
                compact_metadata: CompactMetadataRepr {
                    trigger,
                    pre_tokens,
                    post_tokens,
                    extras: metadata_extras,
                },
                uuid,
                session_id,
                extras,
            })),
            Message::RateLimitEvent { rate_limit_info, uuid, session_id, extras } => {
                MessageRepr::RateLimitEvent { rate_limit_info, uuid, session_id, extras }
            }
            Message::CommandLifecycle { command_uuid, state, uuid, session_id, extras } => {
                MessageRepr::CommandLifecycle { command_uuid, state, uuid, session_id, extras }
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
                extras,
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
                extras,
            },
            Message::StreamEvent { uuid, session_id, event, parent_tool_use_id, extras } => {
                MessageRepr::StreamEvent { uuid, session_id, event, parent_tool_use_id, extras }
            }
            Message::ToolProgress {
                tool_use_id,
                tool_name,
                elapsed_time_seconds,
                heartbeat,
                parent_tool_use_id,
                session_id,
                uuid,
                extras,
            } => MessageRepr::ToolProgress {
                tool_use_id,
                tool_name,
                elapsed_time_seconds,
                heartbeat,
                parent_tool_use_id,
                session_id,
                uuid,
                extras,
            },
            Message::Error { error, extras } => MessageRepr::Error { error, extras },
            // Defensive sentinel - `Serialize` for `Message` special-cases
            // `Unknown` to emit `raw` verbatim, so this branch is dead code
            // at runtime. Kept to keep the `From` impl total without
            // `unreachable!()` (banned by the workspace lint set).
            Message::Unknown { type_str, .. } => MessageRepr::Error {
                error: format!(
                    "Message::Unknown {{ type_str: {type_str:?} }} \
                     cannot be encoded via MessageRepr"
                ),
                extras: Extras::new(),
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

    /// A backend with no prompt cache reports the cache counters as
    /// `null` rather than `0`. Verbatim `usage` block measured from
    /// OpenRouter serving `z-ai/glm-5.3-flash`; a decode failure here
    /// ends the whole event stream, so the session dies mid-turn.
    #[test]
    fn usage_counters_decode_when_the_backend_sends_null() {
        let measured = serde_json::json!({
            "input_tokens": 13,
            "output_tokens": 24,
            "output_tokens_details": { "thinking_tokens": 24 },
            "cache_creation_input_tokens": null,
            "cache_read_input_tokens": 0,
            "cache_creation": null,
            "inference_geo": null,
            "server_tool_use": null,
            "service_tier": null,
            "speed": "standard",
            "cost": 1.395e-05,
            "is_byok": false
        });

        let usage: crate::Usage =
            serde_json::from_value(measured).expect("a null cache counter must not fail the frame");

        assert_eq!(usage.cache_creation_input_tokens, 0, "a null cache counter reads as zero");
        assert_eq!(usage.input_tokens, 13, "and the counters beside it still decode");
        assert_eq!(usage.output_tokens, 24, "and the counters beside it still decode");
    }

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

    /// The CLI stamps a user frame whose words nobody typed, and the
    /// view layer cannot tell the harness speaking from the reader
    /// without it. The reminder the harness injects is built with
    /// `isMeta` internally and the emitter writes that as `isSynthetic`
    /// (measured shape: the one user frame carrying text in
    /// `.claude/skills/claude-cli-upgrade/reference-captures/skill.jsonl`).
    #[test]
    fn user_frame_keeps_the_clis_synthetic_stamp() {
        let raw = json!({
            "type": "user",
            "session_id": "sess-usr",
            "uuid": "user-uuid-2",
            "isSynthetic": true,
            "message": {
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": "Skill /unslop was loaded earlier (see the invoked-skills reminder above); this is a NEW invocation."
                }]
            }
        });

        let msg: Message = serde_json::from_value(raw).expect("parse");
        let encoded = serde_json::to_value(&msg).expect("encode");

        assert_eq!(
            encoded.get("isSynthetic"),
            Some(&json!(true)),
            "the stamp must survive decode and re-encode, or every view reads these words as \
             the reader's own: {encoded}",
        );

        // The carrier that is not the reminder: the compaction summary, whose
        // frame the CLI emits with the mark computed from `isCompactSummary`.
        // Shape taken from `baselines/sdk/2.1.280/compact.jsonl`.
        let summary = json!({
            "type": "user",
            "session_id": "sess-summary",
            "uuid": "user-uuid-3",
            "isSynthetic": true,
            "isReplay": false,
            "message": {
                "role": "user",
                "content": "This session is being continued from a previous conversation that ran out of context."
            }
        });

        let msg: Message = serde_json::from_value(summary).expect("parse");
        let encoded = serde_json::to_value(&msg).expect("encode");

        assert_eq!(
            encoded.get("isSynthetic"),
            Some(&json!(true)),
            "a summary frame is nobody's typed words either: {encoded}",
        );
    }

    /// Only the CLI's own wire spelling stamps a frame. A companion row's
    /// `turnCompanion` is a disk field: `VFe` folds `isMeta`,
    /// `isVisibleInTranscriptOnly` and `isCompactSummary` into the wire's
    /// `isSynthetic`, and no outbound constructor writes anything else, so a
    /// frame stamped by `turnCompanion` alone is a disk-shaped capture rather
    /// than the CLI's bytes.
    #[test]
    fn the_wire_stamps_only_with_its_own_spelling() {
        let raw = json!({
            "type": "user",
            "session_id": "sess-usr",
            "turnCompanion": true,
            "message": {"role": "user", "content": "companion text"}
        });

        let decoded: Message = serde_json::from_value(raw).expect("parse");
        let encoded = serde_json::to_value(&decoded).expect("encode");

        assert_eq!(
            encoded.get("isSynthetic"),
            None,
            "a disk field must not stamp a frame: {encoded}",
        );
    }

    /// The stamp is present or absent, never `false`: every user frame
    /// forge has ever relayed is unmarked, so emitting the key on all of
    /// them would reshape the socket payload and the fixtures written
    /// against it.
    #[test]
    fn unmarked_user_frames_carry_no_synthetic_key() {
        let raw = json!({
            "type": "user",
            "session_id": "sess-usr",
            "message": {"role": "user", "content": "typed by hand"}
        });

        let decoded: Message = serde_json::from_value(raw).expect("parse");
        let encoded = serde_json::to_value(&decoded).expect("encode");

        assert_eq!(
            encoded.get("isSynthetic"),
            None,
            "a frame nobody stamped must not grow the key: {encoded}",
        );
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
        let Message::ThinkingTokens {
            estimated_tokens,
            estimated_tokens_delta,
            uuid,
            session_id,
            ..
        } = msg
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
            extras: Extras::new(),
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
        let Message::TurnDuration {
            ms, message_count, parent_tool_use_id, session_id, uuid, ..
        } = msg
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
            extras: Extras::new(),
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

    /// Frame is verbatim from the `compact` capture, extras included:
    /// the unmodelled `compact_metadata` siblings and
    /// `logical_parent_uuid` must not block the typed match, and must
    /// cross verbatim - a decode that keeps only what this build reads
    /// is a decode that drops CLI data.
    #[test]
    fn compact_boundary_decodes_the_three_fields_forge_reads() {
        let raw = json!({
            "type": "system",
            "subtype": "compact_boundary",
            "session_id": "sess-cb",
            "uuid": "cb-uuid",
            "compact_metadata": {
                "trigger": "manual",
                "pre_tokens": 68031,
                "post_tokens": 9149,
                "cumulative_dropped_tokens": 58882,
                "duration_ms": 48928,
                "preserved_segment": {"head_uuid": "h", "anchor_uuid": "a", "tail_uuid": "t"},
                "preserved_messages": {"anchor_uuid": "a", "uuids": ["h"], "all_uuids": ["h", "t"]},
            },
            "logical_parent_uuid": "lp-uuid",
        });
        let msg: Message = serde_json::from_value(raw.clone()).expect("decode");
        let Message::CompactBoundary {
            trigger,
            pre_tokens,
            post_tokens,
            uuid,
            session_id,
            metadata_extras,
            extras,
        } = msg
        else {
            panic!("compact_boundary must decode typed, not into System; got {msg:?}");
        };
        assert_eq!(trigger, "manual", "trigger drives the TUI's pending-compact clear");
        assert_eq!(pre_tokens, 68031, "pre_tokens is the number the usage panel shows");
        assert_eq!(post_tokens, 9149, "post_tokens is what the row says was carried after the cut");
        assert_eq!(uuid, "cb-uuid");
        assert_eq!(session_id, "sess-cb");
        assert_eq!(
            metadata_extras.get("duration_ms"),
            Some(&json!(48928)),
            "a metadata sibling crosses too; it is a fact about the cut, not noise",
        );
        assert_eq!(
            extras.get("logical_parent_uuid"),
            Some(&json!("lp-uuid")),
            "and a frame-level sibling crosses beside it",
        );

        // The extras come back out where they came in: siblings of the
        // metadata object stay nested under it, the frame's own stay at
        // the top level.
        let encoded = serde_json::to_value(&Message::CompactBoundary {
            trigger: "manual".to_owned(),
            pre_tokens: 68031,
            post_tokens: 9149,
            uuid: "cb-uuid".to_owned(),
            session_id: "sess-cb".to_owned(),
            metadata_extras,
            extras,
        })
        .expect("encode");
        assert_eq!(
            encoded,
            json!({
                "type": "system",
                "subtype": "compact_boundary",
                "session_id": "sess-cb",
                "uuid": "cb-uuid",
                "compact_metadata": {
                    "trigger": "manual",
                    "pre_tokens": 68031,
                    "post_tokens": 9149,
                    "cumulative_dropped_tokens": 58882,
                    "duration_ms": 48928,
                    "preserved_segment": {"head_uuid": "h", "anchor_uuid": "a", "tail_uuid": "t"},
                    "preserved_messages": {"anchor_uuid": "a", "uuids": ["h"], "all_uuids": ["h", "t"]},
                },
                "logical_parent_uuid": "lp-uuid",
            }),
            "encode re-nests the metadata siblings and keeps the frame's own at the top",
        );
    }

    /// The guard hard rule 9 buys by typing this subtype: a rename of
    /// either modelled field drops the frame into the generic bucket,
    /// which `EXPECTED_GENERIC_SYSTEM_SUBTYPES` does not list, so the
    /// next live capture fails instead of the TUI quietly losing the
    /// trigger. `preTokens` is the plausible drift - the TUI already
    /// carries an alias for that spelling - and the frame carries every
    /// other field, so that rename is the only thing that can drop it.
    #[test]
    fn a_renamed_pre_tokens_falls_to_the_generic_bucket() {
        let msg: Message = serde_json::from_value(json!({
            "type": "system",
            "subtype": "compact_boundary",
            "session_id": "sess-cb",
            "uuid": "cb-uuid",
            "compact_metadata": {"trigger": "manual", "preTokens": 68031, "post_tokens": 9149},
        }))
        .expect("decode");
        let Message::System { subtype, .. } = msg else {
            panic!("drifted metadata must NOT satisfy the typed match; got {msg:?}");
        };
        assert_eq!(subtype, "compact_boundary", "and it stays identifiable as the same subtype");
    }

    #[test]
    fn background_tasks_changed_encode_round_trips() {
        let msg = Message::BackgroundTasksChanged {
            tasks: vec![
                json!({"task_id": "t1", "task_type": "local_workflow", "description": "d"}),
            ],
            uuid: "bg-uuid".to_owned(),
            session_id: "sess-bg".to_owned(),
            extras: Extras::new(),
        };
        let encoded = serde_json::to_value(&msg).expect("encode");
        assert_eq!(encoded["type"], "system");
        assert_eq!(encoded["subtype"], "background_tasks_changed");
        let decoded: Message = serde_json::from_value(encoded).expect("decode");
        assert_eq!(decoded, msg);
    }

    #[test]
    fn commands_changed_encode_round_trips() {
        let msg = Message::CommandsChanged {
            commands: vec![json!({"name": "audit", "description": "sweep"})],
            uuid: "cmd-uuid".to_owned(),
            session_id: "sess-cmd".to_owned(),
            extras: Extras::new(),
        };
        let encoded = serde_json::to_value(&msg).expect("encode");
        assert_eq!(encoded["type"], "system");
        assert_eq!(encoded["subtype"], "commands_changed");
        let decoded: Message = serde_json::from_value(encoded).expect("decode");
        assert_eq!(decoded, msg);
    }

    #[test]
    fn hook_started_encode_round_trips() {
        let msg = Message::HookStarted {
            hook_id: "h1".to_owned(),
            extras: Extras::new(),
            hook_name: "SessionStart:startup".to_owned(),
            hook_event: "SessionStart".to_owned(),
            uuid: "hs-uuid".to_owned(),
            session_id: "sess-hs".to_owned(),
        };
        let encoded = serde_json::to_value(&msg).expect("encode");
        assert_eq!(encoded["type"], "system");
        assert_eq!(encoded["subtype"], "hook_started");
        let decoded: Message = serde_json::from_value(encoded).expect("decode");
        assert_eq!(decoded, msg);
    }

    #[test]
    fn hook_response_encode_round_trips() {
        let msg = Message::HookResponse {
            hook_id: "h1".to_owned(),
            hook_name: "SessionStart:startup".to_owned(),
            hook_event: "SessionStart".to_owned(),
            outcome: "success".to_owned(),
            exit_code: 0,
            output: "body".to_owned(),
            stdout: "body".to_owned(),
            stderr: String::new(),
            uuid: "hr-uuid".to_owned(),
            session_id: "sess-hr".to_owned(),
            extras: Extras::new(),
        };
        let encoded = serde_json::to_value(&msg).expect("encode");
        assert_eq!(encoded["type"], "system");
        assert_eq!(encoded["subtype"], "hook_response");
        let decoded: Message = serde_json::from_value(encoded).expect("decode");
        assert_eq!(decoded, msg);
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
        assert_eq!(hook_infos[0].duration_ms, Some(980));
        assert_eq!(parent_tool_use_id.as_deref(), Some("uuid_2"));
        assert_eq!(session_id, "session_0");
    }

    #[test]
    fn stop_hook_summary_errors_cross_the_crate() {
        // The CLI sends `hookErrors` on every stop_hook_summary row, empty
        // when nothing failed, and the client's hooks chip draws them - so the
        // key has to survive decode and re-encode. This row is a real failing
        // one, the ralph-wiggum plugin's directory gone.
        let raw = json!({
            "type": "system",
            "subtype": "stop_hook_summary",
            "hookCount": 1,
            "hookInfos": [{"command": "${CLAUDE_PLUGIN_ROOT}/hooks/stop-hook.sh", "durationMs": 0}],
            "hookErrors": [
                "Failed to run: Plugin directory does not exist: /Users/vedhavyas/.claude/plugins/cache/claude-code-plugins/ralph-wiggum/1.0.0 (ralph-wiggum@claude-code-plugins \u{2014} run /plugin to reinstall)"
            ],
            "hookAdditionalContext": [],
            "preventedContinuation": false,
            "stopReason": "",
            "hasOutput": true,
            "level": "suggestion",
            "toolUseID": "8bdfbd8a-a578-441d-97ff-4d8a2923e1e7",
            "session_id": "3dc2afa8-fdd1-40ff-bc7d-ffaace19246a",
            "uuid": "225cae5c-f638-4b31-afb2-700b8303dc16",
        });
        let msg: Message = serde_json::from_value(raw).expect("decode");
        let Message::StopHookSummary { .. } = &msg else {
            panic!("expected the typed variant, got {msg:?}");
        };
        let encoded = serde_json::to_value(&msg).expect("encode");
        assert_eq!(
            encoded["hookErrors"],
            json!([
                "Failed to run: Plugin directory does not exist: /Users/vedhavyas/.claude/plugins/cache/claude-code-plugins/ralph-wiggum/1.0.0 (ralph-wiggum@claude-code-plugins \u{2014} run /plugin to reinstall)"
            ]),
            "the key a client reads is on the wire, not only on the CLI's bytes"
        );

        // Empty is the shape most rows carry, and the key is present rather
        // than omitted: the socket contract records the key set, and a skipped
        // empty would leave it absent there.
        let quiet = json!({
            "type": "system",
            "subtype": "stop_hook_summary",
            "hookCount": 2,
            "hookInfos": [{"command": "bash hook.sh", "durationMs": 980}],
            "hookErrors": [],
            "level": "suggestion",
            "preventedContinuation": false,
            "stopReason": "",
            "hasOutput": true,
            "toolUseID": "5e586a7f",
            "session_id": "session_0",
            "uuid": "uuid_3",
        });
        let msg: Message = serde_json::from_value(quiet).expect("decode");
        let Message::StopHookSummary { .. } = &msg else {
            panic!("expected the typed variant, got {msg:?}");
        };
        let encoded = serde_json::to_value(&msg).expect("encode");
        assert_eq!(encoded["hookErrors"], json!([]), "and empty crosses as empty");
    }

    #[test]
    fn stop_hook_summary_entry_without_duration_decodes_typed() {
        // 2.1.263 emits hookInfos entries without `durationMs` (observed
        // for plugin-injected hooks), and prompt-driven hooks add
        // `promptText`. The entry must still decode as the typed
        // variant, not fall to the generic system bucket.
        let raw = json!({
            "type": "system",
            "subtype": "stop_hook_summary",
            "hasOutput": true,
            "hookCount": 2,
            "hookErrors": [],
            "hookInfos": [
                {"command": "bash ~/.claude/hooks/cmux-notify.sh", "durationMs": 980},
                {"command": "${CLAUDE_PLUGIN_ROOT}/hooks/reminder.sh",
                 "promptText": "Check verification completeness"},
            ],
            "level": "suggestion",
            "preventedContinuation": false,
            "session_id": "session_0",
            "stopReason": "",
            "toolUseID": "5e586a7f",
            "uuid": "uuid_3",
        });
        let msg: Message = serde_json::from_value(raw).expect("decode");
        let Message::StopHookSummary { actions, hook_infos, .. } = msg else {
            panic!("expected StopHookSummary, got {msg:?}");
        };
        assert_eq!(actions, 2);
        assert_eq!(hook_infos[0].duration_ms, Some(980));
        assert_eq!(hook_infos[1].duration_ms, None, "durationMs may be absent");
        assert_eq!(hook_infos[1].command, "${CLAUDE_PLUGIN_ROOT}/hooks/reminder.sh");
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
        let WorkflowProgressEvent::WorkflowPhase { index, title, .. } = &workflow_progress[0]
        else {
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
            extras,
            ..
        } = &workflow_progress[1]
        else {
            panic!("second event must be WorkflowAgent, got {:?}", workflow_progress[1]);
        };
        assert_eq!(*phase_index, Some(1));
        assert_eq!(phase_title, &Some("Ping".to_string()));
        assert_eq!(state, "done");
        assert_eq!(last_tool_name.as_deref(), Some("StructuredOutput"));
        assert_eq!(last_tool_summary.as_deref(), Some("pong"));
        assert_eq!(result_preview.as_deref(), Some("{\"answer\":\"pong\",\"confidence\":1}"));
        assert_eq!(
            extras.get("agentId"),
            Some(&json!("abf")),
            "the entry's unmodelled facts (agentId, model, timings, tokens) stay on the entry",
        );
        assert_eq!(extras.get("tokens"), Some(&json!(54_707)));
        assert_eq!(extras.get("model"), Some(&json!("claude-opus-4-7")));

        // And they come back out on the entry they arrived in, not at the
        // frame's own level.
        let encoded = serde_json::to_value(&Message::TaskProgress {
            task_id: "woc6i1sab".to_owned(),
            description: String::new(),
            usage: TaskUsage {
                total_tokens: 0,
                tool_uses: 0,
                duration_ms: 0,
                extras: Extras::new(),
            },
            uuid: "u".to_owned(),
            session_id: "s".to_owned(),
            tool_use_id: None,
            last_tool_name: None,
            workflow_progress: vec![WorkflowProgressEvent::WorkflowAgent {
                index: 2,
                label: "pong".to_owned(),
                phase_index: Some(1),
                phase_title: None,
                state: "done".to_owned(),
                last_tool_name: None,
                last_tool_summary: None,
                result_preview: None,
                extras: extras.clone(),
            }],
            extras: Extras::new(),
        })
        .expect("encode");
        assert_eq!(encoded["workflow_progress"][0]["agentId"], "abf");
        assert_eq!(encoded.get("agentId"), None, "an entry's extras never leak up to the frame");
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

    #[test]
    fn background_tasks_changed_decodes_as_typed_variant() {
        let raw = json!({
            "type": "system",
            "subtype": "background_tasks_changed",
            "tasks": [{
                "task_id": "wfm8lm8vx",
                "task_type": "local_workflow",
                "description": "Fan out two trivial agents returning ping and pong",
            }],
            "uuid": "145d5225-f94f-411a-b76b-d7bef6506eff",
            "session_id": "c35950bc-376e-4c74-be2d-8f31f32c613b",
        });
        let msg: Message = serde_json::from_value(raw).expect("decode");
        let Message::BackgroundTasksChanged { tasks, session_id, .. } = msg else {
            panic!("expected BackgroundTasksChanged, got {msg:?}");
        };
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["task_id"], "wfm8lm8vx");
        assert_eq!(session_id, "c35950bc-376e-4c74-be2d-8f31f32c613b");
    }

    #[test]
    fn commands_changed_decodes_as_typed_variant() {
        let raw = json!({
            "type": "system",
            "subtype": "commands_changed",
            "commands": [{"name": "audit", "description": "sweep a codebase"}],
            "uuid": "72af9cde-8c41-434d-baea-f2353a66dab8",
            "session_id": "35f88fef-f4c4-4c35-bffa-e363335aa2ac",
        });
        let msg: Message = serde_json::from_value(raw).expect("decode");
        let Message::CommandsChanged { commands, .. } = msg else {
            panic!("expected CommandsChanged, got {msg:?}");
        };
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0]["name"], "audit");
    }

    #[test]
    fn hook_started_decodes_as_typed_variant() {
        let raw = json!({
            "type": "system",
            "subtype": "hook_started",
            "hook_id": "cc3b0e3e-f894-43dd-9819-e594a4aa4904",
            "hook_name": "SessionStart:startup",
            "hook_event": "SessionStart",
            "uuid": "0a58da42-e1bd-4b87-9d50-711800d43388",
            "session_id": "c35950bc-376e-4c74-be2d-8f31f32c613b",
        });
        let msg: Message = serde_json::from_value(raw).expect("decode");
        let Message::HookStarted { hook_name, hook_event, hook_id, .. } = msg else {
            panic!("expected HookStarted, got {msg:?}");
        };
        assert_eq!(hook_name, "SessionStart:startup");
        assert_eq!(hook_event, "SessionStart");
        assert_eq!(hook_id, "cc3b0e3e-f894-43dd-9819-e594a4aa4904");
    }

    #[test]
    fn hook_response_decodes_as_typed_variant() {
        let raw = json!({
            "type": "system",
            "subtype": "hook_response",
            "hook_id": "946738fb-ca47-4c50-861f-d763fd50eab9",
            "hook_name": "SessionStart:startup",
            "hook_event": "SessionStart",
            "output": "index body",
            "outcome": "success",
            "exit_code": 0,
            "stderr": "",
            "stdout": "index body",
            "uuid": "d6867063-339c-469d-803d-169cddc5b4eb",
            "session_id": "c35950bc-376e-4c74-be2d-8f31f32c613b",
        });
        let msg: Message = serde_json::from_value(raw).expect("decode");
        let Message::HookResponse { outcome, exit_code, stdout, .. } = msg else {
            panic!("expected HookResponse, got {msg:?}");
        };
        assert_eq!(outcome, "success");
        assert_eq!(exit_code, 0);
        assert_eq!(stdout, "index body");
    }

    #[test]
    fn hook_progress_decodes_as_typed_variant() {
        let raw = json!({
            "type": "system",
            "subtype": "hook_progress",
            "hook_id": "012697b9-e191-42e6-9385-cee11f7a17d3",
            "hook_name": "SessionStart:startup",
            "hook_event": "SessionStart",
            "stdout": "{\"async\": true}",
            "stderr": "",
            "output": "{\"async\": true}",
            "uuid": "28d6a071-ae36-4cb8-bbdb-86686acea753",
            "session_id": "e30daa8a-1702-4afd-8379-cab1d235935e",
        });
        let msg: Message = serde_json::from_value(raw).expect("decode");
        let Message::HookProgress { hook_id, hook_name, hook_event, stdout, .. } = msg else {
            panic!("expected HookProgress, got {msg:?}");
        };
        assert_eq!(hook_id, "012697b9-e191-42e6-9385-cee11f7a17d3");
        assert_eq!(hook_name, "SessionStart:startup");
        assert_eq!(hook_event, "SessionStart");
        assert_eq!(stdout, "{\"async\": true}");
    }

    #[test]
    fn permission_denied_decodes_as_typed_variant() {
        // Shape first observed on 2.1.280.
        let raw = json!({
            "type": "system",
            "subtype": "permission_denied",
            "tool_name": "Bash",
            "tool_use_id": "toolu_01FCzjj9ZBDd3HA6HF1LaBGE",
            "decision_reason_type": "other",
            "decision_reason": "Contains simple_expansion",
            "message": "Contains simple_expansion",
            "uuid": "8c7161ee-b318-485e-b733-92cea085ac5b",
            "session_id": "4513c0dc-06a8-4c91-add5-9fdad5d00783",
        });
        let msg: Message = serde_json::from_value(raw).expect("decode");
        let Message::PermissionDenied { tool_name, tool_use_id, decision_reason, .. } = msg else {
            panic!("expected PermissionDenied, got {msg:?}");
        };
        assert_eq!(tool_name, "Bash");
        assert_eq!(tool_use_id, "toolu_01FCzjj9ZBDd3HA6HF1LaBGE");
        assert_eq!(decision_reason, "Contains simple_expansion");
    }

    #[test]
    fn workflow_task_progress_agent_without_phase_decodes_typed() {
        // Agent entries can arrive without `phaseIndex`/`phaseTitle`
        // (observed outside the pinned corpus). The entry must still
        // decode as a typed WorkflowAgent, not fall to the generic
        // system bucket.
        let raw = json!({
            "type": "system",
            "subtype": "task_progress",
            "task_id": "wq8nlqkoi",
            "tool_use_id": "call_17ac8f53355f48eba45c1a39",
            "description": "ping",
            "usage": {"total_tokens": 0, "tool_uses": 0, "duration_ms": 27},
            "last_tool_name": "ping",
            "summary": "Two trivial agents in parallel",
            "workflow_progress": [
                {
                    "type": "workflow_agent",
                    "index": 1,
                    "label": "ping",
                    "agentId": "a05487a5d7a14d1db",
                    "model": "claude-opus-4-8",
                    "state": "start",
                    "startedAt": 1_788_848_411_456_u64,
                    "queuedAt": 1_788_848_411_455_u64,
                    "attempt": 1,
                    "promptPreview": "Reply with the single word ping.",
                    "lastProgressAt": 1_788_848_411_456_u64,
                },
            ],
            "uuid": "uuid_a",
            "session_id": "session_a",
        });
        let msg: Message = serde_json::from_value(raw).expect("decode");
        let Message::TaskProgress { workflow_progress, .. } = msg else {
            panic!("expected TaskProgress, got {msg:?}");
        };
        let WorkflowProgressEvent::WorkflowAgent { phase_index, phase_title, state, .. } =
            &workflow_progress[0]
        else {
            panic!("expected WorkflowAgent, got {:?}", workflow_progress[0]);
        };
        assert_eq!(*phase_index, None);
        assert_eq!(phase_title, &None);
        assert_eq!(state, "start");
    }

    #[test]
    fn notification_decodes_as_typed_variant() {
        let raw = json!({
            "type": "system",
            "subtype": "notification",
            "key": "stop-hook-error",
            "text": "Stop hook error occurred \u{b7} ctrl+o to see",
            "priority": "immediate",
            "session_id": "e0aed9bb-b0de-43d5-8ded-4c5c1778f0fd",
            "uuid": "9d893480-969b-4f02-9ce6-666f87184fa0",
        });
        let msg: Message = serde_json::from_value(raw).expect("decode");
        let Message::Notification { key, text, priority, .. } = msg else {
            panic!("expected Notification, got {msg:?}");
        };
        assert_eq!(key.as_deref(), Some("stop-hook-error"));
        assert_eq!(text, "Stop hook error occurred \u{b7} ctrl+o to see");
        assert_eq!(priority.as_deref(), Some("immediate"));
    }

    #[test]
    fn status_subtype_stays_generic_system() {
        // `status` is heterogeneous (compacting / compact-result /
        // permissionMode); it deliberately stays on the generic
        // catch-all rather than a typed variant.
        let raw = json!({
            "type": "system",
            "subtype": "status",
            "status": null,
            "permissionMode": "plan",
            "uuid": "c77b2ac7-825f-4fb6-a7e5-3d6fd940b293",
            "session_id": "2b5e2c9f-70c7-4df5-81fa-1b5507ce96a4",
        });
        let msg: Message = serde_json::from_value(raw).expect("decode");
        let Message::System { subtype, data, .. } = msg else {
            panic!("expected generic System, got {msg:?}");
        };
        assert_eq!(subtype, "status");
        assert_eq!(data["permissionMode"], "plan");
    }

    /// The CLI's `task_started` carries four fields forge models nothing
    /// of: the sub-agent's TYPE, whether the CLI backgrounded it, the
    /// dispatch depth, and the prompt. A client folding the raw frame
    /// needs all four, and the socket carries only what the decode kept.
    /// Line verbatim from the live parallel-dispatch capture. No modelled
    /// field here is optional-with-a-null, so the whole frame pins as
    /// parse-equality rather than key by key.
    #[test]
    fn a_task_started_frame_keeps_the_fields_no_view_reads_yet() {
        let raw = json!({
            "type": "system",
            "subtype": "task_started",
            "task_id": "aef3c170a790e8ca7",
            "tool_use_id": "call_b7855529e53e4747bd1f5747",
            "description": "alpha-work",
            "subagent_type": "general-purpose",
            "is_backgrounded": false,
            "spawn_depth": 1,
            "task_type": "local_agent",
            "prompt": "First write one short sentence of setup text.",
            "uuid": "af3bfd20-5ae3-48f5-b092-be3fb3017e79",
            "session_id": "ee9485a0-93c8-46c7-940d-3acaa74c60cf",
        });
        let msg: Message = serde_json::from_value(raw.clone()).expect("decode");
        let Message::TaskStarted { task_id, tool_use_id, .. } = &msg else {
            panic!("expected TaskStarted, got {msg:?}");
        };
        assert_eq!(task_id, "aef3c170a790e8ca7", "the modelled fields still decode");
        assert_eq!(
            tool_use_id.as_deref(),
            Some("call_b7855529e53e4747bd1f5747"),
            "and the dispatch link stays the link",
        );

        let encoded = serde_json::to_value(&msg).expect("encode");
        assert_eq!(
            encoded, raw,
            "every field the CLI sent must survive decode and re-serialization",
        );
    }

    /// Every frame a sub-agent produces is stamped with the agent type and
    /// the dispatch's description, and the frame carries the CLI's request
    /// id. None is modelled. The nested envelope and usage carry keys forge
    /// does not read either, and the tool_use block carries `caller` -
    /// all verbatim from the live capture.
    #[test]
    fn a_subagent_inner_assistant_frame_keeps_its_type_description_and_nested_extra() {
        let raw = json!({
            "type": "assistant",
            "message": {
                "id": "msg_011CfLJAJ6Ks5wkwmWpki9J9",
                "role": "assistant",
                "model": "claude-opus-5",
                "provider": "firstParty",
                "container": null,
                "content": [{
                    "type": "tool_use",
                    "id": "toolu_01PRrDjCCJb6F7rS3TQXzWch",
                    "name": "Bash",
                    "input": {"command": "echo forge-subagent-ok"},
                    "caller": {"type": "direct"},
                }],
                "stop_reason": "tool_use",
                "usage": {
                    "input_tokens": 2,
                    "output_tokens": 31,
                    "service_tier": "standard",
                    "inference_geo": "not_available",
                },
            },
            "parent_tool_use_id": "toolu_017brkbqcpsQAmHTWV6unGpQ",
            "session_id": "b84b585a-da00-4eb8-827d-935aacfef568",
            "uuid": "0e72be3a-647d-4e4b-bf27-6a91a6db5ee6",
            "timestamp": "2026-09-23T11:31:44.125Z",
            "request_id": "req_011CfL5GYC4ALgjAdC1ekWhH",
            "subagent_type": "general-purpose",
            "task_description": "Run echo command",
        });
        let msg: Message = serde_json::from_value(raw).expect("decode");
        let Message::Assistant { parent_tool_use_id, .. } = &msg else {
            panic!("expected Assistant, got {msg:?}");
        };
        assert_eq!(
            parent_tool_use_id.as_deref(),
            Some("toolu_017brkbqcpsQAmHTWV6unGpQ"),
            "the parent link is modelled and stays",
        );

        let encoded = serde_json::to_value(&msg).expect("encode");
        assert_eq!(encoded["subagent_type"], "general-purpose", "the agent type survives");
        assert_eq!(encoded["task_description"], "Run echo command", "the dispatch's name survives");
        assert_eq!(
            encoded["request_id"], "req_011CfL5GYC4ALgjAdC1ekWhH",
            "the request id survives"
        );
        assert_eq!(
            encoded["message"]["provider"], "firstParty",
            "an unmodelled envelope key survives"
        );
        assert_eq!(encoded["message"]["container"], Value::Null, "including an explicit null");
        assert_eq!(
            encoded["message"]["usage"]["service_tier"], "standard",
            "and the usage detail survives beside the counters forge counts",
        );
        assert_eq!(
            encoded["message"]["content"][0]["caller"]["type"], "direct",
            "and an unmodelled key inside the content block's own object survives",
        );
    }

    /// The sub-agent's own user frames carry the same two stamps. The
    /// tool_result block inside is what the parent dispatch body answers
    /// with, and its content is already an opaque value.
    #[test]
    fn a_subagent_inner_user_frame_keeps_its_type_and_description() {
        let raw = json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{
                    "tool_use_id": "toolu_01PRrDjCCJb6F7rS3TQXzWch",
                    "type": "tool_result",
                    "content": "forge-subagent-ok",
                    "is_error": false,
                }],
            },
            "parent_tool_use_id": "toolu_017brkbqcpsQAmHTWV6unGpQ",
            "session_id": "b84b585a-da00-4eb8-827d-935aacfef568",
            "uuid": "83f5bfa9-1510-4bbe-a0db-d3a0ceb54f12",
            "timestamp": "2026-09-23T11:31:46.071Z",
            "subagent_type": "general-purpose",
            "task_description": "Run echo command",
        });
        let msg: Message = serde_json::from_value(raw).expect("decode");
        let Message::User { parent_tool_use_id, .. } = &msg else {
            panic!("expected User, got {msg:?}");
        };
        assert_eq!(parent_tool_use_id.as_deref(), Some("toolu_017brkbqcpsQAmHTWV6unGpQ"));

        let encoded = serde_json::to_value(&msg).expect("encode");
        assert_eq!(encoded["subagent_type"], "general-purpose", "the agent type survives");
        assert_eq!(encoded["task_description"], "Run echo command", "the dispatch's name survives");
        assert_eq!(
            encoded["message"]["content"][0]["content"], "forge-subagent-ok",
            "and the inner result stays whole",
        );
    }

    /// A turn's `result` carries the CLI's own roll-up of the sub-agent
    /// work it ran, and latency fields forge does not model. Neither has a
    /// reader today; both must cross so a client can fold them.
    #[test]
    fn a_result_frame_keeps_the_subagent_rollup_and_latency_fields() {
        let raw = json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "num_turns": 3,
            "duration_ms": 12000,
            "duration_api_ms": 9000,
            "result": "done",
            "session_id": "ee9485a0-93c8-46c7-940d-3acaa74c60cf",
            "subagent_stats": {
                "spawned": 2,
                "requested": {"background": 1, "foreground": 1, "unset": 0},
                "started_in_background": 1,
                "max_depth": 1,
                "spawned_by_subagents": 0,
                "completed": 2,
                "failed": 0,
                "killed": {"parent": 0, "user": 0, "system": 0},
                "refused": {"depth_limit": 0, "concurrency_limit": 0, "budget": 0},
                "by_type": {"general-purpose": 2},
            },
            "ttft_ms": 1200,
            "queued_turn_count": 0,
        });
        let msg: Message = serde_json::from_value(raw).expect("decode");
        let Message::Result { num_turns, .. } = &msg else {
            panic!("expected Result, got {msg:?}");
        };
        assert_eq!(*num_turns, 3, "the modelled fields still decode");

        let encoded = serde_json::to_value(&msg).expect("encode");
        assert_eq!(encoded["subagent_stats"]["spawned"], 2, "the roll-up survives");
        assert_eq!(
            encoded["subagent_stats"]["by_type"]["general-purpose"], 2,
            "including the per-type tally",
        );
        assert_eq!(encoded["ttft_ms"], 1200, "and the latency field survives");
    }

    /// One frame per remaining category, each built with a key this build
    /// does not model, so every repr the decode touches has at least one
    /// round-trip pinning that its extras cross.
    #[test]
    fn every_frame_category_keeps_an_unmodelled_key() {
        let cases = [
            (
                json!({
                    "type": "rate_limit_event",
                    "rate_limit_info": {"status": "allowed", "utilization": 0.25},
                    "uuid": "rl-1",
                    "session_id": "s",
                    "window_started_at": 1_790_000_000,
                }),
                "window_started_at",
            ),
            (
                json!({
                    "type": "command_lifecycle",
                    "command_uuid": "c-1",
                    "state": "queued",
                    "uuid": "cl-1",
                    "session_id": "s",
                    "queue_position": 2,
                }),
                "queue_position",
            ),
            (
                json!({
                    "type": "stream_event",
                    "uuid": "se-1",
                    "session_id": "s",
                    "event": {"type": "message_start"},
                    "parent_tool_use_id": "toolu_dispatch",
                    "ttl_ms": 30_000,
                }),
                "ttl_ms",
            ),
            (json!({"type": "error", "error": "pipe broken", "recoverable": false}), "recoverable"),
        ];
        for (raw, extra) in cases {
            let msg: Message = serde_json::from_value(raw.clone()).expect("decode");
            let encoded = serde_json::to_value(&msg).expect("encode");
            assert_eq!(
                encoded.get(extra),
                raw.get(extra),
                "{extra} must survive decode and re-serialization for {}",
                raw["type"],
            );
        }
    }

    /// The nested values a view folds numbers out of keep their own
    /// unmodelled keys too: a task's usage block and a task patch are
    /// re-serialized under the same object they arrived in.
    #[test]
    fn a_task_usage_and_patch_keep_their_unmodelled_keys() {
        let raw = json!({
            "type": "system",
            "subtype": "task_progress",
            "task_id": "t-1",
            "description": "d",
            "usage": {"total_tokens": 1, "tool_uses": 2, "duration_ms": 3, "paused_ms": 4},
            "uuid": "u-1",
            "session_id": "s",
        });
        let msg: Message = serde_json::from_value(raw).expect("decode");
        let encoded = serde_json::to_value(&msg).expect("encode");
        assert_eq!(
            encoded["usage"]["paused_ms"], 4,
            "an unmodelled usage counter crosses under its own object",
        );

        let raw = json!({
            "type": "system",
            "subtype": "task_updated",
            "task_id": "t-1",
            "patch": {"status": "completed", "end_time": 1, "paused_ms": 4},
            "uuid": "u-2",
            "session_id": "s",
        });
        let msg: Message = serde_json::from_value(raw).expect("decode");
        let encoded = serde_json::to_value(&msg).expect("encode");
        assert_eq!(
            encoded["patch"]["paused_ms"], 4,
            "and an unmodelled patch key crosses the same way",
        );
    }

    /// `task_progress` re-fires per tool the sub-agent calls, stamped with
    /// the same agent type. The usage counters and last tool name are
    /// modelled; the type is not.
    #[test]
    fn a_task_progress_frame_keeps_the_subagent_type() {
        let raw = json!({
            "type": "system",
            "subtype": "task_progress",
            "task_id": "aef3c170a790e8ca7",
            "tool_use_id": "call_b7855529e53e4747bd1f5747",
            "description": "Running Echo alpha-one",
            "subagent_type": "general-purpose",
            "usage": {"total_tokens": 10314, "tool_uses": 1, "duration_ms": 1669},
            "last_tool_name": "Bash",
            "uuid": "c8128321-b588-45d3-9b69-23cb638345ce",
            "session_id": "ee9485a0-93c8-46c7-940d-3acaa74c60cf",
        });
        let msg: Message = serde_json::from_value(raw).expect("decode");
        let Message::TaskProgress { usage, last_tool_name, .. } = &msg else {
            panic!("expected TaskProgress, got {msg:?}");
        };
        assert_eq!(usage.tool_uses, 1, "the modelled usage still decodes");
        assert_eq!(last_tool_name.as_deref(), Some("Bash"));

        let encoded = serde_json::to_value(&msg).expect("encode");
        assert_eq!(encoded["subagent_type"], "general-purpose", "the agent type survives");
    }
}
