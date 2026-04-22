//! Typed input payloads for each hook event.
//!
//! Mirrors claude-agent-sdk-python v0.1.64 `types.py:231-344`. Every concrete
//! input flattens [`BaseHookInput`] so callbacks can access `session_id`,
//! `transcript_path`, `cwd`, and (optionally) `permission_mode` without
//! reaching back into [`super::HookContext`].

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Fields shared by every hook input the CLI emits.
///
/// Ported from claude-agent-sdk-python v0.1.64 `src/claude_agent_sdk/types.py:231-237`
/// (`BaseHookInput` `TypedDict`). Every concrete input type below flattens
/// this struct so callbacks can access `session_id`, `transcript_path`,
/// `cwd`, and (optionally) `permission_mode` without reaching back into
/// [`super::HookContext`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaseHookInput {
    /// Session identifier the hook fired in.
    pub session_id: String,
    /// Filesystem path of the session transcript JSONL file.
    pub transcript_path: String,
    /// Working directory of the CLI when the hook fired.
    pub cwd: String,
    /// Permission mode active for the call (e.g. `"default"`,
    /// `"bypassPermissions"`). Absent for hook frames that predate this
    /// field or where the CLI omits it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
}

/// Optional sub-agent attribution present on tool-lifecycle hook inputs
/// (`PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `PermissionRequest`).
///
/// Ported from `types.py:246-263` (`_SubagentContextMixin`). Fields are
/// populated when the hook fires inside a `Task`-spawned sub-agent so
/// tool events can be attributed back to the right agent when multiple
/// sub-agents interleave over the same control channel.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubagentContext {
    /// Sub-agent identifier. Matches the `agent_id` emitted by that
    /// sub-agent's `SubagentStart` / `SubagentStop` hooks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Agent type name (e.g. `"general-purpose"`, `"code-reviewer"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
}

/// Input payload for `PreToolUse`. Ported from `types.py:266-272`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreToolUseInput {
    /// Shared hook context (`session_id`, `transcript_path`, `cwd`, …).
    #[serde(flatten)]
    pub base: BaseHookInput,
    /// Optional sub-agent attribution.
    #[serde(flatten)]
    pub subagent: SubagentContext,
    /// Tool the model wants to invoke.
    pub tool_name: String,
    /// The model's proposed input.
    pub tool_input: Value,
    /// Opaque tool-use identifier the CLI will reference in the matching
    /// `PostToolUse` / `PostToolUseFailure` frame.
    pub tool_use_id: String,
}

/// Input payload for `PostToolUse`. Ported from `types.py:275-282`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostToolUseInput {
    /// Shared hook context.
    #[serde(flatten)]
    pub base: BaseHookInput,
    /// Optional sub-agent attribution.
    #[serde(flatten)]
    pub subagent: SubagentContext,
    /// Tool that was invoked.
    pub tool_name: String,
    /// The input the tool actually ran with.
    pub tool_input: Value,
    /// The tool's response payload.
    pub tool_response: Value,
    /// Tool-use identifier matching the preceding `PreToolUse` frame.
    pub tool_use_id: String,
}

/// Input payload for `PostToolUseFailure`. Ported from `types.py:285-292`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostToolUseFailureInput {
    /// Shared hook context.
    #[serde(flatten)]
    pub base: BaseHookInput,
    /// Optional sub-agent attribution.
    #[serde(flatten)]
    pub subagent: SubagentContext,
    /// Tool that was invoked and failed.
    pub tool_name: String,
    /// Input the tool ran with.
    pub tool_input: Value,
    /// Tool-use identifier matching the preceding `PreToolUse` frame.
    pub tool_use_id: String,
    /// Error message the tool surfaced.
    pub error: String,
    /// `Some(true)` when the failure was caused by user interruption; `None`
    /// when the CLI omits the field (upstream `NotRequired`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_interrupt: Option<bool>,
}

/// Input payload for `UserPromptSubmit`. Ported from `types.py:294-298`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserPromptSubmitInput {
    /// Shared hook context.
    #[serde(flatten)]
    pub base: BaseHookInput,
    /// Raw prompt text the user submitted.
    pub prompt: String,
}

/// Input payload for `Stop`. Ported from `types.py:301-305`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StopInput {
    /// Shared hook context.
    #[serde(flatten)]
    pub base: BaseHookInput,
    /// `true` when the stop-hook chain was previously active (re-entrant
    /// call). Callbacks typically return `HookDecision::passthrough()`
    /// when this is set to avoid infinite loops.
    pub stop_hook_active: bool,
}

/// Input payload for `SubagentStop`. Ported from `types.py:308-314`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentStopInput {
    /// Shared hook context.
    #[serde(flatten)]
    pub base: BaseHookInput,
    /// Re-entrancy indicator (see [`StopInput::stop_hook_active`]).
    pub stop_hook_active: bool,
    /// Sub-agent identifier.
    pub agent_id: String,
    /// Filesystem path of the sub-agent's transcript JSONL file.
    pub agent_transcript_path: String,
    /// Agent type (e.g. `"general-purpose"`).
    pub agent_type: String,
}

/// Input payload for `PreCompact`. Ported from `types.py:317-321`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreCompactInput {
    /// Shared hook context.
    #[serde(flatten)]
    pub base: BaseHookInput,
    /// Trigger that prompted the compaction — `"manual"` or `"auto"`.
    pub trigger: String,
    /// Caller-supplied compaction guidance. `None` when the CLI did not
    /// pass custom instructions.
    #[serde(default)]
    pub custom_instructions: Option<String>,
}

/// Input payload for `Notification`. Ported from `types.py:324-328`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationInput {
    /// Shared hook context.
    #[serde(flatten)]
    pub base: BaseHookInput,
    /// Body of the notification.
    pub message: String,
    /// Optional title; absent when the CLI omits it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Notification classification (e.g. `"permission_request"`).
    pub notification_type: String,
}

/// Input payload for `SubagentStart`. Ported from `types.py:331-335`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentStartInput {
    /// Shared hook context.
    #[serde(flatten)]
    pub base: BaseHookInput,
    /// Sub-agent identifier for this spawn.
    pub agent_id: String,
    /// Agent type (e.g. `"general-purpose"`).
    pub agent_type: String,
}

/// Input payload for `PermissionRequest`. Ported from `types.py:338-344`.
///
/// Observed when the CLI asks the SDK to confirm a tool invocation. This is
/// distinct from the `can_use_tool` `control_request` path — `PermissionRequest`
/// hooks are observational; actual allow/deny decisions still flow through
/// `can_use_tool`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRequestInput {
    /// Shared hook context.
    #[serde(flatten)]
    pub base: BaseHookInput,
    /// Optional sub-agent attribution.
    #[serde(flatten)]
    pub subagent: SubagentContext,
    /// Tool the CLI is asking about.
    pub tool_name: String,
    /// Proposed tool input.
    pub tool_input: Value,
    /// Optional list of permission-rule suggestions the CLI proposes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_suggestions: Option<Vec<Value>>,
}
