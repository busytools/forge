//! Example: a permission callback that allows read-only tools and denies
//! anything that mutates the workspace.
//!
//! Run against the real claude binary:
//! ```bash
//! cargo run -p forge-sdk --example permissions -- "Read /tmp/README.md"
//! ```


#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use anyhow::Result;
use forge_sdk::Message;
use forge_sdk::{
    Client, OptionsBuilder, PermissionDecision, PermissionMode, ToolPermissionContext,
};

fn is_read_only(tool_name: &str) -> bool {
    matches!(tool_name, "Read" | "Grep" | "Glob" | "LS" | "WebFetch")
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let prompt =
        std::env::args().nth(1).unwrap_or_else(|| "Read /tmp/README.md and summarise".to_string());

    let opts = OptionsBuilder::new()
        .permission_mode(PermissionMode::Ask)
        .can_use_tool(|ctx: ToolPermissionContext| async move {
            if is_read_only(&ctx.tool_name) {
                eprintln!("ALLOW {} {}", ctx.tool_name, ctx.tool_input);
                PermissionDecision::allow()
            } else {
                eprintln!("DENY {} (mutating tools not allowed)", ctx.tool_name);
                PermissionDecision::deny(format!(
                    "{} is not allowed in this read-only session",
                    ctx.tool_name
                ))
            }
        })
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
