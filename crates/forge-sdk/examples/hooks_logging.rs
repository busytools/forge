//! Example: log every tool use via `PreToolUse` + `PostToolUse` hooks.
//!
//! Run:
//! ```bash
//! cargo run -p forge-sdk --example hooks_logging -- "List /tmp"
//! ```

// Examples are illustrative; aborting on misuse is the right exit behaviour.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use anyhow::Result;
use forge_primitives::Message;
use forge_sdk::{
    Client, HookContext, HookDecision, HooksBuilder, OptionsBuilder, PermissionMode,
    PostToolUseInput, PreToolUseInput,
};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let prompt = std::env::args().nth(1).unwrap_or_else(|| "List /tmp".into());

    let hooks = HooksBuilder::new()
        .pre_tool_use("*", |input: PreToolUseInput, _ctx: HookContext| async move {
            eprintln!("PRE  {:>20} {}", input.tool_name, input.tool_input);
            HookDecision::allow()
        })
        .post_tool_use("*", |input: PostToolUseInput, _ctx: HookContext| async move {
            let preview = input.tool_response.to_string();
            let short = if preview.len() > 80 { &preview[..80] } else { &preview };
            eprintln!("POST {:>20} {short}", input.tool_name);
            HookDecision::passthrough()
        })
        .build();

    let opts = OptionsBuilder::new()
        .permission_mode(PermissionMode::BypassPermissions)
        .hooks(hooks)
        .build();

    let (client, mut events) = Client::spawn(opts).await?;
    client.send_user_message(&prompt).await?;

    while let Some(item) = events.recv().await {
        if let Message::Result { .. } = item? {
            break;
        }
    }

    client.disconnect().await?;
    Ok(())
}
