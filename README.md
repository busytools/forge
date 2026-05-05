# forge

A Rust workspace for personal-use agentic tooling around Anthropic's
`claude` CLI. forge is a **peer reference implementation** of clients
to that binary — it shares the stream-json wire contract with the CLI,
but otherwise gets to be its own thing (idiomatic Rust, channels-based
concurrency, no Python-parity contract).

See [`PARITY.md`](PARITY.md) for the history of the parity-tracking era
(2026-04-21 → 2026-04-27) and [`CLAUDE.md`](CLAUDE.md) for the current
project guide.

## Crates

The workspace is layered, with strictly acyclic dependencies:

```
forge-primitives ──── leaf (pure data, no logic)
forge-sdk        ──→ primitives
forge-agent      ──→ primitives + sdk
forge-tui        ──→ primitives + agent
```

| Crate | Description |
|---|---|
| [`forge-primitives`](crates/forge-primitives) | Workspace-shared wire-shape types — message envelopes, content blocks, hook/permission/option/subagent data, channel commands, IDs, render-side views. Pure data, no I/O. |
| [`forge-sdk`](crates/forge-sdk) | Wraps the `claude` CLI subprocess. Owns the stream-json codec, transport, control dispatch, in-process MCP host, and the callback registries (Hooks/HooksBuilder, CanUseToolCallback). |
| [`forge-agent`](crates/forge-agent) | Drives one `forge-sdk` Client behind a channel-based `Agent`/`AgentHandle` API. Owns userdata (settings, trust, sessions catalog, memory, plugins), cloud (oauth, usage, account, service status), and env (git context). |
| [`forge-tui`](crates/forge-tui) | Native terminal interface. Consumes `AgentEvent`s, emits `Command`s. No direct dep on `forge-sdk`. |
| [`forge-test-harness`](crates/forge-test-harness) | Wire-conformance harness for `forge-sdk` ↔ `claude` CLI. Replay-based offline tests + opt-in live capture. |

Multiple sessions = multiple `forge` processes (one per tmux/zellij pane).
No daemon, no shared state.

## Development

Requires nightly Rust pinned via `rust-toolchain.toml`.

```bash
just check               # fmt + clippy + nextest + docs in one shot
cargo nextest run
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

## Licence

MIT.
