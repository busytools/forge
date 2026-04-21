# forge

A Rust workspace for building Claude-assisted tooling. The first deliverable is
[`forge-sdk`](crates/forge-sdk) — a feature-parity Rust port of Anthropic's
[`claude-agent-sdk`](https://github.com/anthropics/claude-agent-sdk-python).

## Status

- **v0.0.x** — `forge-sdk` under active development. M0 scaffolding + M1 core
  transport in flight.
- `forged` (daemon) and `forge-tui` (terminal client) will land as sibling
  crates in later milestones.

## Crates

| Crate | Description | Status |
|---|---|---|
| [`forge-sdk`](crates/forge-sdk) | Rust port of `claude-agent-sdk` | M0 + M1 in progress |

## Development

Requires nightly Rust pinned via `rust-toolchain.toml`.

```bash
cargo build
cargo nextest run
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

## Licence

MIT.
