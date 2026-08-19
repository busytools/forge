# What forge is

forge is a Rust workspace that drives Anthropic's `claude` CLI. It is
two things sharing a repository:

- **A multi-session terminal UI.** One `forge` process holds many
  `claude` sessions across several projects and several accounts, with
  a launchpad, per-session chat, a projects pane and an inspector. The
  full set of surfaces it can render is catalogued in
  [`docs/forge-map.html`](https://github.com/busytools/forge/blob/main/docs/forge-map.html),
  which is kept in step with the code.
- **An SDK for the `claude` CLI.** `forge-sdk` spawns the binary and
  speaks its stream-json protocol over stdio: the codec, the transport,
  control-request dispatch, and an in-process MCP host.

forge never calls the Anthropic API itself. It spawns `claude` and
talks to it, so the CLI stays the thing that runs the agent loop.

## It is not a port of the Python SDK

forge-sdk and Anthropic's Python `claude-agent-sdk` wrap the same
binary. That is the whole of the relationship. They share a wire
contract with `claude` and nothing else: no shared API shape, no
parity target, no matching method names.

The practical consequence is that forge-sdk's public surface is
whatever serves the rest of this workspace, and it changes when that
need changes. If you are looking for a drop-in Rust equivalent of the
Python SDK, this is not that.

What *is* fixed is the wire. What forge writes to the CLI's stdin and
reads from its stdout has to be what `claude` expects, and a difference
there is a bug. The [wire contract](./wire-contract.md) page covers how
that is enforced.

## Scope, honestly

forge was written for one person's use across a few machines. That
shows in places: development is macOS-first, the security model assumes
a single trusted user, and the configuration file is hand-authored
rather than managed through the UI. It is open source because the code
may be useful to read or build on, not because it has been generalised
for arbitrary deployments.

Two behaviours are worth knowing before you run it:

- forge starts a local man-in-the-middle HTTPS proxy and routes every
  `claude` child through it. It terminates TLS for every host the child
  reaches, not only Anthropic, and anything the child spawns inherits
  it. forge refuses to boot if that proxy cannot start. See
  [the proxy page](./classification-proxy.md).
- forge takes an exclusive lock on a config directory, so a second
  forge on the same config directory is normally refused at boot. The
  guard is best-effort and warns rather than failing if it cannot be
  established.

## Where to go next

- [Install and build](./install.md) to get it running.
- [forge.toml reference](./configuration.md) for the one file forge
  reads for configuration.
- [Architecture](./architecture.md) for how the crates fit together.
- [Contributing](./contributing.md) if you want to send a patch.
