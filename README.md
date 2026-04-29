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

| Crate | Description |
|---|---|
| [`forge-sdk`](crates/forge-sdk) | Wraps the `claude` CLI as a structured-message API. |
| [`forge-daemon`](crates/forge-daemon) | Multiplexes WS clients onto SDK sessions over JSON-RPC. Runs as launchd. |
| [`forge-tui`](crates/forge-tui) | Optional terminal client over WS. |
| [`forge-test-harness`](crates/forge-test-harness) | Wire-conformance harness (SDK ↔ CLI, daemon ↔ client). |

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
