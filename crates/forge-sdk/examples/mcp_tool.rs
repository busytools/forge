//! Example: expose a Rust function as an MCP tool and let claude use it.
//!
//! Run:
//! ```bash
//! cargo run -p forge-sdk --example mcp_tool -- "Double 21 using mcp__local__double"
//! ```

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use anyhow::Result;
use forge_sdk::Message;
use forge_sdk::mcp::{McpServerBuilder, ToolInput, ToolOutput};
use forge_sdk::{Client, OptionsBuilder, PermissionMode, tool};

tool! {
    name: "double",
    description: "Double an integer n",
    schema: serde_json::json!({
        "type": "object",
        "properties": {"n": {"type": "integer"}},
        "required": ["n"]
    }),
    call: |input: ToolInput| async move {
        let n = input.value["n"].as_i64().unwrap_or(0);
        ToolOutput::text((n * 2).to_string())
    },
    tool_type: DoubleTool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let prompt = std::env::args().nth(1).unwrap_or_else(|| {
        "Call mcp__local__double with n=21 and reply with the result.".to_string()
    });

    let server = McpServerBuilder::new("local", "0.0.1").tool(DoubleTool).build();

    let opts = OptionsBuilder::new()
        .permission_mode(PermissionMode::BypassPermissions)
        .mcp_server("local", server)
        .build();

    let (client, mut events) = Client::spawn(opts).await?;
    client.send_user_message(&prompt).await?;

    while let Some(item) = events.recv().await {
        let event = item?;
        match &event {
            Message::Assistant { message, .. } => {
                for block in &message.content {
                    println!("{block:?}");
                }
            }
            Message::Result { total_cost_usd, .. } => {
                let cost = total_cost_usd.unwrap_or(0.0);
                println!("done, cost ${cost:.4}");
                break;
            }
            _ => {}
        }
    }

    client.disconnect().await?;
    Ok(())
}
