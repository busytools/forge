//! Control-protocol messages exchanged over the main stream-json stdio channel.
//!
//! These carry out-of-band requests that the SDK must answer synchronously,
//! such as permission checks (`can_use_tool`). See `docs/protocol-notes.md`
//! for the observed Python SDK wire shapes we mirror here.

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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
pub enum ControlRequestKind {
    /// A permission check for a tool call. Matches Python SDK
    /// `SDKControlPermissionRequest` at types.py:1283-1291.
    CanUseTool {
        /// Tool name the model wants to invoke.
        tool_name: String,
        /// The JSON input the model generated.
        input: Value,
        /// Permission-suggestion hints from the binary (may be empty).
        #[serde(default)]
        permission_suggestions: Vec<Value>,
        /// Path blocked by workspace sandboxing, when applicable.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        blocked_path: Option<String>,
        /// Identifier of this tool-use (required; string).
        tool_use_id: String,
        /// Agent identifier when this is a sub-agent request.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
    },
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
/// Mirrors Python's `PermissionResultAllow` / `PermissionResultDeny`.
/// NOTE: wire keys are camelCase (`updatedInput`, `updatedPermissions`) —
/// do not rename to `snake_case`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "behavior", rename_all = "snake_case")]
pub enum AllowBehavior {
    /// Allow the call. `updated_input` is ALWAYS populated on the wire —
    /// when the callback had no override, caller must echo the original
    /// `input` into this field (Python always does).
    Allow {
        /// The input to invoke the tool with (possibly modified by the
        /// callback). Serialised on the wire as `updatedInput`.
        #[serde(rename = "updatedInput")]
        updated_input: Value,
        /// Optional permission-policy updates (advanced). Serialised as
        /// `updatedPermissions`. Typically `None` in v0.1 usage.
        #[serde(
            default,
            rename = "updatedPermissions",
            skip_serializing_if = "Option::is_none"
        )]
        updated_permissions: Option<Value>,
    },
    /// Deny the call with a user-visible message, optionally signalling
    /// that the model should be interrupted rather than continue a turn.
    Deny {
        /// Feedback forwarded to the model.
        message: String,
        /// Interrupt flag. Python emits this only when truthy
        /// (`_internal/query.py:373-376`); mirror that by skipping when
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
    /// response when the callback supplied no override (Python SDK always
    /// populates `updatedInput`).
    #[must_use]
    pub fn original_tool_input(&self) -> Option<&Value> {
        match &self.request {
            ControlRequestKind::CanUseTool { input, .. } => Some(input),
            _ => None,
        }
    }
}
