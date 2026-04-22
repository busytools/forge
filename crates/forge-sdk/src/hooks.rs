//! Hook callbacks — 10 hook kinds dispatched by opaque `callback_id`.
//!
//! Mirrors Python SDK's `HookMatcher` / `HookContext` machinery. Callbacks
//! are registered at initialize time; the CLI emits `hook_callback`
//! `control_request`s with an opaque `callback_id` (minted by the SDK) plus
//! an `input` payload whose `hook_event_name` discriminates concrete types.

use std::marker::PhantomData;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Identifies which hook point a callback is registered for. Ten event
/// kinds mirrored from Python SDK v0.1.64 (`types.py:216-227`), plus
/// `Unknown` as a fallback for forward-compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookKind {
    /// Before a tool is invoked.
    PreToolUse,
    /// After a tool completes successfully.
    PostToolUse,
    /// After a tool errors.
    PostToolUseFailure,
    /// Every user prompt (can rewrite or cancel).
    UserPromptSubmit,
    /// End of an assistant turn.
    Stop,
    /// End of a sub-agent turn.
    SubagentStop,
    /// Start of a sub-agent turn.
    SubagentStart,
    /// Before session compaction.
    PreCompact,
    /// Out-of-band notification to the caller.
    Notification,
    /// Permission request observation (distinct from `can_use_tool`).
    PermissionRequest,
    /// Fallback for hook events forge-sdk doesn't yet recognise.
    Unknown,
}

impl HookKind {
    /// Wire-name used by the `claude` binary.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::PostToolUseFailure => "PostToolUseFailure",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::Stop => "Stop",
            Self::SubagentStop => "SubagentStop",
            Self::SubagentStart => "SubagentStart",
            Self::PreCompact => "PreCompact",
            Self::Notification => "Notification",
            Self::PermissionRequest => "PermissionRequest",
            Self::Unknown => "Unknown",
        }
    }

    /// Parse a wire-name back into the enum. Unknown strings fall through
    /// to `HookKind::Unknown` — forge-sdk is forward-compatible with new
    /// hook types Anthropic introduces between our parity checks.
    #[must_use]
    pub fn from_wire(s: &str) -> Self {
        match s {
            "PreToolUse" => Self::PreToolUse,
            "PostToolUse" => Self::PostToolUse,
            "PostToolUseFailure" => Self::PostToolUseFailure,
            "UserPromptSubmit" => Self::UserPromptSubmit,
            "Stop" => Self::Stop,
            "SubagentStop" => Self::SubagentStop,
            "SubagentStart" => Self::SubagentStart,
            "PreCompact" => Self::PreCompact,
            "Notification" => Self::Notification,
            "PermissionRequest" => Self::PermissionRequest,
            _ => Self::Unknown,
        }
    }
}

/// Context carried alongside every hook invocation.
#[derive(Debug, Clone)]
pub struct HookContext {
    /// Hook point being invoked.
    pub kind: HookKind,
    /// Tool name when applicable (`PreToolUse` / `PostToolUse`).
    pub tool_name: Option<String>,
    /// Session id.
    pub session_id: String,
    /// Tool-use id when the hook fired in a tool-use context
    /// (`PreToolUse`, `PostToolUse`). `None` for other hook kinds.
    pub tool_use_id: Option<String>,
}

/// Fields shared by every hook input the CLI emits.
///
/// Ported from claude-agent-sdk-python v0.1.64 `src/claude_agent_sdk/types.py:231-237`
/// (`BaseHookInput` `TypedDict`). Every concrete input type below flattens
/// this struct so callbacks can access `session_id`, `transcript_path`,
/// `cwd`, and (optionally) `permission_mode` without reaching back into
/// [`HookContext`].
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

// ---------------------------------------------------------------------------
// hookSpecificOutput — per-event typed wrappers
//
// Mirrors claude-agent-sdk-python v0.1.64 `types.py:369-438`. Each event kind
// has its own `*HookSpecificOutput` `TypedDict` upstream with a fixed
// `hookEventName` discriminator plus event-specific optional fields. The
// Rust structs carry a zero-sized `event_name` field that serde always
// emits as the correct string — guaranteeing the discriminator is present
// whether the wrapper is serialised standalone or via [`HookSpecificOutput`].
// ---------------------------------------------------------------------------

/// Permission decision a `PreToolUse` hook can express. Mirrors Python's
/// `Literal["allow", "deny", "ask"]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PreToolUsePermissionDecision {
    /// Allow the tool invocation.
    Allow,
    /// Deny the tool invocation with a reason.
    Deny,
    /// Defer to the interactive permission prompt.
    Ask,
}

/// Tag ZST helpers that serialise as a fixed `hookEventName` string and
/// ignore the actual value on the way back in. One ZST per event kind keeps
/// the wrapper structs `Default`, `Clone`, and roundtrip-safe without
/// requiring nightly-only const-generic string parameters.
macro_rules! declare_event_name_tag {
    ($name:ident, $tag:literal) => {
        #[doc = concat!("Zero-sized tag that always serialises as `\"", $tag, "\"`.")]
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
        pub struct $name;

        impl Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
                ser.serialize_str($tag)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
                let _ = String::deserialize(de)?;
                Ok(Self)
            }
        }
    };
}

declare_event_name_tag!(PreToolUseTag, "PreToolUse");
declare_event_name_tag!(PostToolUseTag, "PostToolUse");
declare_event_name_tag!(PostToolUseFailureTag, "PostToolUseFailure");
declare_event_name_tag!(UserPromptSubmitTag, "UserPromptSubmit");
declare_event_name_tag!(SessionStartTag, "SessionStart");
declare_event_name_tag!(NotificationTag, "Notification");
declare_event_name_tag!(SubagentStartTag, "SubagentStart");
declare_event_name_tag!(PermissionRequestTag, "PermissionRequest");

/// `hookSpecificOutput` shape for `PreToolUse` hook responses.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PreToolUseHookSpecificOutput {
    /// Fixed `"PreToolUse"` discriminator on the wire.
    #[serde(rename = "hookEventName", default)]
    pub event_name: PreToolUseTag,
    /// Optional permission decision.
    #[serde(
        default,
        rename = "permissionDecision",
        skip_serializing_if = "Option::is_none"
    )]
    pub permission_decision: Option<PreToolUsePermissionDecision>,
    /// Human-readable reason attached to the permission decision.
    #[serde(
        default,
        rename = "permissionDecisionReason",
        skip_serializing_if = "Option::is_none"
    )]
    pub permission_decision_reason: Option<String>,
    /// Substitute input the tool should run with instead of the proposed one.
    #[serde(
        default,
        rename = "updatedInput",
        skip_serializing_if = "Option::is_none"
    )]
    pub updated_input: Option<Value>,
    /// Out-of-band context to inject into the session.
    #[serde(
        default,
        rename = "additionalContext",
        skip_serializing_if = "Option::is_none"
    )]
    pub additional_context: Option<String>,
}

/// `hookSpecificOutput` shape for `PostToolUse`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PostToolUseHookSpecificOutput {
    /// Fixed `"PostToolUse"` discriminator.
    #[serde(rename = "hookEventName", default)]
    pub event_name: PostToolUseTag,
    /// Out-of-band context to inject.
    #[serde(
        default,
        rename = "additionalContext",
        skip_serializing_if = "Option::is_none"
    )]
    pub additional_context: Option<String>,
    /// Replacement MCP-tool output (for in-process MCP servers).
    #[serde(
        default,
        rename = "updatedMCPToolOutput",
        skip_serializing_if = "Option::is_none"
    )]
    pub updated_mcp_tool_output: Option<Value>,
}

/// `hookSpecificOutput` shape for `PostToolUseFailure`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PostToolUseFailureHookSpecificOutput {
    /// Fixed `"PostToolUseFailure"` discriminator.
    #[serde(rename = "hookEventName", default)]
    pub event_name: PostToolUseFailureTag,
    /// Out-of-band context to inject.
    #[serde(
        default,
        rename = "additionalContext",
        skip_serializing_if = "Option::is_none"
    )]
    pub additional_context: Option<String>,
}

/// `hookSpecificOutput` shape for `UserPromptSubmit`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserPromptSubmitHookSpecificOutput {
    /// Fixed `"UserPromptSubmit"` discriminator.
    #[serde(rename = "hookEventName", default)]
    pub event_name: UserPromptSubmitTag,
    /// Out-of-band context to inject alongside the submitted prompt.
    #[serde(
        default,
        rename = "additionalContext",
        skip_serializing_if = "Option::is_none"
    )]
    pub additional_context: Option<String>,
}

/// `hookSpecificOutput` shape for `SessionStart`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionStartHookSpecificOutput {
    /// Fixed `"SessionStart"` discriminator.
    #[serde(rename = "hookEventName", default)]
    pub event_name: SessionStartTag,
    /// Out-of-band context to inject at session start.
    #[serde(
        default,
        rename = "additionalContext",
        skip_serializing_if = "Option::is_none"
    )]
    pub additional_context: Option<String>,
}

/// `hookSpecificOutput` shape for `Notification`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NotificationHookSpecificOutput {
    /// Fixed `"Notification"` discriminator.
    #[serde(rename = "hookEventName", default)]
    pub event_name: NotificationTag,
    /// Out-of-band context to inject when reacting to a notification.
    #[serde(
        default,
        rename = "additionalContext",
        skip_serializing_if = "Option::is_none"
    )]
    pub additional_context: Option<String>,
}

/// `hookSpecificOutput` shape for `SubagentStart`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubagentStartHookSpecificOutput {
    /// Fixed `"SubagentStart"` discriminator.
    #[serde(rename = "hookEventName", default)]
    pub event_name: SubagentStartTag,
    /// Out-of-band context to inject when a sub-agent starts.
    #[serde(
        default,
        rename = "additionalContext",
        skip_serializing_if = "Option::is_none"
    )]
    pub additional_context: Option<String>,
}

/// `hookSpecificOutput` shape for `PermissionRequest`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PermissionRequestHookSpecificOutput {
    /// Fixed `"PermissionRequest"` discriminator.
    #[serde(rename = "hookEventName", default)]
    pub event_name: PermissionRequestTag,
    /// Raw decision payload surfaced upstream — the CLI treats this as a
    /// callback-scoped object of rules/behaviors. `Value::Null` when unset.
    #[serde(default)]
    pub decision: Value,
}

/// Tagged union over every typed `hookSpecificOutput` shape. Uses serde's
/// untagged representation — each variant's inner struct already carries
/// its own `hookEventName` discriminator, so probing by `hookEventName` is
/// the right way to decide the variant on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HookSpecificOutput {
    /// `PreToolUse` event output.
    PreToolUse(PreToolUseHookSpecificOutput),
    /// `PostToolUse` event output.
    PostToolUse(PostToolUseHookSpecificOutput),
    /// `PostToolUseFailure` event output.
    PostToolUseFailure(PostToolUseFailureHookSpecificOutput),
    /// `UserPromptSubmit` event output.
    UserPromptSubmit(UserPromptSubmitHookSpecificOutput),
    /// `SessionStart` event output.
    SessionStart(SessionStartHookSpecificOutput),
    /// `Notification` event output.
    Notification(NotificationHookSpecificOutput),
    /// `SubagentStart` event output.
    SubagentStart(SubagentStartHookSpecificOutput),
    /// `PermissionRequest` event output.
    PermissionRequest(PermissionRequestHookSpecificOutput),
}

/// A hook decision.
#[derive(Debug, Clone)]
pub struct HookDecision {
    inner: HookDecisionKind,
}

#[derive(Debug, Clone)]
enum HookDecisionKind {
    Allow {
        updated_input: Option<Value>,
    },
    Deny {
        reason: String,
    },
    /// No-op — purely observational; continue unchanged.
    Passthrough,
}

impl HookDecision {
    /// Allow the action unchanged.
    #[must_use]
    pub fn allow() -> Self {
        Self {
            inner: HookDecisionKind::Allow {
                updated_input: None,
            },
        }
    }

    /// Allow but substitute a new input payload (`PreToolUse` /
    /// `UserPromptSubmit`).
    #[must_use]
    pub fn replace_input(new_input: Value) -> Self {
        Self {
            inner: HookDecisionKind::Allow {
                updated_input: Some(new_input),
            },
        }
    }

    /// Deny the action with a reason string.
    #[must_use]
    pub fn deny(reason: impl Into<String>) -> Self {
        Self {
            inner: HookDecisionKind::Deny {
                reason: reason.into(),
            },
        }
    }

    /// Observational only — continue unchanged (typical `PostToolUse` /
    /// `Stop`).
    #[must_use]
    pub fn passthrough() -> Self {
        Self {
            inner: HookDecisionKind::Passthrough,
        }
    }

    /// True if the decision allows the action.
    #[must_use]
    pub fn is_allow(&self) -> bool {
        !matches!(self.inner, HookDecisionKind::Deny { .. })
    }

    /// Optional modified input.
    #[must_use]
    pub fn updated_input(&self) -> Option<&Value> {
        match &self.inner {
            HookDecisionKind::Allow { updated_input } => updated_input.as_ref(),
            _ => None,
        }
    }

    /// Optional deny reason.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match &self.inner {
            HookDecisionKind::Deny { reason } => Some(reason),
            _ => None,
        }
    }
}

/// Trait for hook callbacks. Each hook kind has its own concrete callback;
/// the trait is parameterised over the input type.
pub trait HookCallback<I>: Send + Sync
where
    I: Send + 'static,
{
    /// Called when the matching hook fires.
    fn call<'a>(
        &'a self,
        input: I,
        context: HookContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = HookDecision> + Send + 'a>>;
}

impl<F, Fut, I> HookCallback<I> for F
where
    F: Fn(I, HookContext) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = HookDecision> + Send + 'static,
    I: Send + 'static,
{
    fn call<'a>(
        &'a self,
        input: I,
        context: HookContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = HookDecision> + Send + 'a>> {
        Box::pin(self(input, context))
    }
}

/// Type-erased hook callback. The concrete input type is rebuilt at
/// dispatch time from `input.hook_event_name`.
#[async_trait::async_trait]
pub trait ErasedHookCallback: Send + Sync {
    /// Deserialise `input` into the concrete input type and invoke the
    /// wrapped callback.
    async fn call_erased(&self, input: Value, context: HookContext) -> HookDecision;
}

/// Adapter so any typed [`HookCallback<I>`] implements
/// [`ErasedHookCallback`] when `I: DeserializeOwned`.
pub(crate) struct ErasedCallback<I, C>
where
    I: serde::de::DeserializeOwned + Send + 'static,
    C: HookCallback<I>,
{
    pub(crate) inner: C,
    pub(crate) _marker: PhantomData<fn() -> I>,
}

#[async_trait::async_trait]
impl<I, C> ErasedHookCallback for ErasedCallback<I, C>
where
    I: serde::de::DeserializeOwned + Send + 'static,
    C: HookCallback<I>,
{
    async fn call_erased(&self, input: Value, context: HookContext) -> HookDecision {
        match serde_json::from_value::<I>(input) {
            Ok(typed) => self.inner.call(typed, context).await,
            Err(e) => {
                // Security-permissive passthrough would silently skip the
                // caller's hook logic. Log prominently so a CLI schema drift
                // doesn't invisibly bypass the user's policy.
                tracing::warn!(
                    error = %e,
                    hook_kind = ?context.kind,
                    "hook input deserialise failed; passthrough (hook not consulted). CLI schema drift?"
                );
                HookDecision::passthrough()
            }
        }
    }
}

/// Registry of hook callbacks. Construct with [`HooksBuilder`], attach to
/// `OptionsBuilder` via `.hooks(...)`.
#[derive(Default, Clone)]
pub struct Hooks {
    pub(crate) pre_tool_use: Vec<(String, Arc<dyn ErasedHookCallback>)>,
    pub(crate) post_tool_use: Vec<(String, Arc<dyn ErasedHookCallback>)>,
    pub(crate) post_tool_use_failure: Vec<(String, Arc<dyn ErasedHookCallback>)>,
    pub(crate) user_prompt_submit: Vec<Arc<dyn ErasedHookCallback>>,
    pub(crate) stop: Vec<Arc<dyn ErasedHookCallback>>,
    pub(crate) subagent_stop: Vec<Arc<dyn ErasedHookCallback>>,
    pub(crate) subagent_start: Vec<Arc<dyn ErasedHookCallback>>,
    pub(crate) pre_compact: Vec<Arc<dyn ErasedHookCallback>>,
    pub(crate) notification: Vec<Arc<dyn ErasedHookCallback>>,
    pub(crate) permission_request: Vec<(String, Arc<dyn ErasedHookCallback>)>,
}

impl std::fmt::Debug for Hooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hooks")
            .field("pre_tool_use_count", &self.pre_tool_use.len())
            .field("post_tool_use_count", &self.post_tool_use.len())
            .field(
                "post_tool_use_failure_count",
                &self.post_tool_use_failure.len(),
            )
            .field("user_prompt_submit_count", &self.user_prompt_submit.len())
            .field("stop_count", &self.stop.len())
            .field("subagent_stop_count", &self.subagent_stop.len())
            .field("subagent_start_count", &self.subagent_start.len())
            .field("pre_compact_count", &self.pre_compact.len())
            .field("notification_count", &self.notification.len())
            .field("permission_request_count", &self.permission_request.len())
            .finish()
    }
}

impl Hooks {
    /// Mint opaque `callback_id`s (e.g. `hook_0`, `hook_1`, …) for every
    /// registered callback and return them bundled for dispatch-time
    /// lookup. Returns a map `id → callback` plus a parallel metadata
    /// vector describing which event/matcher each id belongs to (used to
    /// populate the initialize `control_request` payload).
    pub(crate) fn mint_registry(&self) -> HookRegistry {
        let mut registry = HookRegistry::default();
        let mut counter: u64 = 0;

        let mut mint =
            |kind: HookKind, matcher: Option<String>, cb: Arc<dyn ErasedHookCallback>| {
                let id = format!("hook_{counter}");
                counter += 1;
                registry.metadata.push(HookRegistryEntry {
                    id: id.clone(),
                    kind,
                    matcher,
                });
                registry.by_id.insert(id, cb);
            };

        for (matcher, cb) in &self.pre_tool_use {
            mint(HookKind::PreToolUse, Some(matcher.clone()), cb.clone());
        }
        for (matcher, cb) in &self.post_tool_use {
            mint(HookKind::PostToolUse, Some(matcher.clone()), cb.clone());
        }
        for (matcher, cb) in &self.post_tool_use_failure {
            mint(
                HookKind::PostToolUseFailure,
                Some(matcher.clone()),
                cb.clone(),
            );
        }
        for cb in &self.user_prompt_submit {
            mint(HookKind::UserPromptSubmit, None, cb.clone());
        }
        for cb in &self.stop {
            mint(HookKind::Stop, None, cb.clone());
        }
        for cb in &self.subagent_stop {
            mint(HookKind::SubagentStop, None, cb.clone());
        }
        for cb in &self.subagent_start {
            mint(HookKind::SubagentStart, None, cb.clone());
        }
        for cb in &self.pre_compact {
            mint(HookKind::PreCompact, None, cb.clone());
        }
        for cb in &self.notification {
            mint(HookKind::Notification, None, cb.clone());
        }
        for (matcher, cb) in &self.permission_request {
            mint(
                HookKind::PermissionRequest,
                Some(matcher.clone()),
                cb.clone(),
            );
        }

        registry
    }

    /// Render the `hooks` key of the `initialize` `control_request` payload
    /// exactly as the Client will send it. Test-only surface — production
    /// code uses this indirectly through the Client's initialize path.
    #[doc(hidden)]
    #[must_use]
    pub fn to_initialize_payload_for_test(&self) -> serde_json::Value {
        self.mint_registry().to_initialize_payload()
    }
}

/// Internal bundle mapping opaque ids to erased callbacks, with parallel
/// metadata for the initialize payload.
#[derive(Default)]
pub(crate) struct HookRegistry {
    pub(crate) by_id: std::collections::HashMap<String, Arc<dyn ErasedHookCallback>>,
    pub(crate) metadata: Vec<HookRegistryEntry>,
}

impl HookRegistry {
    /// Render the `hooks` field of the `initialize` `control_request`:
    /// `{"PreToolUse": [{"matcher": "...", "hookCallbackIds": ["hook_0"], "timeout": 30}, ...], ...}`.
    pub(crate) fn to_initialize_payload(&self) -> serde_json::Value {
        use std::collections::BTreeMap;

        // Group ids by (kind, matcher). BTreeMap for deterministic output.
        let mut by_kind: BTreeMap<&'static str, BTreeMap<String, Vec<String>>> = BTreeMap::new();
        for entry in &self.metadata {
            let kind_name = entry.kind.as_str();
            let matcher_key = entry.matcher.clone().unwrap_or_default();
            by_kind
                .entry(kind_name)
                .or_default()
                .entry(matcher_key)
                .or_default()
                .push(entry.id.clone());
        }

        let mut map = serde_json::Map::new();
        for (kind_name, matcher_group) in by_kind {
            let specs: Vec<serde_json::Value> = matcher_group
                .into_iter()
                .map(|(matcher, ids)| {
                    let mut spec = serde_json::Map::new();
                    if !matcher.is_empty() {
                        spec.insert("matcher".into(), serde_json::Value::String(matcher));
                    }
                    spec.insert(
                        "hookCallbackIds".into(),
                        serde_json::Value::Array(ids.into_iter().map(Into::into).collect()),
                    );
                    spec.insert("timeout".into(), serde_json::json!(30));
                    serde_json::Value::Object(spec)
                })
                .collect();
            map.insert(kind_name.into(), serde_json::Value::Array(specs));
        }
        serde_json::Value::Object(map)
    }
}

/// One entry describing a minted hook id.
#[derive(Debug, Clone)]
pub(crate) struct HookRegistryEntry {
    pub(crate) id: String,
    pub(crate) kind: HookKind,
    pub(crate) matcher: Option<String>,
}

/// Builder for [`Hooks`].
#[derive(Default)]
pub struct HooksBuilder {
    inner: Hooks,
}

impl std::fmt::Debug for HooksBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HooksBuilder")
            .field("inner", &self.inner)
            .finish()
    }
}

impl HooksBuilder {
    /// Start empty.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a `PreToolUse` hook. `matcher` is a glob against tool names
    /// (`"*"` matches all); pass `"Bash"` to match only the Bash tool.
    #[must_use]
    pub fn pre_tool_use<C>(mut self, matcher: impl Into<String>, callback: C) -> Self
    where
        C: HookCallback<PreToolUseInput> + 'static,
    {
        self.inner.pre_tool_use.push((
            matcher.into(),
            Arc::new(ErasedCallback::<PreToolUseInput, C> {
                inner: callback,
                _marker: PhantomData,
            }),
        ));
        self
    }

    /// Register a `PostToolUse` hook.
    #[must_use]
    pub fn post_tool_use<C>(mut self, matcher: impl Into<String>, callback: C) -> Self
    where
        C: HookCallback<PostToolUseInput> + 'static,
    {
        self.inner.post_tool_use.push((
            matcher.into(),
            Arc::new(ErasedCallback::<PostToolUseInput, C> {
                inner: callback,
                _marker: PhantomData,
            }),
        ));
        self
    }

    /// Register a `UserPromptSubmit` hook.
    #[must_use]
    pub fn user_prompt_submit<C>(mut self, callback: C) -> Self
    where
        C: HookCallback<UserPromptSubmitInput> + 'static,
    {
        self.inner
            .user_prompt_submit
            .push(Arc::new(ErasedCallback::<UserPromptSubmitInput, C> {
                inner: callback,
                _marker: PhantomData,
            }));
        self
    }

    /// Register a `Stop` hook.
    #[must_use]
    pub fn stop<C>(mut self, callback: C) -> Self
    where
        C: HookCallback<StopInput> + 'static,
    {
        self.inner
            .stop
            .push(Arc::new(ErasedCallback::<StopInput, C> {
                inner: callback,
                _marker: PhantomData,
            }));
        self
    }

    /// Register a `SubagentStop` hook.
    #[must_use]
    pub fn subagent_stop<C>(mut self, callback: C) -> Self
    where
        C: HookCallback<SubagentStopInput> + 'static,
    {
        self.inner
            .subagent_stop
            .push(Arc::new(ErasedCallback::<SubagentStopInput, C> {
                inner: callback,
                _marker: PhantomData,
            }));
        self
    }

    /// Register a `PreCompact` hook.
    #[must_use]
    pub fn pre_compact<C>(mut self, callback: C) -> Self
    where
        C: HookCallback<PreCompactInput> + 'static,
    {
        self.inner
            .pre_compact
            .push(Arc::new(ErasedCallback::<PreCompactInput, C> {
                inner: callback,
                _marker: PhantomData,
            }));
        self
    }

    /// Register a `PostToolUseFailure` hook. `matcher` follows the same
    /// tool-name glob semantics as [`Self::pre_tool_use`] / [`Self::post_tool_use`].
    #[must_use]
    pub fn post_tool_use_failure<C>(mut self, matcher: impl Into<String>, callback: C) -> Self
    where
        C: HookCallback<PostToolUseFailureInput> + 'static,
    {
        self.inner.post_tool_use_failure.push((
            matcher.into(),
            Arc::new(ErasedCallback::<PostToolUseFailureInput, C> {
                inner: callback,
                _marker: PhantomData,
            }),
        ));
        self
    }

    /// Register a `Notification` hook.
    #[must_use]
    pub fn notification<C>(mut self, callback: C) -> Self
    where
        C: HookCallback<NotificationInput> + 'static,
    {
        self.inner
            .notification
            .push(Arc::new(ErasedCallback::<NotificationInput, C> {
                inner: callback,
                _marker: PhantomData,
            }));
        self
    }

    /// Register a `SubagentStart` hook.
    #[must_use]
    pub fn subagent_start<C>(mut self, callback: C) -> Self
    where
        C: HookCallback<SubagentStartInput> + 'static,
    {
        self.inner
            .subagent_start
            .push(Arc::new(ErasedCallback::<SubagentStartInput, C> {
                inner: callback,
                _marker: PhantomData,
            }));
        self
    }

    /// Register a `PermissionRequest` hook (observational; `matcher` globs
    /// against tool names the same way as [`Self::pre_tool_use`]).
    #[must_use]
    pub fn permission_request<C>(mut self, matcher: impl Into<String>, callback: C) -> Self
    where
        C: HookCallback<PermissionRequestInput> + 'static,
    {
        self.inner.permission_request.push((
            matcher.into(),
            Arc::new(ErasedCallback::<PermissionRequestInput, C> {
                inner: callback,
                _marker: PhantomData,
            }),
        ));
        self
    }

    /// Finalise.
    #[must_use]
    pub fn build(self) -> Hooks {
        self.inner
    }
}
