# What forge is

forge is a Rust workspace that drives Anthropic's `claude` CLI. It is
three things sharing a repository:

- **A multi-session terminal UI.** One `forge` process holds many
  `claude` sessions across several projects and several accounts, with
  a launchpad, per-session chat, a projects pane and an inspector. The
  full set of surfaces it can render is catalogued in the
  [UI surface pages](./ui/workspace.md), which are kept in step with
  the code.
- **An SDK for the `claude` CLI.** `forge-sdk` spawns the binary and
  speaks its stream-json protocol over stdio: the codec, the transport,
  control-request dispatch, and an in-process MCP host.
- **A web view of the same core.** `forge-web` serves the sessions as
  HTML over HTTP, from the process the TUI runs in and on `127.0.0.1`
  by default, so reaching it from another machine is a `[web] bind` line
  rather than a second forge. It serves a wiring-proof page today.

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

One behaviour is worth knowing before you run it:

- forge takes an exclusive lock on a config directory, so a second
  forge on the same config directory is normally refused at boot. The
  guard is best-effort and warns rather than failing if it cannot be
  established.

## API reference

Every crate in the workspace is published as rustdoc at
[the API reference](./rustdoc/), private items included. It is the place
to go when you are reading or changing the code rather than running it.

## Where to go next

- [Install and build](./install.md) to get it running.
- [forge.toml reference](./configuration.md) for the one file forge
  reads for configuration.
- [Architecture](./architecture.md) for how the crates fit together.
- [Contributing](./contributing.md) if you want to send a patch.
