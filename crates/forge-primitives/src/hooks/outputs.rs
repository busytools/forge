//! `hookSpecificOutput` typed wrappers for the two hook kinds forge
//! answers with an input override.
//!
//! Each carries a zero-sized `event_name` field that serde always emits
//! as the correct `hookEventName` string, so the discriminator is present
//! however the wrapper is serialised.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::HookKind;

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
declare_event_name_tag!(UserPromptSubmitTag, "UserPromptSubmit");

/// `hookSpecificOutput` shape for `PreToolUse` hook responses.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PreToolUseHookSpecificOutput {
    /// Fixed `"PreToolUse"` discriminator on the wire.
    #[serde(rename = "hookEventName", default)]
    pub event_name: PreToolUseTag,
    /// Substitute input the tool should run with instead of the proposed one.
    #[serde(default, rename = "updatedInput", skip_serializing_if = "Option::is_none")]
    pub updated_input: Option<Value>,
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
            // warn - there is no wire field to land a structured
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
    // Test-mod `use super::*;` brings the parent's full surface in; not every test consumes every item.
    #[allow(unused_imports)]
    use super::*;

    use crate::{PreToolUseHookSpecificOutput, UserPromptSubmitHookSpecificOutput};
    use serde_json::json;

    #[test]
    fn pre_tool_use_hook_specific_output_serialises_with_updated_input() {
        let out = PreToolUseHookSpecificOutput {
            updated_input: Some(json!({"command": "echo safe"})),
            ..Default::default()
        };
        let v = serde_json::to_value(&out).expect("serialise");
        assert_eq!(v["hookEventName"], "PreToolUse");
        assert_eq!(v["updatedInput"], json!({"command": "echo safe"}));
    }

    #[test]
    fn pre_tool_use_hook_specific_output_omits_unset_updated_input() {
        let v = serde_json::to_value(PreToolUseHookSpecificOutput::default()).expect("serialise");
        assert_eq!(v["hookEventName"], "PreToolUse");
        assert!(v.get("updatedInput").is_none(), "None-valued fields must not serialise");
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
}
