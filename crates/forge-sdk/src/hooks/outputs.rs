//! `hookSpecificOutput` typed wrappers — one per event kind.
//!
//! Mirrors claude-agent-sdk-python v0.1.64 `types.py:369-438`. Each event kind
//! has its own `*HookSpecificOutput` `TypedDict` upstream with a fixed
//! `hookEventName` discriminator plus event-specific optional fields. The
//! Rust structs carry a zero-sized `event_name` field that serde always
//! emits as the correct string — guaranteeing the discriminator is present
//! whether the wrapper is serialised standalone or via [`HookSpecificOutput`].

use serde::{Deserialize, Serialize};
use serde_json::Value;

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
