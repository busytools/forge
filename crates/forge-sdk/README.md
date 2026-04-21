# forge-sdk

Rust port of Anthropic's [`claude-agent-sdk`](https://github.com/anthropics/claude-agent-sdk-python).

## Status

- **v0.0.2** — M0 + M1 + M2 + M3 complete. Core transport, permission
  callback, in-process MCP tool hosting all working. Not yet published
  to crates.io.

## Scope

Targets feature parity with Python `claude-agent-sdk`. Current crate exposes:

- `Client` / `OptionsBuilder` / `PermissionMode`
- `content::*`, `messages::*` (stream-json types)
- `permissions::{PermissionDecision, ToolPermissionContext, CanUseToolCallback}`
- `mcp::{McpServer, McpServerBuilder, Tool, ToolInput, ToolOutput}`
- `tool!` declarative macro

See the top-level [forge README](../..) for roadmap.

## Minimal example

```rust,ignore
use forge_sdk::{Client, OptionsBuilder};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opts = OptionsBuilder::new().build();
    let mut client = Client::spawn(opts).await?;
    client.send_user_message("hello").await?;
    while let Some(msg) = client.next_event().await? {
        println!("{msg:?}");
    }
    client.disconnect().await?;
    Ok(())
}
```

See `examples/` for working permissions + MCP demos.

## Licence

MIT.
