//! Unit-test `McpServer::dispatch` - pure request/response, no transport.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::needless_pass_by_value,
    clippy::unnecessary_literal_bound
)]

use async_trait::async_trait;
use forge_sdk::mcp::protocol::JsonRpcRequest;
use forge_sdk::mcp::{McpServerBuilder, Tool, ToolInput, ToolOutput};
use serde_json::json;

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echoes its input"
    }
    fn input_schema(&self) -> serde_json::Value {
        json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]})
    }
    async fn call(&self, input: ToolInput) -> ToolOutput {
        ToolOutput::text(input.value["text"].as_str().unwrap_or("").to_string())
    }
}

fn req(id: i64, method: &str, params: serde_json::Value) -> JsonRpcRequest {
    serde_json::from_value(json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    }))
    .unwrap()
}

#[tokio::test]
async fn dispatch_initialize() {
    let server = McpServerBuilder::new("probe", "0.0.1").tool(EchoTool).build();
    let resp = server
        .dispatch(&req(1, "initialize", json!({"protocolVersion": "2024-11-05"})))
        .await
        .expect("initialize produces a response");
    let raw = serde_json::to_value(&resp).unwrap();
    assert_eq!(raw["id"], 1);
    assert_eq!(raw["result"]["serverInfo"]["name"], "probe");
}

#[tokio::test]
async fn dispatch_tools_list() {
    let server = McpServerBuilder::new("probe", "0.0.1").tool(EchoTool).build();
    let resp = server
        .dispatch(&req(2, "tools/list", serde_json::Value::Null))
        .await
        .expect("tools/list produces a response");
    let raw = serde_json::to_value(&resp).unwrap();
    assert_eq!(raw["id"], 2);
    assert_eq!(raw["result"]["tools"][0]["name"], "echo");
}

#[tokio::test]
async fn dispatch_tools_call() {
    let server = McpServerBuilder::new("probe", "0.0.1").tool(EchoTool).build();
    let resp = server
        .dispatch(&req(3, "tools/call", json!({"name": "echo", "arguments": {"text": "hi"}})))
        .await
        .expect("tools/call produces a response");
    let raw = serde_json::to_value(&resp).unwrap();
    assert_eq!(raw["id"], 3);
    assert_eq!(raw["result"]["content"][0]["text"], "hi");
}

#[tokio::test]
async fn dispatch_notifications_return_none() {
    let server = McpServerBuilder::new("probe", "0.0.1").tool(EchoTool).build();
    let notif: JsonRpcRequest = serde_json::from_value(json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
    }))
    .unwrap();
    assert!(server.dispatch(&notif).await.is_none());
}

#[tokio::test]
async fn dispatch_unknown_method_returns_error() {
    let server = McpServerBuilder::new("probe", "0.0.1").tool(EchoTool).build();
    let resp = server
        .dispatch(&req(4, "does/not/exist", serde_json::Value::Null))
        .await
        .expect("unknown method still returns an error response");
    let raw = serde_json::to_value(&resp).unwrap();
    assert_eq!(raw["error"]["code"], -32601);
}
