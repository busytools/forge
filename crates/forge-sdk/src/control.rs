//! Control-protocol messages exchanged over the main stream-json stdio channel.
//!
//! These carry out-of-band requests that the SDK must answer synchronously,
//! such as permission checks (`can_use_tool`). See `docs/protocol-notes.md`
//! for the observed the CLI wire shapes we mirror here.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A control request emitted by the `claude` binary. The SDK must respond
/// with a matching [`ControlResponse`] carrying the same `request_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlRequest {
    /// Fixed discriminant `"control_request"`.
    #[serde(rename = "type")]
    pub ty: ControlRequestType,
    /// Opaque id correlating request to response.
    pub request_id: String,
    /// The request body.
    pub request: ControlRequestKind,
}

/// The discriminant type for [`ControlRequest`]. Always `"control_request"`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ControlRequestType {
    /// The only current variant.
    ControlRequest,
}

/// Enumerates the kinds of control requests we handle.
///
/// `Unknown` is the forward-compat catch-all — any `subtype` the CLI
/// ships that forge-sdk doesn't yet model lands here with the full
/// payload captured. The dispatcher in `client/control_dispatch.rs`
/// responds with a `control_response` error (since we have no handler)
/// and logs a `tracing::warn!` so callers notice drift. Unlike the
/// previous strict-enum behaviour, an unknown subtype no longer
/// panics the session.
#[derive(Debug, Clone)]
pub enum ControlRequestKind {
    /// A permission check for a tool call.
    CanUseTool {
        /// Tool name the model wants to invoke.
        tool_name: String,
        /// The JSON input the model generated.
        input: Value,
        /// Permission-suggestion hints from the binary (may be empty).
        permission_suggestions: Vec<Value>,
        /// Path blocked by workspace sandboxing, when applicable.
        blocked_path: Option<String>,
        /// Identifier of this tool-use (required; string).
        tool_use_id: String,
        /// Agent identifier when this is a sub-agent request.
        agent_id: Option<String>,
        /// Free-form reason the CLI surfaced for why human review is
        /// needed (e.g. `"workspace not yet trusted"`).
        decision_reason: Option<String>,
        /// Short title the CLI suggests for the prompt
        /// (e.g. `"Run tests"`).
        title: Option<String>,
        /// Display name for the tool call (often a humanised tool name).
        display_name: Option<String>,
        /// Long-form description the CLI surfaces in the prompt body.
        description: Option<String>,
    },
    /// MCP JSON-RPC message routed in-process. See `mcp` module for the MCP
    /// routing pipeline.
    McpMessage {
        /// Name of the in-process MCP server this message is addressed to
        /// (matches `mcpServers` config key).
        server_name: String,
        /// The JSON-RPC request body (initialize / tools/list / tools/call /
        /// notifications/initialized).
        message: Value,
    },
    /// Hook callback invocation. `callback_id` was registered at connect
    /// time via the `initialize` `control_request`. See Plan 3 hooks section.
    HookCallback {
        /// Opaque id assigned by forge-sdk at initialize time.
        callback_id: String,
        /// Hook-specific payload; includes `hook_event_name` to discriminate.
        input: Value,
        /// Tool-use id when the hook fired in a tool-use context.
        tool_use_id: Option<String>,
    },
    /// Forward-compat fallback for `subtype` values forge-sdk doesn't
    /// recognise. The dispatcher responds with an error (since no handler
    /// is registered) and logs a warning. This prevents an unknown CLI
    /// subtype from panicking the session.
    Unknown {
        /// The unrecognised `subtype` value.
        subtype: String,
        /// Full request JSON including `request` body.
        raw: Value,
    },
}

impl Serialize for ControlRequestKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            Self::CanUseTool {
                tool_name,
                input,
                permission_suggestions,
                blocked_path,
                tool_use_id,
                agent_id,
                decision_reason,
                title,
                display_name,
                description,
            } => {
                let mut field_count = 4;
                if !permission_suggestions.is_empty() {
                    field_count += 1;
                }
                if blocked_path.is_some() {
                    field_count += 1;
                }
                if agent_id.is_some() {
                    field_count += 1;
                }
                for opt in [decision_reason, title, display_name, description] {
                    if opt.is_some() {
                        field_count += 1;
                    }
                }
                let mut map = serializer.serialize_map(Some(field_count))?;
                map.serialize_entry("subtype", "can_use_tool")?;
                map.serialize_entry("tool_name", tool_name)?;
                map.serialize_entry("input", input)?;
                if !permission_suggestions.is_empty() {
                    map.serialize_entry("permission_suggestions", permission_suggestions)?;
                }
                if let Some(bp) = blocked_path {
                    map.serialize_entry("blocked_path", bp)?;
                }
                map.serialize_entry("tool_use_id", tool_use_id)?;
                if let Some(aid) = agent_id {
                    map.serialize_entry("agent_id", aid)?;
                }
                if let Some(dr) = decision_reason {
                    map.serialize_entry("decision_reason", dr)?;
                }
                if let Some(t) = title {
                    map.serialize_entry("title", t)?;
                }
                if let Some(dn) = display_name {
                    map.serialize_entry("display_name", dn)?;
                }
                if let Some(d) = description {
                    map.serialize_entry("description", d)?;
                }
                map.end()
            }
            Self::McpMessage { server_name, message } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("subtype", "mcp_message")?;
                map.serialize_entry("server_name", server_name)?;
                map.serialize_entry("message", message)?;
                map.end()
            }
            Self::HookCallback { callback_id, input, tool_use_id } => {
                let mut field_count = 3;
                if tool_use_id.is_some() {
                    field_count += 1;
                }
                let mut map = serializer.serialize_map(Some(field_count))?;
                map.serialize_entry("subtype", "hook_callback")?;
                map.serialize_entry("callback_id", callback_id)?;
                map.serialize_entry("input", input)?;
                if let Some(tid) = tool_use_id {
                    map.serialize_entry("tool_use_id", tid)?;
                }
                map.end()
            }
            Self::Unknown { raw, .. } => raw.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for ControlRequestKind {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Value::deserialize(deserializer)?;
        // `subtype` is required per the wire spec. Distinguish "missing"
        // (wire corruption / CLI bug) from "unrecognised" (forward-compat
        // drift) so debug logs + `Unknown` payloads carry the right
        // signal for the dispatcher.
        let subtype = raw.get("subtype").and_then(Value::as_str).unwrap_or("<missing>").to_string();
        match subtype.as_str() {
            "can_use_tool" => {
                let tool_name = raw
                    .get("tool_name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| serde::de::Error::missing_field("tool_name"))?
                    .to_string();
                let input = raw.get("input").cloned().unwrap_or(Value::Null);
                let permission_suggestions = raw
                    .get("permission_suggestions")
                    .and_then(|v| v.as_array().cloned())
                    .unwrap_or_default();
                let blocked_path =
                    raw.get("blocked_path").and_then(Value::as_str).map(str::to_string);
                let tool_use_id = raw
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| serde::de::Error::missing_field("tool_use_id"))?
                    .to_string();
                let agent_id = raw.get("agent_id").and_then(Value::as_str).map(str::to_string);
                let decision_reason =
                    raw.get("decision_reason").and_then(Value::as_str).map(str::to_string);
                let title = raw.get("title").and_then(Value::as_str).map(str::to_string);
                let display_name =
                    raw.get("display_name").and_then(Value::as_str).map(str::to_string);
                let description =
                    raw.get("description").and_then(Value::as_str).map(str::to_string);
                Ok(Self::CanUseTool {
                    tool_name,
                    input,
                    permission_suggestions,
                    blocked_path,
                    tool_use_id,
                    agent_id,
                    decision_reason,
                    title,
                    display_name,
                    description,
                })
            }
            "mcp_message" => {
                let server_name = raw
                    .get("server_name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| serde::de::Error::missing_field("server_name"))?
                    .to_string();
                let message = raw.get("message").cloned().unwrap_or(Value::Null);
                Ok(Self::McpMessage { server_name, message })
            }
            "hook_callback" => {
                let callback_id = raw
                    .get("callback_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| serde::de::Error::missing_field("callback_id"))?
                    .to_string();
                let input = raw.get("input").cloned().unwrap_or(Value::Null);
                let tool_use_id =
                    raw.get("tool_use_id").and_then(Value::as_str).map(str::to_string);
                Ok(Self::HookCallback { callback_id, input, tool_use_id })
            }
            // Forward-compat catch-all. NOTE: when adding a new known
            // `ControlRequestKind` variant, add a matching arm above —
            // a new variant without an arm here will silently land in
            // `Unknown` instead of being recognised.
            other => Ok(Self::Unknown { subtype: other.to_string(), raw }),
        }
    }
}

/// Control response from SDK back to the `claude` binary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlResponse {
    /// Fixed discriminant `"control_response"`.
    #[serde(rename = "type", default = "default_control_response_type")]
    pub ty: ControlResponseType,
    /// The response payload.
    pub response: ControlResponseKind,
}

fn default_control_response_type() -> ControlResponseType {
    ControlResponseType::ControlResponse
}

/// The discriminant type for [`ControlResponse`]. Always `"control_response"`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ControlResponseType {
    /// The only current variant.
    ControlResponse,
}

/// Response body, success or error.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
pub enum ControlResponseKind {
    /// Successful response. `response` carries the control-specific payload.
    Success {
        /// Request id being answered.
        request_id: String,
        /// The nested payload (e.g. an [`AllowBehavior`] serialised to JSON).
        response: Value,
    },
    /// Error response. `error` is human-readable.
    Error {
        /// Request id being answered.
        request_id: String,
        /// Error message.
        error: String,
    },
}

/// Serialisable shape for the `response` field inside a successful
/// `can_use_tool` response.
///
/// Wraps the CLI's `PermissionResultAllow` / `PermissionResultDeny`.
/// NOTE: wire keys are camelCase (`updatedInput`, `updatedPermissions`) —
/// do not rename to `snake_case`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "behavior", rename_all = "snake_case")]
pub enum AllowBehavior {
    /// Allow the call. `updated_input` is ALWAYS populated on the wire —
    /// when the callback had no override, caller must echo the original
    /// `input` into this field (the CLI always does).
    Allow {
        /// The input to invoke the tool with (possibly modified by the
        /// callback). Serialised on the wire as `updatedInput`.
        #[serde(rename = "updatedInput")]
        updated_input: Value,
        /// Optional permission-policy updates (advanced). Serialised as
        /// `updatedPermissions`. Typically `None` in v0.1 usage.
        #[serde(default, rename = "updatedPermissions", skip_serializing_if = "Option::is_none")]
        updated_permissions: Option<Value>,
    },
    /// Deny the call with a user-visible message, optionally signalling
    /// that the model should be interrupted rather than continue a turn.
    Deny {
        /// Feedback forwarded to the model.
        message: String,
        /// Interrupt flag. The CLI emits this only when truthy
        ///; mirror that by skipping when
        /// `false`.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        interrupt: bool,
    },
}

impl ControlRequest {
    /// Helper: build a `control_response` envelope corresponding to this
    /// request given an allow/deny behaviour.
    ///
    /// # Errors
    ///
    /// [`serde_json::Error`] if the decision can't be serialised (practically
    /// impossible for valid decisions).
    pub fn build_response(
        &self,
        behavior: AllowBehavior,
    ) -> Result<ControlResponse, serde_json::Error> {
        let response_value = serde_json::to_value(behavior)?;
        Ok(ControlResponse {
            ty: ControlResponseType::ControlResponse,
            response: ControlResponseKind::Success {
                request_id: self.request_id.clone(),
                response: response_value,
            },
        })
    }

    /// For a `CanUseTool` request, return a reference to the original
    /// `input` JSON. Used to echo the original input back in an allow
    /// response when the callback supplied no override (the CLI always
    /// populates `updatedInput`).
    #[must_use]
    pub fn original_tool_input(&self) -> Option<&Value> {
        match &self.request {
            ControlRequestKind::CanUseTool { input, .. } => Some(input),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests_control_types {
    // Test-mod `use super::*;` brings the parent's full surface in; not every test consumes every item.
    #[allow(unused_imports)]
    use super::*;

    use crate::control::{
        AllowBehavior, ControlRequest, ControlRequestKind, ControlResponse, ControlResponseKind,
        ControlResponseType,
    };
    use serde_json::json;

    #[test]
    fn parse_can_use_tool_request() {
        let raw = json!({
            "type": "control_request",
            "request_id": "req_1_a1b2c3d4",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "Edit",
                "input": {"file_path": "/tmp/x"},
                "permission_suggestions": [],
                "blocked_path": null,
                "tool_use_id": "toolu_01abc",
                "agent_id": null
            }
        });
        let req: ControlRequest = serde_json::from_value(raw).expect("parse");
        assert_eq!(req.request_id, "req_1_a1b2c3d4");
        match req.request {
            ControlRequestKind::CanUseTool { tool_name, input, tool_use_id, .. } => {
                assert_eq!(tool_name, "Edit");
                assert_eq!(input["file_path"], "/tmp/x");
                assert_eq!(tool_use_id, "toolu_01abc");
            }
            other => panic!("expected CanUseTool, got: {other:?}"),
        }
    }

    #[test]
    fn serialize_allow_response_uses_camelcase_updated_input() {
        let allow = AllowBehavior::Allow {
            updated_input: json!({"file_path": "/tmp/x"}),
            updated_permissions: None,
        };
        let raw = serde_json::to_value(&allow).expect("ser");
        assert_eq!(raw["behavior"], "allow");
        // CRITICAL: wire is camelCase `updatedInput`, not snake_case.
        assert_eq!(raw["updatedInput"]["file_path"], "/tmp/x");
        assert!(raw.get("updated_input").is_none(), "must NOT serialise snake_case");
    }

    #[test]
    fn serialize_deny_response_with_interrupt_true() {
        let deny = AllowBehavior::Deny { message: "not allowed".into(), interrupt: true };
        let raw = serde_json::to_value(&deny).expect("ser");
        assert_eq!(raw["behavior"], "deny");
        assert_eq!(raw["message"], "not allowed");
        assert_eq!(raw["interrupt"], true);
    }

    #[test]
    fn deny_with_interrupt_false_omits_field() {
        let deny = AllowBehavior::Deny { message: "nope".into(), interrupt: false };
        let raw = serde_json::to_value(&deny).expect("ser");
        assert_eq!(raw["behavior"], "deny");
        assert!(raw.get("interrupt").is_none(), "interrupt must be absent when false");
    }

    #[test]
    fn full_control_response_envelope() {
        let resp = ControlResponse {
            ty: ControlResponseType::ControlResponse,
            response: ControlResponseKind::Success {
                request_id: "req_1_a1b2c3d4".into(),
                response: serde_json::to_value(AllowBehavior::Allow {
                    updated_input: json!({"file_path": "/tmp/x"}),
                    updated_permissions: None,
                })
                .unwrap(),
            },
        };
        let raw = serde_json::to_value(&resp).expect("serialize");
        assert_eq!(raw["type"], "control_response");
        assert_eq!(raw["response"]["subtype"], "success");
        assert_eq!(raw["response"]["request_id"], "req_1_a1b2c3d4");
        assert_eq!(raw["response"]["response"]["behavior"], "allow");
        assert_eq!(raw["response"]["response"]["updatedInput"]["file_path"], "/tmp/x");
    }

    #[test]
    fn deserialize_missing_subtype_lands_in_unknown_with_missing_sentinel() {
        // A `control_request` without `subtype` is wire corruption.
        // The Deserialize impl distinguishes this from forward-compat
        // drift by using the `<missing>` sentinel — pinned here so a
        // future refactor can't silently flip back to `unwrap_or("")`
        // without breaking a test.
        let raw = json!({
            "type": "control_request",
            "request_id": "r1",
            "request": {"some": "shape"}
        });
        let req: ControlRequest = serde_json::from_value(raw).expect("parse");
        match req.request {
            ControlRequestKind::Unknown { subtype, .. } => {
                assert_eq!(
                    subtype, "<missing>",
                    "missing-subtype must use the explicit sentinel, not empty string"
                );
            }
            other => panic!("expected ControlRequestKind::Unknown, got: {other:?}"),
        }
    }

    #[test]
    fn deserialize_unrecognised_subtype_distinguishable_from_missing() {
        // Pairs with the test above. A real-but-unknown subtype must
        // round-trip its actual string into Unknown so debug logs +
        // dispatcher branches can tell forward-compat drift from
        // wire corruption.
        let raw = json!({
            "type": "control_request",
            "request_id": "r1",
            "request": {"subtype": "future_thing", "data": 1}
        });
        let req: ControlRequest = serde_json::from_value(raw).expect("parse");
        match req.request {
            ControlRequestKind::Unknown { subtype, .. } => {
                assert_eq!(subtype, "future_thing");
                assert_ne!(subtype, "<missing>");
            }
            other => panic!("expected ControlRequestKind::Unknown, got: {other:?}"),
        }
    }
}
