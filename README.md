# forge

A Rust workspace that drives Anthropic's `claude` CLI: a server that
runs multi-session orchestration, the clients that draw it, plus an SDK
that speaks the CLI's stream-json wire
protocol over stdio.

forge never calls the Anthropic API directly. It spawns `claude` and
talks to it, so the CLI stays the thing that runs the agent loop.

**[Documentation](https://busytools.github.io/forge/)** - **[API reference](https://busytools.github.io/forge/rustdoc/)**

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
forge-capture         leaf: microphone capture, depends on no forge-* crate
forge-dictate     ->  capture
forge-system-one      leaf: decision-model client, depends on no forge-* crate
forge-gateway     ->  primitives
forge-connectors  ->  primitives
forge-sdk         ->  primitives
forge-agent       ->  primitives + sdk + gateway
forge-workspace   ->  primitives + agent + sdk + dictate + gateway + connectors
forge-server      ->  primitives + workspace + sdk
forge-test-harness->  primitives + sdk
```

| Crate | Description |
|---|---|
| [`forge-primitives`](crates/forge-primitives) | Every type that crosses a crate boundary: message envelopes, content blocks, hook and permission payloads, IDs, render-side views. Pure data. |
| [`forge-capture`](crates/forge-capture) | Microphone capture: the input device, the input plan, the downmix and resampler, and the mono model-rate audio a host reads from it. A leaf: depends on no forge-* crate, so a client's own shell captures through it and `forge-dictate` takes its sample rate. |
| [`forge-dictate`](crates/forge-dictate) | The dictation primitive: audio in, text out. Owns its model files, speech recognition and normalization, and takes audio as mono frames a host pushes in. Its one forge-* dependency is the capture leaf, for the sample rate, and it knows nothing about a host. |
| [`forge-system-one`](crates/forge-system-one) | The System One decision-model client: the `[systemone]` config shape, the wire question/answer types with their validation, and the HTTP call with its timeout and error mapping. Depends on no forge-* crate. |
| [`forge-gateway`](crates/forge-gateway) | The account pool: one backend per provider token (credential resolution, the usage probe's HTTP and payload mapping, billing shape), plus account selection by declared models, account health, probe scheduling and backoff. Depends on forge-primitives only. |
| [`forge-connectors`](crates/forge-connectors) | One module per inbound connector: the stream client, REST lookups and matching for one external integration (Gotify and Slack today). Depends on forge-primitives only. |
| [`forge-sdk`](crates/forge-sdk) | The `claude` subprocess. Stream-json codec, transport, control dispatch, in-process MCP host, options builder. |
| [`forge-agent`](crates/forge-agent) | Drives one SDK client behind a channel-based `Agent` and `AgentHandle`. User-data reads, cloud calls, environment probes, event translation, tooling. |
| [`forge-workspace`](crates/forge-workspace) | Multi-session orchestrator and the server's single point of contact. Owns `forge.toml`, per-session actors, the machine-local state store, the in-process MCP server forge exposes to every spawned session, and the output-tail reader a call's own read answers from. |
| [`forge-server`](crates/forge-server) | The server: what a client reads of the core, and no view of its own. The read surface a client uses, the session records as a view sees them, the peer envelope parsing, forge's own slash commands, the transcript fold that says where a conversation's turns sit and the task endings it carries, the sub-agent instance list the session task's own fold keeps and pushes, the `ReadCallOutput` verb (with the workspace's tail reader re-exported for views), the boot path the `forge` binary runs, and the socket that carries all of it. Sits between the workspace and whatever draws. The name is this crate; forge's in-process MCP server is unrelated and is named as the `forge` MCP server. This crate ships the `forge` binary. |
| [`forge-test-harness`](crates/forge-test-harness) | Wire-conformance harness: replay-based offline tests plus opt-in live capture. |

The client lives in `client/` - a Svelte page inside a Rust shell - and
the client part of [the book](https://busytools.github.io/forge/)
is the visual map of every surface it can draw.

## Getting started

Requires nightly Rust at the date pinned in `rust-toolchain.toml`
(rustup applies it automatically), the `claude` CLI, `cargo-nextest`,
and a native toolchain for the dictation model runtimes - a C and C++
compiler, `cmake`, `libclang`, and ALSA headers on Linux. The
[install page](docs/book/src/install.md) has the per-platform packages.

```bash
just check                  # fmt + unicode gate + clippy + nextest + docs
just check-feature-configs  # the configuration `just check` leaves out
just install                # build and install the `forge` binary
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
Development is macOS-first. It is open source because the code may be
useful to read or build on, not because it has been generalised.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## Licence

MIT. See [LICENSE](LICENSE).
