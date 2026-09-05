# forge

A Rust workspace that drives Anthropic's `claude` CLI: a multi-session
terminal UI, plus an SDK that speaks the CLI's stream-json wire
protocol over stdio.

forge never calls the Anthropic API directly. It spawns `claude` and
talks to it, so the CLI stays the thing that runs the agent loop.

**[Documentation](https://busytools.github.io/forge/)**

## Not a port of the Python SDK

`forge-sdk` and Anthropic's Python `claude-agent-sdk` wrap the same
binary. That is the whole of the relationship: they share a wire
contract with `claude` and nothing else. There is no shared API shape
and no parity target. What is fixed is the wire, and a difference
between what forge writes to the CLI's stdin and what `claude` expects
is a bug.

## Crates

Strictly acyclic:

```
forge-primitives      leaf: pure data, no logic, no I/O, no async
forge-dictate         leaf: dictation, depends on no forge-* crate
forge-providers   ->  primitives
forge-connectors  ->  primitives
forge-sdk         ->  primitives
forge-agent       ->  primitives + sdk + providers
forge-workspace   ->  primitives + agent + sdk + dictate + providers + connectors
forge-tui         ->  primitives + workspace
forge-test-harness->  primitives + sdk
```

| Crate | Description |
|---|---|
| [`forge-primitives`](crates/forge-primitives) | Every type that crosses a crate boundary: message envelopes, content blocks, hook and permission payloads, IDs, render-side views. Pure data. |
| [`forge-dictate`](crates/forge-dictate) | The dictation primitive: audio in, text out. Owns its model files, speech recognition and normalization. Depends on no forge-* crate and knows nothing about a host. |
| [`forge-providers`](crates/forge-providers) | One backend per provider token: credential resolution, the usage probe's HTTP and payload mapping, billing shape, the OpenRouter model catalog. Depends on forge-primitives only. |
| [`forge-connectors`](crates/forge-connectors) | One module per inbound connector: the stream client, REST lookups and matching for one external integration (Gotify today). Depends on forge-primitives only. |
| [`forge-sdk`](crates/forge-sdk) | The `claude` subprocess. Stream-json codec, transport, control dispatch, in-process MCP host, options builder. |
| [`forge-agent`](crates/forge-agent) | Drives one SDK client behind a channel-based `Agent` and `AgentHandle`. User-data reads, cloud calls, environment probes, event translation, tooling. |
| [`forge-workspace`](crates/forge-workspace) | Multi-session orchestrator and the TUI's single point of contact. Owns `forge.toml`, per-session actors, the machine-local state store, and the in-process MCP server forge exposes to every spawned session. |
| [`forge-tui`](crates/forge-tui) | The view layer, and the `forge` binary. Rendering, input handling, per-session presentation state. No direct `forge-agent` dependency. |
| [`forge-test-harness`](crates/forge-test-harness) | Wire-conformance harness: replay-based offline tests plus opt-in live capture. |

[`docs/forge-map.html`](docs/forge-map.html) is the visual map of every
surface the TUI can currently render. Open it in a browser.

## Getting started

Requires nightly Rust at the date pinned in `rust-toolchain.toml`
(rustup applies it automatically), the `claude` CLI, `cargo-nextest`,
and a native toolchain for the dictation model runtimes - a C and C++
compiler, `cmake`, `libclang`, and ALSA headers on Linux. The
[install page](docs/book/src/install.md) has the per-platform packages.

```bash
just check      # fmt + unicode gate + clippy + nextest + docs
just install    # build and install the `forge` binary
```

forge reads exactly one configuration file,
`<config_dir>/forge/forge.toml`, and needs it before it will start. The
[configuration reference](https://busytools.github.io/forge/configuration.html)
documents every key and has a complete example.

## One thing to know before running it

forge takes an exclusive lock on a config directory, so a second forge
on the same config directory is normally refused at boot, naming the
holder's PID. The guard is best-effort and warns rather than failing if
it cannot be established.

## Scope

forge was written for one person's use across a few machines.
Development is macOS-first: OAuth credentials are read from the macOS
Keychain behind a `cfg` gate, and on other targets that reader returns
nothing. It is open source because the code may be useful to read or
build on, not because it has been generalised.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## Licence

MIT. See [LICENSE](LICENSE).
