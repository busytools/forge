//! Roundtrip tests for MCP JSON-RPC message shapes.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forge_sdk::mcp::McpError;
use forge_sdk::mcp::protocol::{
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
        McpError {
            code: -32601,
            message: "Method not found".into(),
            data: None,
        },
    );
    let raw = serde_json::to_value(&resp).expect("ser");
    assert_eq!(raw["error"]["code"], -32601);
    assert_eq!(raw["error"]["message"], "Method not found");
    assert!(raw["result"].is_null());
}

#[test]
fn initialize_response_shape() {
    let resp = JsonRpcResponse::success(
        json!(1),
        JsonRpcResult::Initialize {
            protocol_version: "2024-11-05".into(),
            capabilities: json!({"tools": {}}),
            server_info: ServerInfo {
                name: "probe".into(),
                version: "0.0.1".into(),
            },
        },
    );
    let raw = serde_json::to_value(&resp).expect("ser");
    assert_eq!(raw["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(raw["result"]["serverInfo"]["name"], "probe");
}
