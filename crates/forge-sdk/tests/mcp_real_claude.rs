//! Real-claude MCP round-trip.
//!
//! Requires `claude` on PATH. Gated with `#[ignore]`; run with
//! `cargo nextest run --run-ignored all --test mcp_real_claude`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unnecessary_literal_bound,
    clippy::collapsible_match,
    clippy::collapsible_if
)]

use async_trait::async_trait;
use forge_primitives::Message;
use forge_sdk::mcp::{McpServerBuilder, Tool, ToolInput, ToolOutput};
use forge_sdk::{Client, OptionsBuilder, PermissionMode};
use serde_json::json;

struct GreetTool;

#[async_trait]
impl Tool for GreetTool {
    fn name(&self) -> &str {
        "greet"
    }
    fn description(&self) -> &str {
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
#[ignore = "requires `claude` on PATH"]
async fn real_claude_calls_in_process_tool() {
    if std::process::Command::new("claude").arg("--version").output().is_err() {
        eprintln!("skipping: `claude` not on PATH");
        return;
    }

    let server = McpServerBuilder::new("probe", "0.0.1").tool(GreetTool).build();

    let opts = OptionsBuilder::new()
        .permission_mode(PermissionMode::BypassPermissions)
        .mcp_server("probe", server)
        .build();

    let (client, mut events) = Client::spawn(opts).await.expect("spawn");
    client
        .send_user_message(
            "Call the mcp__probe__greet tool with name='world' and reply with exactly what it returns.",
        )
        .await
        .expect("send");

    let mut saw_tool_use = false;
    let mut saw_greeting = false;
    loop {
        let item = tokio::time::timeout(std::time::Duration::from_secs(90), events.recv())
            .await
            .expect("timeout");
        let Some(msg) = item else {
            panic!("stream ended early");
        };
        let msg = msg.expect("read");
        match msg {
            Message::Assistant { message, .. } => {
                for block in message.content {
                    match block {
                        forge_primitives::ContentBlock::ToolUse { name, .. } => {
                            if name == "mcp__probe__greet" {
                                saw_tool_use = true;
                            }
                        }
                        forge_primitives::ContentBlock::Text { text } => {
                            if text.contains("Hello, world!") {
                                saw_greeting = true;
                            }
                        }
                        _ => {}
                    }
                }
            }
            Message::Result { .. } => break,
            _ => {}
        }
    }

    assert!(saw_tool_use, "expected claude to invoke mcp__probe__greet");
    assert!(saw_greeting, "expected `Hello, world!` in final reply");
    client.disconnect().await.expect("disconnect");
}
