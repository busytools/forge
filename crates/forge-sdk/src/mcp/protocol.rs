//! JSON-RPC 2.0 message types used by MCP.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// MCP protocol revisions this server speaks, newest first. Nothing here is
/// version-specific beyond text content blocks and `isError`, both unchanged
/// across the range.
pub const SUPPORTED_PROTOCOL_VERSIONS: [&str; 4] =
    ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// Answered when the client asks for a revision we do not speak.
pub const LATEST_PROTOCOL_VERSION: &str = SUPPORTED_PROTOCOL_VERSIONS[0];

/// A JSON-RPC request from the `claude` binary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Request id; absent for notifications.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    /// Method name (e.g. `"initialize"`, `"tools/list"`, `"tools/call"`).
    pub method: String,
    /// Method-specific params.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// A JSON-RPC response from the MCP server back to the `claude` binary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Request id this response correlates to.
    pub id: Value,
    /// Success payload, mutually exclusive with `error`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<JsonRpcResult>,
    /// Error payload, mutually exclusive with `result`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<McpError>,
}

impl JsonRpcResponse {
    /// Build a successful response.
    pub fn success(id: Value, result: JsonRpcResult) -> Self {
        Self { jsonrpc: "2.0".into(), id, result: Some(result), error: None }
    }

    /// Build an error response.
    pub fn error(id: Value, error: McpError) -> Self {
        Self { jsonrpc: "2.0".into(), id, result: None, error: Some(error) }
    }
}

/// Typed successful-response payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcResult {
    /// Response to `initialize`.
    Initialize {
        /// Protocol version the server speaks.
        #[serde(rename = "protocolVersion")]
        protocol_version: String,
        /// Server capabilities (narrow for now; tools only).
        capabilities: Value,
        /// Server metadata.
        #[serde(rename = "serverInfo")]
        server_info: ServerInfo,
    },
    /// Response to `tools/list`.
    ToolsList {
        /// Registered tool descriptions.
        tools: Vec<ToolDescription>,
    },
    /// Response to `tools/call`.
    ToolsCall {
        /// The content blocks the tool produced.
        content: Vec<Value>,
        /// Whether the call was an error.
        #[serde(default, rename = "isError", skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
    /// The spec's empty `{}` result, used by `ping`. Must stay last: untagged
    /// deserialisation tries variants in order and a fieldless struct variant
    /// matches any object.
    Empty {},
}

/// Server metadata in `initialize` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    /// Human-readable server name (e.g. `"forge-sdk-in-process"`).
    pub name: String,
    /// Server version string.
    pub version: String,
}

/// Standard MCP error body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpError {
    /// JSON-RPC error code. Common values: -32700 parse, -32600 invalid request,
    /// -32601 method not found, -32602 invalid params, -32603 internal error.
    pub code: i32,
    /// Human-readable error message.
    pub message: String,
    /// Optional structured data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// A tool as described in the `tools/list` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDescription {
    /// Tool name (e.g. `"greet"` - will be exposed to the model as
    /// `mcp__<server-name>__greet`).
    pub name: String,
    /// One-line human-readable description.
    pub description: String,
    /// JSON-Schema describing the tool's `arguments` object.
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

#[cfg(test)]
mod tests_mcp_protocol {
    // Test-mod `use super::*;` brings the parent's full surface in; not every test consumes every item.
    #[allow(unused_imports)]
    use super::*;

    use crate::mcp::McpError;
    use crate::mcp::protocol::{
        JsonRpcRequest, JsonRpcResponse, JsonRpcResult, ServerInfo, ToolDescription,
    };
    use serde_json::json;

    #[test]
    fn initialize_request_parse() {
        let raw = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2024-11-05"}
        });
        let req: JsonRpcRequest = serde_json::from_value(raw).expect("parse");
        assert_eq!(req.jsonrpc, "2.0");
        assert_eq!(req.method, "initialize");
        assert_eq!(req.id, Some(json!(1)));
    }

    #[test]
    fn tools_list_response_shape() {
        let tools = vec![ToolDescription {
            name: "greet".into(),
            description: "Greet by name".into(),
            input_schema: json!({"type": "object", "properties": {"name": {"type": "string"}}}),
        }];
        let resp = JsonRpcResponse::success(json!(1), JsonRpcResult::ToolsList { tools });
        let raw = serde_json::to_value(&resp).expect("ser");
        assert_eq!(raw["jsonrpc"], "2.0");
        assert_eq!(raw["id"], 1);
        assert!(raw["result"]["tools"].is_array());
        assert_eq!(raw["result"]["tools"][0]["name"], "greet");
    }

    #[test]
    fn error_response_shape() {
        let resp = JsonRpcResponse::error(
            json!(2),
            McpError { code: -32601, message: "Method not found".into(), data: None },
        );
        let raw = serde_json::to_value(&resp).expect("ser");
        assert_eq!(raw["error"]["code"], -32601);
        assert_eq!(raw["error"]["message"], "Method not found");
        assert!(raw["result"].is_null());
    }

    #[test]
    fn empty_variant_does_not_shadow_the_others() {
        // `Empty {}` matches any object, so untagged deserialisation only stays
        // correct while it is declared last.
        let call = json!({"content": [{"type": "text", "text": "hi"}], "isError": true});
        let parsed: JsonRpcResult = serde_json::from_value(call).expect("parse");
        assert!(matches!(parsed, JsonRpcResult::ToolsCall { is_error: true, .. }));

        let parsed: JsonRpcResult = serde_json::from_value(json!({})).expect("parse");
        assert!(matches!(parsed, JsonRpcResult::Empty {}));
    }

    #[test]
    fn ping_result_serialises_to_an_empty_object() {
        let resp = JsonRpcResponse::success(json!(9), JsonRpcResult::Empty {});
        let raw = serde_json::to_value(&resp).expect("ser");
        assert_eq!(raw["result"], json!({}));
    }

    #[test]
    fn initialize_response_shape() {
        let resp = JsonRpcResponse::success(
            json!(1),
            JsonRpcResult::Initialize {
                protocol_version: "2024-11-05".into(),
                capabilities: json!({"tools": {}}),
                server_info: ServerInfo { name: "probe".into(), version: "0.0.1".into() },
            },
        );
        let raw = serde_json::to_value(&resp).expect("ser");
        assert_eq!(raw["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(raw["result"]["serverInfo"]["name"], "probe");
    }
}
