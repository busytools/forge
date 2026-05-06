//! Live-capture scenario: in-process MCP server + tool call.
//!
//! Registers an SDK-hosted MCP server with a single `greet` tool, then
//! asks the model to call it. The captured trace includes the
//! `mcp_message` `control_request` round trip — CLI → SDK sending
//! `initialize` / `tools/list` / `tools/call` JSON-RPC, SDK replying
//! with the same `request_id`'s `control_response`.
//!
//! Uses `PermissionMode::BypassPermissions` because the `mcp__probe__greet`
//! tool isn't in the CLI's default allow list and we don't want the
//! permission callback in the path (that's its own scenario).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use async_trait::async_trait;
use forge_sdk::mcp::{McpServerBuilder, Tool, ToolInput, ToolOutput};
use forge_sdk::{OptionsBuilder, PermissionMode};
use forge_test_harness::sdk_wire::run_live_scenario;
use serde_json::json;

struct GreetTool;

#[async_trait]
impl Tool for GreetTool {
    fn name(&self) -> &'static str {
        "greet"
    }
    fn description(&self) -> &'static str {
        "Greet someone by name"
    }
    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {"name": {"type": "string"}},
            "required": ["name"]
        })
    }
    async fn call(&self, input: ToolInput) -> ToolOutput {
        let name = input.value["name"].as_str().unwrap_or("friend");
        ToolOutput::text(format!("Hello, {name}!"))
    }
}

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_in_process_mcp() {
    let server = McpServerBuilder::new("probe", "0.0.1").tool(GreetTool).build();

    let opts = OptionsBuilder::new()
        .max_turns(3)
        .permission_mode(PermissionMode::BypassPermissions)
        .mcp_server("probe", server)
        .build();

    run_live_scenario("in_process_mcp", opts, |client, events| async move {
        client
            .send_user_message(
                "Call the mcp__probe__greet tool with name=\"forge\" \
                 and reply with exactly what it returns.",
            )
            .await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}
