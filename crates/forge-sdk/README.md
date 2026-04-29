# forge-sdk

A peer reference implementation in Rust of a client for Anthropic's
`claude` CLI. Spawns the binary, speaks stream-json over stdio,
exposes the agentic surface as typed Rust messages + commands. Wire
contract with the CLI is the only hard external invariant; API shape
is whatever serves [`forge-daemon`](../forge-daemon) and
[`forge-tui`](../forge-tui) best.

Not a Python-parity port. See [`PARITY.md`](../../PARITY.md) for the
history of the parity-tracking era and the
[`CLAUDE.md`](../../CLAUDE.md) workspace guide for the current
direction.

- Release history: [`../../docs/CHANGELOG.md`](../../docs/CHANGELOG.md).

## Public surface

Core types and functions exposed from the crate root:

- **Entry points** — `Client::spawn(options)`,
  `Client::spawn_with_transport(options, transport)`, `query()`,
  `query_stream()`.
- **Transport extension** — `pub trait Transport` for injecting
  custom I/O (e.g. wire-recording in `forge-test-harness`).
  `Subprocess` is the shipped in-process implementation.
- **Messages** — `Message` enum (variants for Assistant, User,
  System, Result, TaskStarted/Progress/Notification, RateLimitEvent,
  MirrorError, StreamEvent), `AssistantEnvelope`, `UserEnvelope`,
  `RateLimitInfo`, `StopReason`, `TaskUsage`, `Usage`.
- **Content blocks** — `ContentBlock` enum
  (Text / Thinking / ToolUse / ToolResult / ServerToolUse /
  ServerToolResult).
- **Options + config** — `Options`, `OptionsBuilder`,
  `PermissionMode` (6 variants), `SystemPromptKind`, `ThinkingConfig`,
  `ToolsPreset`, `SettingSource`, `SdkPluginConfig`, `SdkBeta`,
  sandbox types.
- **Permissions** — `CanUseToolCallback`, `PermissionDecision`,
  `PermissionUpdate`, `ToolPermissionContext`.
- **Hooks** — `HookCallback`, `HooksBuilder`, `HookDecision`,
  11 `*Input` structs + 7 `*SpecificOutput` structs.
- **MCP hosting** — `mcp::{McpServer, McpServerBuilder, Tool,
  ToolInput, ToolOutput}` + `tool!` declarative macro.
- **Session storage** — `SessionStore` trait,
  `MemorySessionStore` (alias: `InMemorySessionStore`),
  `FsSessionStore`, `SessionKey`, `SessionStoreEntry`,
  `SessionSummaryEntry`, `fold_session_summary`,
  `summary_entry_to_sdk_info`.
- **Session scanning / mutations** — `session::scan::*`
  (list_sessions, get_session_info, get_session_messages,
  list_subagents, get_subagent_messages, project_key_for_directory)
  + `session::mutations::*` (rename / tag / delete / fork) +
  `session::via_store::*` (async `_from_store` /
  `_via_store` variants).
- **Testing harness** — `forge_sdk::testing::run_session_store_conformance`
  — 14-contract conformance suite third-party store adapters call
  to certify their implementations.
- **Agents** — `agents::AgentDefinition`.
- **Errors** — `Error` enum (variants mirror Python's
  `CLIConnectionError` / `CLINotFoundError` / `ProcessError` /
  `CLIJSONDecodeError` / `MessageParse` families).

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

## Streaming variant

```rust,ignore
use forge_sdk::{query_stream, OptionsBuilder};
use tokio_stream::StreamExt;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let stream = query_stream("What is 2+2?", Some(OptionsBuilder::new().build()));
    tokio::pin!(stream);
    while let Some(item) = stream.next().await {
        println!("{:?}", item?);
    }
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
