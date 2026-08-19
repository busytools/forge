# Architecture

Six crates, layered so the dependency graph stays acyclic.

```
forge-primitives      leaf: pure data, no logic, no I/O, no async
forge-sdk         ->  primitives
forge-agent       ->  primitives + sdk
forge-workspace   ->  primitives + agent + sdk
forge-tui         ->  primitives + workspace
forge-test-harness->  primitives + sdk
```

| Crate | What it owns |
|---|---|
| `forge-primitives` | Every type that crosses a crate boundary: message envelopes, content blocks, hook and permission payloads, IDs, render-side view structs. No logic, no I/O, no async. |
| `forge-sdk` | The `claude` subprocess. Stream-json codec, transport, control dispatch, the in-process MCP host, the options builder, and the wire-classification proxy. |
| `forge-agent` | Drives one SDK client behind a channel-based `Agent` and `AgentHandle`. Owns user-data reads, cloud calls, environment probes, event translation and tooling. Async, may shell out. |
| `forge-workspace` | The multi-session orchestrator and the TUI's single point of contact. Owns `forge.toml` loading, `DomainSession`, per-session actors, the machine-local state store, and the peer and worker MCP servers. |
| `forge-tui` | The view layer. Rendering, key and mouse handling, per-session presentation state. Ships the `forge` binary. |
| `forge-test-harness` | The wire-conformance harness. Replay tests plus opt-in live capture. |

Only `forge-tui` produces a binary. The dependency direction is
enforced by the manifests: `forge-tui` has no `forge-agent` dependency
at all, so it cannot reach the agent layer except through
`forge-workspace`.

## Where new code goes

Work top-down; the first match wins.

1. **A type that crosses a crate boundary** (an envelope, a snapshot
   struct, a hook payload, anything sent over a channel or touched by
   more than one crate) goes in `forge-primitives`. Data shapes only.
2. **Anything that speaks stream-json to the subprocess** (a decoder, a
   new control-request subtype, transport, the MCP host, the options
   builder) goes in `forge-sdk`, and ships with a wire-conformance
   scenario.
3. **Live state about the user's environment** (git watching, cwd
   resolution, environment probes, OAuth, plugins, settings I/O,
   catalog scans) goes in `forge-agent`.
4. **Orchestration across projects, sessions, accounts, `forge.toml`
   or the command bus** goes in `forge-workspace`.
5. **A widget, screen, key binding, mouse handler or per-session
   presentation state** goes in `forge-tui`.
6. **A wire-conformance scenario** goes in `forge-test-harness`.

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
no callback hooks, and no shared mutable state. The TUI holds no handle
into the agent layer. Query-style refreshes such as the status
snapshot, context usage and the MCP snapshot are plain `Workspace`
methods rather than command variants, because they are reads rather
than user actions.

`DomainSession`, on the workspace side, keeps only routing metadata.
The operational state the TUI renders lives on `UiSession`.

Two things about this boundary surprise people, and both are worth
knowing before you change it:

**The update channel doubles as a TUI-internal event bus.** A few
`forge-tui` modules take `Workspace::update_sender()` and emit their
own `SessionUpdate`s rather than making a command round-trip. That is
an implicit second contract, and a non-TUI frontend would have to
replicate it. It is tracked in
[issue 105](https://github.com/busytools/forge/issues/105) rather than
treated as settled design.

**`forge-workspace` is a thin facade, not strong isolation.** The
boundary is enforced by the dependency graph, not by visibility:
workspace wildcard-re-exports whole `forge-agent` submodules, so the
TUI sees the agent's surface verbatim under a different name. Do not
read "the TUI cannot touch the agent" into it.

## Single instance per config directory

One forge process owns one config directory and runs many sessions
inside it. A second is refused at boot and reports the holder's PID.

The lock is a non-blocking exclusive `flock` on a dedicated file that
is only ever opened and locked, never rewritten in place. It lives
under forge's machine-local application-support directory at
`locks/<hash>.lock`, not in the config directory, because `flock` binds
to the inode rather than the path: a sync tool that replaces the file
by rename would swap the lock out from under a running process on
another machine.

## The UI surface map

[`docs/forge-map.html`](https://github.com/busytools/forge/blob/main/docs/forge-map.html)
is the visual reference for every surface `forge-tui` can currently
render, with mockups, glyph tables and colour tables. It is scoped to
current state only. Open it in a browser rather than reading the
source; it is a single self-contained page.
