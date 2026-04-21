# forge-sdk

Rust port of Anthropic's [`claude-agent-sdk`](https://github.com/anthropics/claude-agent-sdk-python).

## Status

- **v0.1.0** — parity with Python `claude-agent-sdk` v0.1.64. All seven
  milestones (M0 scaffolding → M7 polish) shipped. Not yet published to
  crates.io.

## Scope

Targets feature parity with Python `claude-agent-sdk`. Current crate exposes:

- `Client` / `OptionsBuilder` / `PermissionMode` (6 variants)
- `content::*`, `messages::*` (stream-json types)
- `permissions::{PermissionDecision, ToolPermissionContext, CanUseToolCallback}`
- `mcp::{McpServer, McpServerBuilder, Tool, ToolInput, ToolOutput}`
- `tool!` declarative macro
- `hooks::*` — 10 hook kinds + `HooksBuilder` + `HookDecision`
- `session_store::{SessionStore, MemorySessionStore, FsSessionStore}`
- `tracing_bridge` — turn / tool / hook spans
- Skills, `allowed_tools`, `setting_sources`, `exclude_dynamic_sections`
  on `OptionsBuilder`

See the top-level [forge README](../..) for roadmap and
[`docs/CHANGELOG.md`](../../docs/CHANGELOG.md) for release history.

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
