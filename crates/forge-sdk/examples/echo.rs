//! Minimal example: spawn `claude`, ask it a single question, print every
//! event until the result arrives, then disconnect.
//!
//! Run:
//! ```bash
//! cargo run -p forge-sdk --example echo -- "What is 2 + 2?"
//! ```

use anyhow::Result;
use forge_sdk::Message;
use forge_sdk::{Client, OptionsBuilder, PermissionMode};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let prompt = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "Reply with the word 'hello' and nothing else.".to_string());

    let opts = OptionsBuilder::new().permission_mode(PermissionMode::BypassPermissions).build();
    let (client, mut events) = Client::spawn(opts).await?;
    println!("session: {}", client.session_id());

    client.send_user_message(&prompt).await?;

    while let Some(item) = events.recv().await {
        let event = item?;
        match &event {
            Message::Assistant { message, .. } => {
                for block in &message.content {
                    println!("{block:?}");
                }
            }
            Message::Result { total_cost_usd, duration_ms, .. } => {
                let cost = total_cost_usd.unwrap_or(0.0);
                println!("result: ${cost:.4} in {duration_ms}ms");
                break;
            }
            other => println!("{other:?}"),
        }
    }

    client.disconnect().await?;
    Ok(())
}
