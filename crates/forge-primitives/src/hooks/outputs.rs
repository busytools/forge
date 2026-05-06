//! `hookSpecificOutput` typed wrappers — one per event kind.
//!
//! Mirrors  Each event kind
//! has its own `*HookSpecificOutput` `TypedDict` upstream with a fixed
//! `hookEventName` discriminator plus event-specific optional fields. The
//! Rust structs carry a zero-sized `event_name` field that serde always
//! emits as the correct string — guaranteeing the discriminator is present
//! whether the wrapper is serialised standalone or via [`HookSpecificOutput`].

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::HookKind;

/// Permission decision a `PreToolUse` hook can express. Wire shape:
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
    #[serde(default, rename = "permissionDecision", skip_serializing_if = "Option::is_none")]
    pub permission_decision: Option<PreToolUsePermissionDecision>,
    /// Human-readable reason attached to the permission decision.
    #[serde(default, rename = "permissionDecisionReason", skip_serializing_if = "Option::is_none")]
    pub permission_decision_reason: Option<String>,
    /// Substitute input the tool should run with instead of the proposed one.
    #[serde(default, rename = "updatedInput", skip_serializing_if = "Option::is_none")]
    pub updated_input: Option<Value>,
    /// Out-of-band context to inject into the session.
    #[serde(default, rename = "additionalContext", skip_serializing_if = "Option::is_none")]
    pub additional_context: Option<String>,
}

/// `hookSpecificOutput` shape for `PostToolUse`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PostToolUseHookSpecificOutput {
    /// Fixed `"PostToolUse"` discriminator.
    #[serde(rename = "hookEventName", default)]
    pub event_name: PostToolUseTag,
    /// Out-of-band context to inject.
    #[serde(default, rename = "additionalContext", skip_serializing_if = "Option::is_none")]
    pub additional_context: Option<String>,
    /// Replacement MCP-tool output (for in-process MCP servers).
    #[serde(default, rename = "updatedMCPToolOutput", skip_serializing_if = "Option::is_none")]
    pub updated_mcp_tool_output: Option<Value>,
}

/// `hookSpecificOutput` shape for `PostToolUseFailure`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PostToolUseFailureHookSpecificOutput {
    /// Fixed `"PostToolUseFailure"` discriminator.
    #[serde(rename = "hookEventName", default)]
    pub event_name: PostToolUseFailureTag,
    /// Out-of-band context to inject.
    #[serde(default, rename = "additionalContext", skip_serializing_if = "Option::is_none")]
    pub additional_context: Option<String>,
}

/// `hookSpecificOutput` shape for `UserPromptSubmit`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserPromptSubmitHookSpecificOutput {
    /// Fixed `"UserPromptSubmit"` discriminator.
    #[serde(rename = "hookEventName", default)]
    pub event_name: UserPromptSubmitTag,
    /// Out-of-band context to inject alongside the submitted prompt.
    #[serde(default, rename = "additionalContext", skip_serializing_if = "Option::is_none")]
    pub additional_context: Option<String>,
}

/// `hookSpecificOutput` shape for `SessionStart`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionStartHookSpecificOutput {
    /// Fixed `"SessionStart"` discriminator.
    #[serde(rename = "hookEventName", default)]
    pub event_name: SessionStartTag,
    /// Out-of-band context to inject at session start.
    #[serde(default, rename = "additionalContext", skip_serializing_if = "Option::is_none")]
    pub additional_context: Option<String>,
}

/// `hookSpecificOutput` shape for `Notification`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NotificationHookSpecificOutput {
    /// Fixed `"Notification"` discriminator.
    #[serde(rename = "hookEventName", default)]
    pub event_name: NotificationTag,
    /// Out-of-band context to inject when reacting to a notification.
    #[serde(default, rename = "additionalContext", skip_serializing_if = "Option::is_none")]
    pub additional_context: Option<String>,
}

/// `hookSpecificOutput` shape for `SubagentStart`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubagentStartHookSpecificOutput {
    /// Fixed `"SubagentStart"` discriminator.
    #[serde(rename = "hookEventName", default)]
    pub event_name: SubagentStartTag,
    /// Out-of-band context to inject when a sub-agent starts.
    #[serde(default, rename = "additionalContext", skip_serializing_if = "Option::is_none")]
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
///
/// **Note.** forge-sdk's internal write path constructs the concrete
/// wrapper structs (`PreToolUseHookSpecificOutput` /
/// `UserPromptSubmitHookSpecificOutput`) directly rather than pattern-
/// matching on this enum — the dispatch lives in the callback handler.
/// `HookSpecificOutput` is carried here purely for caller ergonomics:
/// consumers that want to construct or inspect a response by event name
/// have a typed union they can match on without reinventing the
/// discriminator logic.
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

/// Encode a `replace_input` `forge_sdk::HookDecision` into a
/// `hookSpecificOutput` wrapper for the given hook kind.
///
/// Returns `None` when the hook kind has no wire field for an input
/// override (anything other than `PreToolUse` / `UserPromptSubmit`) or
/// when the supplied value doesn't match the wrapper's expected shape
/// (e.g. a non-string payload for `UserPromptSubmit`, whose wire struct
/// only carries `additionalContext: str`). Emits a `tracing::warn!` in
/// every drop path so misuse is visible rather than silent.
pub fn encode_updated_input_wrapper(kind: HookKind, updated: &Value) -> Option<Value> {
    match kind {
        HookKind::PreToolUse => {
            let typed = PreToolUseHookSpecificOutput {
                updated_input: Some(updated.clone()),
                ..Default::default()
            };
            serde_json::to_value(typed)
                .map_err(|e| {
                    tracing::warn!(
                        ?kind,
                        error = %e,
                        "PreToolUse hookSpecificOutput serialise failed; \
                         dropping updated_input"
                    );
                })
                .ok()
        }
        HookKind::UserPromptSubmit => {
            // Upstream `UserPromptSubmitHookSpecificOutput` only carries
            // `additionalContext: str`. If the caller hands us a JSON
            // string, forward it there; otherwise drop the payload and
            // warn — there is no wire field to land a structured
            // replacement.
            updated.as_str().map_or_else(
                || {
                    tracing::warn!(
                        ?kind,
                        "UserPromptSubmit replace_input expects a JSON string \
                         (used as additionalContext); dropping non-string payload"
                    );
                    None
                },
                |s| {
                    let typed = UserPromptSubmitHookSpecificOutput {
                        additional_context: Some(s.to_string()),
                        ..Default::default()
                    };
                    serde_json::to_value(typed)
                        .map_err(|e| {
                            tracing::warn!(
                                ?kind,
                                error = %e,
                                "UserPromptSubmit hookSpecificOutput serialise failed; \
                                 dropping updated_input"
                            );
                        })
                        .ok()
                },
            )
        }
        _ => {
            tracing::warn!(
                ?kind,
                "hook returned updated_input but hook kind doesn't support it; ignoring"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests_hooks_specific_output {
    #[allow(unused_imports)]
    use super::*;

    use crate::{
        HookSpecificOutput, NotificationHookSpecificOutput, PermissionRequestHookSpecificOutput,
        PostToolUseFailureHookSpecificOutput, PostToolUseHookSpecificOutput,
        PreToolUseHookSpecificOutput, PreToolUsePermissionDecision, SessionStartHookSpecificOutput,
        SubagentStartHookSpecificOutput, UserPromptSubmitHookSpecificOutput,
    };
    use serde_json::json;

    #[test]
    fn pre_tool_use_hook_specific_output_serialises_with_updated_input() {
        let out = PreToolUseHookSpecificOutput {
            permission_decision: Some(PreToolUsePermissionDecision::Allow),
            permission_decision_reason: Some("matched allowlist".into()),
            updated_input: Some(json!({"command": "echo safe"})),
            additional_context: None,
            ..Default::default()
        };
        let v = serde_json::to_value(&out).expect("serialise");
        assert_eq!(v["hookEventName"], "PreToolUse");
        assert_eq!(v["permissionDecision"], "allow");
        assert_eq!(v["permissionDecisionReason"], "matched allowlist");
        assert_eq!(v["updatedInput"], json!({"command": "echo safe"}));
        assert!(v.get("additionalContext").is_none(), "None-valued fields must not serialise");
    }

    #[test]
    fn pre_tool_use_hook_specific_output_permission_decision_enum_encodings() {
        for (variant, wire) in [
            (PreToolUsePermissionDecision::Allow, "allow"),
            (PreToolUsePermissionDecision::Deny, "deny"),
            (PreToolUsePermissionDecision::Ask, "ask"),
        ] {
            let out = PreToolUseHookSpecificOutput {
                permission_decision: Some(variant),
                ..Default::default()
            };
            let v = serde_json::to_value(&out).expect("serialise");
            assert_eq!(v["permissionDecision"], wire);
        }
    }

    #[test]
    fn post_tool_use_hook_specific_output_round_trips() {
        let out = PostToolUseHookSpecificOutput {
            additional_context: Some("ran ok".into()),
            updated_mcp_tool_output: Some(json!({"stdout": "rewritten"})),
            ..Default::default()
        };
        let v = serde_json::to_value(&out).expect("serialise");
        assert_eq!(v["hookEventName"], "PostToolUse");
        assert_eq!(v["additionalContext"], "ran ok");
        assert_eq!(v["updatedMCPToolOutput"], json!({"stdout": "rewritten"}));
    }

    #[test]
    fn post_tool_use_failure_hook_specific_output_minimal() {
        let out =
            PostToolUseFailureHookSpecificOutput { additional_context: None, ..Default::default() };
        let v = serde_json::to_value(&out).expect("serialise");
        assert_eq!(
            v,
            json!({"hookEventName": "PostToolUseFailure"}),
            "bare form should only carry the discriminator"
        );
    }

    #[test]
    fn user_prompt_submit_hook_specific_output_round_trips() {
        let out = UserPromptSubmitHookSpecificOutput {
            additional_context: Some("context to inject".into()),
            ..Default::default()
        };
        let v = serde_json::to_value(&out).expect("serialise");
        assert_eq!(v["hookEventName"], "UserPromptSubmit");
        assert_eq!(v["additionalContext"], "context to inject");
    }

    #[test]
    fn session_start_hook_specific_output_discriminator_only() {
        let out = SessionStartHookSpecificOutput { additional_context: None, ..Default::default() };
        let v = serde_json::to_value(&out).expect("serialise");
        assert_eq!(v, json!({"hookEventName": "SessionStart"}));
    }

    #[test]
    fn notification_hook_specific_output_round_trips() {
        let out = NotificationHookSpecificOutput {
            additional_context: Some("heads-up".into()),
            ..Default::default()
        };
        let v = serde_json::to_value(&out).expect("serialise");
        assert_eq!(v["hookEventName"], "Notification");
        assert_eq!(v["additionalContext"], "heads-up");
    }

    #[test]
    fn subagent_start_hook_specific_output_round_trips() {
        let out = SubagentStartHookSpecificOutput {
            additional_context: Some("agent starting".into()),
            ..Default::default()
        };
        let v = serde_json::to_value(&out).expect("serialise");
        assert_eq!(v["hookEventName"], "SubagentStart");
        assert_eq!(v["additionalContext"], "agent starting");
    }

    #[test]
    fn permission_request_hook_specific_output_carries_decision() {
        let out = PermissionRequestHookSpecificOutput {
            decision: json!({"behavior": "allow"}),
            ..Default::default()
        };
        let v = serde_json::to_value(&out).expect("serialise");
        assert_eq!(v["hookEventName"], "PermissionRequest");
        assert_eq!(v["decision"], json!({"behavior": "allow"}));
    }

    #[test]
    fn hook_specific_output_enum_tags_by_event_name() {
        let pre = HookSpecificOutput::PreToolUse(PreToolUseHookSpecificOutput {
            permission_decision: Some(PreToolUsePermissionDecision::Deny),
            permission_decision_reason: Some("nope".into()),
            updated_input: None,
            additional_context: None,
            ..Default::default()
        });
        let v = serde_json::to_value(&pre).expect("serialise");
        // The untagged enum forwards to the inner struct's own discriminator.
        assert_eq!(v["hookEventName"], "PreToolUse");
        assert_eq!(v["permissionDecision"], "deny");

        let noti = HookSpecificOutput::Notification(NotificationHookSpecificOutput {
            additional_context: None,
            ..Default::default()
        });
        assert_eq!(
            serde_json::to_value(&noti).expect("serialise")["hookEventName"],
            "Notification"
        );
    }
}
