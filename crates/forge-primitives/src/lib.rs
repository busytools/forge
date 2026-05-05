//! `forge-primitives` — types-only crate shared across forge crates.
//!
//! The workspace base crate. Holds every wire-shape type that
//! crosses any forge-* crate boundary, with **no** logic, I/O, or
//! async. Other crates depend on this; this depends on nothing
//! forge-shaped.
//!
//! Module map:
//!
//! - [`command`] — `Command` enum (UI → agent channel envelope) + IDs.
//! - [`ids`] — `SessionId`, `ToolUseId`, `MessageId`.
//! - [`image`] — `ImageAttachment` + clipboard validation helpers.
//! - [`messages`] — top-level stream-json shapes (`Message`,
//!   `AssistantEnvelope`, `Usage`, `RateLimit*`, `StopReason`,
//!   task-lifecycle variants).
//! - [`content`] — wire-side `ContentBlock` (Text, Thinking, ToolUse,
//!   ToolResult, server-tool, Image).
//! - [`public_types`] — public wire types: `AccountInfo`,
//!   `McpServer*`, `McpStatusResponse`, `ContextUsage*`,
//!   `SDKSessionInfo`, `SessionMessage*`, `Sandbox*`, `SettingSource`,
//!   `StreamEvent`.
//! - [`hooks`] — hook event data (`HookKind`, `HookContext`, all 12
//!   `*Input` structs, all 9 `*HookSpecificOutput` types).
//! - [`permissions`] — permission decision data (`PermissionDecision`,
//!   `ToolPermissionContext`, `PermissionUpdate*`, `Permission*`).
//! - [`options`] — option-config enums shared between the SDK's
//!   `Options` builder and consumers (`PermissionMode`,
//!   `SystemPromptKind`, `ToolsPreset`, `ThinkingConfig`,
//!   `SdkPluginConfig`).
//! - [`subagents`] — `SubagentDefinition` + nested types.
//! - [`runtime`] — live runtime state: mode/model state, available
//!   commands/agents/models, rate-limit views, retry classification,
//!   terminal reasons.
//! - [`session_update`] — `SessionUpdate` + everything it embeds
//!   (chunks, tool calls, tool-call updates, plan entries, output
//!   metadata).
//! - [`permission_ui`] — UI-side permission-prompt request/response
//!   shapes (distinct from the wire-side decisions in
//!   [`permissions`]).
//! - [`question`] — `AskUserQuestion` request/response shapes.
//! - [`elicitation`] — MCP elicitation request/response (form / URL).
//! - [`mcp_view`] — MCP UI events (`McpAuthRedirect`,
//!   `McpOperationError`).
//! - [`session_meta`] — `SessionListEntry`, `PromptChunk`.
//!
//! Add a type here when 2+ forge crates need it. Never reach for
//! cross-crate `pub use` chains as a substitute.

pub mod command;
pub mod content;
pub mod elicitation;
pub mod hooks;
pub mod ids;
pub mod image;
pub mod mcp_view;
pub mod messages;
pub mod options;
pub mod permission_ui;
pub mod permissions;
pub mod public_types;
pub mod question;
pub mod runtime;
pub mod session_meta;
pub mod session_update;
pub mod subagents;

pub use command::Command;
pub use content::ContentBlock;
pub use elicitation::{
    ElicitationAction, ElicitationMode, ElicitationRequest, ElicitationResponse,
};
pub use hooks::{
    BaseHookInput, HookContext, HookKind, HookSpecificOutput, NotificationHookSpecificOutput,
    NotificationInput, PermissionRequestHookSpecificOutput, PermissionRequestInput,
    PostToolUseFailureHookSpecificOutput, PostToolUseFailureInput, PostToolUseHookSpecificOutput,
    PostToolUseInput, PreCompactInput, PreToolUseHookSpecificOutput, PreToolUseInput,
    PreToolUsePermissionDecision, SessionStartHookSpecificOutput, StopInput, SubagentContext,
    SubagentStartHookSpecificOutput, SubagentStartInput, SubagentStopInput,
    UserPromptSubmitHookSpecificOutput, UserPromptSubmitInput,
};
pub use ids::{MessageId, SessionId, ToolUseId};
pub use image::{
    ImageAttachment, SUPPORTED_IMAGE_MIME_TYPES, is_supported_image_type, is_valid_base64,
    validate_image,
};
pub use mcp_view::{McpAuthRedirect, McpOperationError};
pub use messages::{
    AssistantEnvelope, AssistantMessageError, Message, RateLimitInfo, RateLimitStatus,
    RateLimitType, StopReason, TaskNotificationStatus, TaskUsage, Usage, UserEnvelope,
};
pub use options::{PermissionMode, SdkPluginConfig, SystemPromptKind, ThinkingConfig, ToolsPreset};
pub use permission_ui::{
    PermissionDisplay, PermissionOption, PermissionOutcome, PermissionRequest,
};
pub use permissions::{
    PermissionBehavior, PermissionDecision, PermissionRuleValue, PermissionUpdate,
    PermissionUpdateDestination, ToolPermissionContext,
};
pub use public_types::{
    AccountInfo, ContextUsageCategory, ContextUsageResponse, McpServerConfig,
    McpServerConnectionStatus, McpServerInfo, McpServerStatus, McpStatusResponse,
    McpToolAnnotations, McpToolInfo, SDKSessionInfo, SandboxIgnoreViolations, SandboxNetworkConfig,
    SandboxSettings, SessionMessage, SessionMessageKind, SettingSource, StreamEvent,
};
pub use question::{
    QuestionAnnotation, QuestionOption, QuestionOutcome, QuestionPrompt, QuestionRequest,
};
pub use runtime::{
    ApiRetryError, ApiRetryUpdate, AvailableAgent, AvailableCommand, AvailableModel,
    CompactionTrigger, CurrentModel, EffortLevel, FastModeState, ModeInfo, ModeState,
    RateLimitUpdate, RuntimeSessionState, SessionStatus, SettingsParseErrorUpdate, TerminalReason,
};
pub use session_meta::{PromptChunk, SessionListEntry};
pub use session_update::{
    BashOutputMetadata, ChunkContent, PlanEntry, SessionUpdate, TaskMetadata,
    TodoWriteOutputMetadata, ToolCall, ToolCallContent, ToolCallUpdate, ToolCallUpdateFields,
    ToolLocation, ToolOutputMetadata,
};
pub use subagents::{EffortPreset, SubagentDefinition, SubagentMcpServerRef, SubagentMemory};
