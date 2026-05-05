# forge-sdk

A peer reference implementation in Rust of a client for Anthropic's
`claude` CLI. Spawns the binary, speaks stream-json over stdio,
exposes the agentic surface as typed Rust messages + commands. Wire
contract with the CLI is the only hard external invariant; API shape
is whatever serves [`forge-agent`](../forge-agent) (and through it,
[`forge-tui`](../forge-tui)) best.

Not a Python-parity port. See [`PARITY.md`](../../PARITY.md) for the
history of the parity-tracking era and the
[`CLAUDE.md`](../../CLAUDE.md) workspace guide for the current
direction.

- Release history: [`../../docs/CHANGELOG.md`](../../docs/CHANGELOG.md).

## Scope

Single responsibility: wrap the long-lived `claude` subprocess and
dispatch its callbacks. Specifically:

- Build argv from `Options` + spawn the subprocess.
- Stream-json codec on stdin/stdout.
- Control-request dispatch (permissions, hooks, in-process MCP).
- Callback registries: `Hooks`/`HooksBuilder`, `CanUseToolCallback`.
- The `Client` handle + `ClientEvents` mpsc receiver returned from
  `Client::spawn`.

Wire-shape types (Message, ContentBlock, AccountInfo, hook
inputs/outputs, permission decisions, option enums, subagent
definitions, …) live in [`forge-primitives`](../forge-primitives) and
are re-exported here for back-compat. Filesystem reads (settings,
trust, sessions catalog, project memory) live in
[`forge-agent`](../forge-agent).

## Public surface

Core types and functions exposed from the crate root:

- **Entry point** — `Client::spawn(options) -> (Client, ClientEvents)`.
- **Transport extension** — `pub trait Transport` for injecting
  custom I/O (e.g. wire-recording in `forge-test-harness`).
  `Subprocess` is the shipped in-process implementation.
- **Options + config** — `Options`, `OptionsBuilder`. The pure-data
  enums (`PermissionMode`, `SystemPromptKind`, `ThinkingConfig`,
  `ToolsPreset`, `SdkPluginConfig`) live in `forge-primitives`.
- **Hooks** — `Hooks`, `HooksBuilder`, `HookCallback` trait,
  `HookDecision`. Input + output structs live in
  `forge_primitives::hooks::{inputs, outputs}`.
- **Permissions** — `CanUseToolCallback` trait. Decision +
  context types live in `forge_primitives::permissions`.
- **MCP hosting** — `mcp::{McpServer, McpServerBuilder, Tool,
  ToolInput, ToolOutput}` + `tool!` declarative macro.
- **Subagents** — `SubagentDefinition`, `SubagentMap`. (Builder
  setters live in `forge_primitives::subagents`.)
- **Path resolution** — `claude_config_dir()`, `projects_dir()`.
- **Errors** — `Error` enum (variants mirror Python's
  `CLIConnectionError` / `CLINotFoundError` / `ProcessError` /
  `CLIJSONDecodeError` / `MessageParse` families).

## Minimal example

```rust,ignore
use forge_sdk::{Client, OptionsBuilder};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opts = OptionsBuilder::new().build();
    let (client, mut events) = Client::spawn(opts).await?;
    client.send_user_message("hello").await?;
    while let Some(msg) = events.recv().await {
        println!("{msg:?}");
    }
    client.disconnect().await?;
    Ok(())
}
```

See `examples/` for working demos:

- `echo.rs` — minimal client lifecycle.
- `hooks_logging.rs` — `UserPromptSubmit` hook capturing prompts.
- `mcp_tool.rs` — in-process MCP server exposing a tool.
- `permissions.rs` — `can_use_tool` callback gating tool invocations.

## Development

Full gate (tests + clippy + fmt + docs) via:

```bash
just check
```

The Monday upstream-watch ritual lives in
[`../../CLAUDE.md`](../../CLAUDE.md) — scan Python `claude-agent-sdk`
for new ideas worth pulling in (forge-native, not 1:1 parity).

## Licence

MIT.
