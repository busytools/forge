# Architecture

Nine crates, layered so the dependency graph stays acyclic.

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

| Crate | What it owns |
|---|---|
| `forge-primitives` | Every type that crosses a crate boundary: message envelopes, content blocks, hook and permission payloads, IDs, render-side view structs. No logic, no I/O, no async. |
| `forge-dictate` | The dictation primitive: audio in, text out. Owns its model files, speech recognition and transcript normalization. Depends on no forge-* crate and knows nothing about the program embedding it. |
| `forge-providers` | One backend per provider token: credential resolution, the usage probe's HTTP and payload mapping, billing shape, the OpenRouter model catalog. Depends on forge-primitives only; keychain, the `claude --version` user agent and TLS-trust plumbing arrive through the host port forge-agent implements. |
| `forge-connectors` | One module per inbound connector: the stream client, REST lookups, subscription matching and subsystem pump for one external integration (Gotify today). Depends on forge-primitives only; workspace state and message dispatch arrive through the host port forge-workspace implements. |
| `forge-sdk` | The `claude` subprocess. Stream-json codec, transport, control dispatch, the in-process MCP host, and the options builder. |
| `forge-agent` | Drives one SDK client behind a channel-based `Agent` and `AgentHandle`. Owns user-data reads, cloud calls, environment probes, event translation and tooling. Async, may shell out. |
| `forge-workspace` | The multi-session orchestrator and the TUI's single point of contact. Owns `forge.toml` loading, `DomainSession`, per-session actors, the machine-local state store, and the in-process MCP server forge exposes to every spawned session. |
| `forge-tui` | The view layer. Rendering, key and mouse handling, per-session presentation state. Ships the `forge` binary. |
| `forge-test-harness` | The wire-conformance harness. Replay tests plus opt-in live capture. |

Only `forge-tui` produces a binary. The dependency direction is
enforced by the manifests: `forge-tui` has no `forge-agent` dependency
at all, so it cannot reach the agent layer except through
`forge-workspace`.

## Where new code goes

Work top-down; the first match wins.

1. **Audio, speech recognition, or turning either into text** (capture,
   model fetch, transcription, transcript normalization) goes in
   `forge-dictate`. It is a leaf: it may not depend on any forge-*
   crate, and its own types stay there even once another crate reads
   them.
2. **A type that crosses a crate boundary** (an envelope, a snapshot
   struct, a hook payload, anything sent over a channel or touched by
   more than one crate) goes in `forge-primitives`. Data shapes only.
3. **Provider credential resolution, the usage probe, payload-to-snapshot
   mapping, billing shape or repair policy** goes in `forge-providers`,
   as one backend per provider token.
4. **Inbound connector work for an external integration** (its stream
   client, REST lookups, subscription matching) goes in
   `forge-connectors`, one module per connector. The connector holds no
   workspace state; what it needs from the workspace arrives through
   the `GotifyHost` port that forge-workspace implements.
5. **Anything that speaks stream-json to the subprocess** (a decoder, a
   new control-request subtype, transport, the MCP host, the options
   builder) goes in `forge-sdk`, and ships with a wire-conformance
   scenario.
6. **Live state about the user's environment** (git watching, cwd
   resolution, environment probes, OAuth, plugins, settings I/O,
   plugin catalog scans) goes in `forge-agent`.
7. **Orchestration across projects, sessions, accounts, `forge.toml`
   or the command bus** goes in `forge-workspace`.
8. **A widget, screen, key binding, mouse handler or per-session
   presentation state** goes in `forge-tui`.
9. **A wire-conformance scenario** goes in `forge-test-harness`.

Splits across several crates are normal; a git-diff feature naturally
touches agent, workspace and TUI. The rule of thumb is that logic, I/O
and subprocess work belong in the agent layer, cross-crate shapes in
primitives, multi-session state in workspace, and only what the user
sees in the TUI. The common mistake is putting too much in
`forge-tui`, so when in doubt, push it down.

Four patterns get caught in review repeatedly:

- **Spawning a subprocess from `forge-tui`.** That belongs in
  `forge-agent`, exposed as a workspace method the TUI awaits.
- **Adding a `SessionUpdate` variant for data that never leaves the
  TUI.** If both producer and consumer are in `forge-tui`, use a
  separate channel.
- **Defining the same shape in two crates.** Lift it to primitives, or
  import the re-export.
- **Provider dispatch outside `forge-providers`.** A match on
  `Provider` in workspace or tui is the thing the provider-backends
  crate exists to delete.
- **Reaching around the command bus for a user action.** User-initiated
  actions go through `dispatch`; query-style refreshes are direct
  methods.

## The TUI and workspace contract

One channel pair, one producer and one consumer in each direction.

```
forge-tui  --  Workspace::dispatch(Command)  ->  forge-workspace
forge-tui  <-  SessionUpdate via subscribe()  --  forge-workspace
```

That is the whole contract. There is no second channel in the design,
no callback hooks, and no shared mutable state. Nothing under
`forge-tui/src` holds a handle into the agent layer, though that crate's
own integration tests do build one directly. Query-style refreshes such
as the status snapshot, context usage and the MCP snapshot are plain
`Workspace` methods rather than command variants, because they are
reads rather than user actions.

`DomainSession`, on the workspace side, keeps only workspace-internal
routing metadata, plus the pending-interaction and turn-state
bookkeeping the session actors need. The operational state the TUI
renders lives on `UiSession`.

Two things about this boundary surprise people, and both are worth
knowing before you change it:

**The update channel doubles as a TUI-internal event bus.** `App`
grabs `Workspace::update_sender()` once at construction and keeps it as
`App.update_tx`; a few `forge-tui` modules then emit their own
`SessionUpdate`s through that field rather than making a command
round-trip. That is an implicit second contract, and a non-TUI frontend
would have to replicate it. It is tracked in
[issue 105](https://github.com/busytools/forge/issues/105) rather than
treated as settled design.

**`forge-workspace` is a thin facade, not strong isolation.** The
boundary is enforced by the dependency graph, not by visibility:
workspace wildcard-re-exports whole `forge-agent` submodules, so the
TUI sees the agent's surface verbatim under a different name. Do not
read "the TUI cannot touch the agent" into it.

## The in-process MCP server

forge exposes one MCP server, named `forge`, to every spawned session.
It is not a subprocess: it is hosted inside forge and reached over the
CLI's own MCP transport. Its tools are grouped by submodule and render
to the model as `mcp__forge__<group>__<tool>`, with five groups today:
`peers`, `workers`, `review`, `cron` and `gotify`.

`review`, `cron` and `gotify` are registered for every session. The
peers-versus-workers split is the part that varies by session kind:
lead sessions get both `peers__*` and `workers__*`, workers get
`workers__*` but not `peers__*`, so cross-project traffic stays the
lead's job.

## Single instance per config directory

One forge process owns one config directory and runs many sessions
inside it. When the guard is in force, a second instance is refused at
boot, naming the holder's PID when it can read one from the lockfile.

The guard is best-effort rather than absolute. If the application-support
directory will not resolve, the `locks/` directory cannot be created,
the lockfile will not open, or `flock` fails for any reason other than
the contended one, forge logs a warning and boots without the
guarantee.

The lock is a non-blocking exclusive `flock` on a dedicated file. The
holder truncates and rewrites its PID into that same file through the
locked descriptor, which is safe; what must never happen is the file
being *replaced*, because `flock` binds to the inode rather than the
path. That is why it lives under forge's machine-local
application-support directory at `locks/<hash>.lock` and not in the
config directory: a sync tool that replaces the file
by rename would swap the lock out from under a running process on
another machine.

## The UI surface map

[`docs/forge-map.html`](https://github.com/busytools/forge/blob/main/docs/forge-map.html)
is the visual reference for every surface `forge-tui` can currently
render, with mockups, glyph tables and colour tables. It is scoped to
current state only. Open it in a browser rather than reading the
source; it is a single self-contained page.
