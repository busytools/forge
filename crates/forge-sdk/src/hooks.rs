//! Hook callbacks — 10 hook kinds dispatched by opaque `callback_id`.
//!
//! Mirrors Python SDK's `HookMatcher` / `HookContext` machinery. Callbacks
//! are registered at initialize time; the CLI emits `hook_callback`
//! `control_request`s with an opaque `callback_id` (minted by the SDK) plus
//! an `input` payload whose `hook_event_name` discriminates concrete types.
//!
//! Split across submodules for legibility:
//! - [`inputs`] — `BaseHookInput`, `SubagentContext`, and the ten `*Input`
//!   payload structs.
//! - [`outputs`] — per-event `*HookSpecificOutput` wrappers, the
//!   `hookEventName` tag ZSTs, and the [`HookSpecificOutput`] union.
//! - [`callback`] — [`HookCallback`], [`HookDecision`], and the type-erasure
//!   adapter that lets the dispatch loop call a typed callback from an
//!   opaque `callback_id`.
//! - [`registry`] — [`Hooks`] store, [`HooksBuilder`], and the internal
//!   registry that renders the `initialize` `control_request` payload.

pub mod callback;
pub mod inputs;
pub mod outputs;
pub mod registry;

pub use callback::{ErasedHookCallback, HookCallback, HookDecision};
pub use inputs::{
    BaseHookInput, NotificationInput, PermissionRequestInput, PostToolUseFailureInput,
    PostToolUseInput, PreCompactInput, PreToolUseInput, StopInput, SubagentContext,
    SubagentStartInput, SubagentStopInput, UserPromptSubmitInput,
};
pub use outputs::{
    HookSpecificOutput, NotificationHookSpecificOutput, PermissionRequestHookSpecificOutput,
    PostToolUseFailureHookSpecificOutput, PostToolUseHookSpecificOutput,
    PreToolUseHookSpecificOutput, PreToolUsePermissionDecision, SessionStartHookSpecificOutput,
    SubagentStartHookSpecificOutput, UserPromptSubmitHookSpecificOutput,
};
pub use registry::{Hooks, HooksBuilder};

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
