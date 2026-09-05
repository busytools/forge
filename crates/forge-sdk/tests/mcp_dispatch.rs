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
    fn name(&self) -> &'static str {
        "echo"
    }
    fn description(&self) -> &'static str {
        "Echoes its input"
    }
    fn input_schema(&self) -> serde_json::Value {
        json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]})
    }
    async fn call(&self, input: ToolInput) -> ToolOutput {
        ToolOutput::text(input.value["text"].as_str().unwrap_or("").to_string())
    }
}

struct NamedTool(&'static str);

#[async_trait]
impl Tool for NamedTool {
    fn name(&self) -> &'static str {
        self.0
    }
    fn description(&self) -> &'static str {
        "Named probe tool"
    }
    fn input_schema(&self) -> serde_json::Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }
    async fn call(&self, _input: ToolInput) -> ToolOutput {
        ToolOutput::text("ok")
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

#[test]
fn has_tool_prefix_matches_registered_names_only() {
    let server = McpServerBuilder::new("forge", "0.0.0").tool(NamedTool("workers__list")).build();
    assert!(server.has_tool_prefix("workers__"));
    assert!(!server.has_tool_prefix("peers__"), "no peers tool registered");
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

async fn negotiated(requested: serde_json::Value) -> String {
    let server = McpServerBuilder::new("probe", "0.0.1").tool(EchoTool).build();
    let resp = server
        .dispatch(&req(1, "initialize", requested))
        .await
        .expect("initialize produces a response");
    serde_json::to_value(&resp).unwrap()["result"]["protocolVersion"]
        .as_str()
        .expect("protocolVersion is a string")
        .to_string()
}

#[tokio::test]
async fn initialize_echoes_every_supported_version() {
    for v in forge_sdk::mcp::protocol::SUPPORTED_PROTOCOL_VERSIONS {
        assert_eq!(negotiated(json!({"protocolVersion": v})).await, v, "should echo {v}");
    }
}

/// The wire answer is our latest whenever we cannot agree, rather than an
/// error - the client decides whether it can live with that. The unsupported
/// and malformed cases are told apart by the classifier, not here; see
/// `tests_negotiation` in `mcp/server.rs`.
#[tokio::test]
async fn initialize_answers_latest_when_it_cannot_agree() {
    for params in [
        json!({"protocolVersion": "1999-01-01"}),
        // Batching revision we deliberately do not advertise.
        json!({"protocolVersion": "2025-03-26"}),
        json!({"protocolVersion": 5}),
        json!({"capabilities": {}}),
        serde_json::Value::Null,
    ] {
        assert_eq!(
            negotiated(params.clone()).await,
            forge_sdk::mcp::protocol::LATEST_PROTOCOL_VERSION,
            "params: {params}"
        );
    }
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
async fn tools_list_order_is_deterministic() {
    // Registration order is deliberately neither sorted nor reverse-sorted.
    let mut b = McpServerBuilder::new("probe", "0.0.1");
    for n in ["spawn", "list", "tell", "ask", "despawn", "update"] {
        b = b.tool(NamedTool(n));
    }
    let resp =
        b.build().dispatch(&req(2, "tools/list", serde_json::Value::Null)).await.expect("response");
    let listed: Vec<String> = serde_json::to_value(&resp).unwrap()["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();

    let mut sorted = listed.clone();
    sorted.sort();
    assert_eq!(listed, sorted, "tools/list must emit a stable, sorted order");
}

#[tokio::test]
async fn dispatch_ping_returns_empty_result() {
    let server = McpServerBuilder::new("probe", "0.0.1").tool(EchoTool).build();
    let resp = server
        .dispatch(&req(9, "ping", serde_json::Value::Null))
        .await
        .expect("ping produces a response");
    let raw = serde_json::to_value(&resp).unwrap();
    assert_eq!(raw["id"], 9);
    assert_eq!(raw["result"], json!({}), "spec requires an empty result object");
    assert!(raw["error"].is_null());
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

/// An id-bearing notification used to return `None`, which
/// `control_dispatch` turns into an id-less `{"jsonrpc":"2.0","result":{}}` -
/// a response the client cannot correlate to anything it sent. -32601 matches
/// the CLI's own SDK, which routes a stray id into `_onrequest` and misses the
/// handler lookup because the method is registered as a notification.
#[tokio::test]
async fn id_bearing_notification_gets_a_correlatable_error() {
    let server = McpServerBuilder::new("probe", "0.0.1").tool(EchoTool).build();
    let resp = server
        .dispatch(&req(7, "notifications/initialized", serde_json::Value::Null))
        .await
        .expect("an id-bearing request always gets a response");
    let raw = serde_json::to_value(&resp).unwrap();
    assert_eq!(raw["id"], 7, "the client's id must come back");
    assert_eq!(raw["error"]["code"], -32601);
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
