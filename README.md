# forge

A Rust workspace for building Claude-assisted tooling. The first deliverable is
[`forge-sdk`](crates/forge-sdk) — a feature-parity Rust port of Anthropic's
[`claude-agent-sdk`](https://github.com/anthropics/claude-agent-sdk-python).

## Status

- **v0.1.64** — `forge-sdk` at full feature + behavioural parity with
  Python `claude-agent-sdk` v0.1.64. 764 tests + 107 ignored green; 14/14
  in-scope Python test files mirrored; only remaining parity gap is
  `AsyncHookJSONOutput` out-of-band delivery (upstream-blocked). See
  [`PARITY.md`](PARITY.md) for the full parity log and
  [`docs/forge-sdk-parity-map.html`](docs/forge-sdk-parity-map.html) for
  the interactive surface map.
- `forged` (daemon) and `forge-tui` (terminal client) will land as sibling
  crates in later milestones.

## Crates

| Crate | Description | Status |
|---|---|---|
| [`forge-sdk`](crates/forge-sdk) | Rust port of `claude-agent-sdk` | v0.1.64 parity-complete |

## Development

Requires nightly Rust pinned via `rust-toolchain.toml`.

```bash
just check               # all tests + clippy + fmt + docs in one shot
cargo nextest run
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Parity with Python upstream is verified weekly — see
[`docs/parity-check.md`](docs/parity-check.md).

## Licence

MIT.
