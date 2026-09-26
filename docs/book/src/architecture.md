# Architecture

Eleven crates, layered so the dependency graph stays acyclic.

```
forge-primitives      leaf: pure data, no logic, no I/O, no async
forge-dictate         leaf: dictation, depends on no forge-* crate
forge-gateway     ->  primitives
forge-connectors  ->  primitives
forge-sdk         ->  primitives
forge-agent       ->  primitives + sdk + gateway
forge-workspace   ->  primitives + agent + sdk + dictate + gateway + connectors
forge-sessions    ->  primitives + workspace
forge-web         ->  primitives
forge-tui         ->  primitives + workspace + sessions + web
forge-test-harness->  primitives + sdk
```

| Crate | What it owns |
|---|---|
| `forge-primitives` | Every type that crosses a crate boundary: message envelopes, content blocks, hook and permission payloads, IDs, render-side view structs. No logic, no I/O, no async. |
| `forge-dictate` | The dictation primitive: audio in, text out. Owns its model files, speech recognition and transcript normalization. Depends on no forge-* crate and knows nothing about the program embedding it. |
| `forge-gateway` | The account pool: one backend per provider token (credential resolution, the usage probe's HTTP and payload mapping, billing shape), plus account selection by declared models, account health, probe scheduling and backoff. Depends on forge-primitives only; the `claude --version` user agent and TLS-trust plumbing arrive through the host port forge-agent implements. |
| `forge-connectors` | One module per inbound connector: the stream client, REST lookups, subscription matching and subsystem pump for one external integration (Gotify and Slack today). Depends on forge-primitives only; Gotify's workspace state and message dispatch arrive through the host port forge-workspace implements. |
| `forge-sdk` | The `claude` subprocess. Stream-json codec, transport, control dispatch, the in-process MCP host, and the options builder. |
| `forge-agent` | Drives one SDK client behind a channel-based `Agent` and `AgentHandle`. Owns user-data reads, cloud calls, environment probes, event translation and tooling. Async, may shell out. |
| `forge-workspace` | The multi-session orchestrator and the TUI's single point of contact. Owns `forge.toml` loading, `DomainSession`, per-session actors, the machine-local state store, and the in-process MCP server forge exposes to every spawned session. |
| `forge-sessions` | What a view needs and nothing about how it renders: the read surface a view uses, the session records as a view sees them, the peer envelope parsing in both directions, the tool family table, and the policy that folds a run of blocks. Holds no terminal types, so a second view attaches beside the TUI rather than duplicating it. |
| `forge-web` | The web view: HTTP served beside the TUI, from the process that owns the sessions, with a wiring-proof page today. axum plus server-rendered markup. Never names `forge-workspace`, and starts no subsystem of its own. |
| `forge-tui` | The view layer. Rendering, key and mouse handling, per-session presentation state. Ships the `forge` binary. |
| `forge-test-harness` | The wire-conformance harness. Replay tests plus opt-in live capture. Dev tooling, not in the runtime path. |

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
   mapping, billing shape or repair policy** goes in `forge-gateway`,
   as one backend per provider token. **Account selection, account
   health, probe scheduling or backoff** goes there as well: the gateway
   owns the account pool, and the workspace drives it.
4. **Inbound connector work for an external integration** (its stream
   client, REST lookups, subscription matching) goes in
   `forge-connectors`, one module per connector. The connector holds no
   workspace state; Gotify reaches the workspace through the
   `GotifyHost` port that forge-workspace implements, and Slack holds
   only its Web API client.
5. **Anything that speaks stream-json to the subprocess** (a decoder, a
   new control-request subtype, transport, the MCP host, the options
   builder) goes in `forge-sdk`, and ships with a wire-conformance
   scenario.
6. **Live state about the user's environment** (git watching, cwd
   resolution, environment probes, OAuth, plugins, settings I/O,
   plugin catalog scans) goes in `forge-agent`.
7. **Orchestration across projects, sessions, accounts, `forge.toml`
   or the command bus** goes in `forge-workspace`. A read a VIEW needs
   is a verb on the view surface below, not a bare method.
8. **A session record as a view sees it, or a decision any view would
   make over one** (the render-ready record, the reducer that derives
   it, the policy that decides how a run of blocks folds, the peer
   envelope parsing in both directions) goes in `forge-sessions`. The
   test is "does this render?" - if it does, it is the view's.
9. **A widget, screen, key binding, mouse handler or per-session
   presentation state** goes in `forge-tui`.
10. **A view that is not the TUI** - its routes, its markup, its own
    per-view state - goes in `forge-web`, which sits beside `forge-tui`
    on the same core. A read of the core goes through the view surface
    in `forge-sessions`, never through `forge-workspace`.
11. **A wire-conformance scenario** goes in `forge-test-harness`.

**The view surface's read verbs are built.** A view reads the core
through named verbs by subject - `roster`, `session`, `accounts`,
`plugins`, `reviews`, `workers`, `connectors`, `dictate` - acts through
`dispatch(Command)`, and receives changes through `subscribe()`. All
eight exist in `forge-sessions`, and the TUI reads its project roster,
session scan cwd, worker registry, account pool, plugin records, review
threads, connector subscriptions and dictation state through them. What
the migration has not reached is the write half: the five refreshes that
ask the core for a new snapshot are still direct `Workspace` calls, so
`forge-tui` keeps its `forge-workspace` dependency and the arrow above
is not yet one-way. A read a second view would want goes on that
surface; a read only the TUI makes stays a plain method.

Splits across several crates are normal; a git-diff feature naturally
touches agent, workspace and TUI. The rule of thumb is that logic, I/O
and subprocess work belong in the agent layer, cross-crate shapes in
primitives, multi-session state in workspace, a session record as a
view sees it in sessions, and only what the user sees in the TUI. The
common mistake is putting too much in `forge-tui`, so when in doubt,
push it down.

Five patterns get caught in review repeatedly:

- **Spawning a subprocess from `forge-tui`.** That belongs in
  `forge-agent`, exposed as a workspace method the TUI awaits.
- **Adding a `SessionUpdate` variant for data that never leaves the
  TUI.** If both producer and consumer are in `forge-tui`, use a
  separate channel.
- **Defining the same shape in two crates.** Lift it to primitives, or
  import the re-export.
- **Provider dispatch outside `forge-gateway`.** A match on
  `Provider` in workspace or tui is the thing `forge-gateway` exists
  to delete.
- **Reaching around the command bus for a user action.** User-initiated
  actions go through `dispatch`; query-style refreshes are direct
  methods.

## The TUI and workspace contract

One entry point in each direction, and the workspace's stream fans out
to however many views subscribe.

```
forge-tui  --  Workspace::dispatch(Command)  ->  forge-workspace
forge-tui  <-  SessionUpdate via subscribe()  --  forge-workspace
```

`subscribe()` hands every caller a stream of its own, so a second view
attaches beside the TUI rather than being refused the one receiver. A
stream carries what the workspace emits after that call, so a view that
attaches late is handed no backlog, and a view that drops its
subscription leaves the fan-out. The first caller to attach is handed
what was emitted before it as well, which is how a notice raised during
boot reaches a view.

A subscriber declares whether it can answer the workspace's prompts.
The TUI does, so a permission or question request delivered to it keeps
its turn alive; a consumer that only reads takes `subscribe_observer()`
instead, and a request that reaches no answering subscriber is resolved
`Cancelled` rather than parked on a reply nobody will send.

That is the whole contract. There are no callback hooks and no shared
mutable state. Nothing under
`forge-tui/src` holds a handle into the agent layer, though that crate's
own integration tests do build one directly. Query-style refreshes such
as the status snapshot, context usage and the MCP snapshot are plain
`Workspace` methods rather than command variants, because they are
reads rather than user actions.

**Both enums address a session by its slot.** A slot is the
`(org, project, label)` triple `forge-primitives`' `SessionSlot`
carries: it names the seat, and the claude session id names the
occupant. `/new` and `/resume` swap the occupant and leave the slot
alone, so a `Command` carries a slot and a `SessionUpdate` routes on
one; nothing infers a bucket from a wire session id. A session id
survives only where the `claude` CLI or the child's own address needs
it: the `--session-id` / `--resume` arguments, the gateway binding's
URL segment, the stream-json `session_id` field, and the transcript
filenames with the caches that mirror them. `SessionUpdate::Connected`
and `SessionReplaced` also carry one, since they announce a new
occupant: the id is their payload, the slot is still their address.

`DomainSession`, on the workspace side, keeps only workspace-internal
routing metadata, plus the pending-interaction and turn-state
bookkeeping the session actors need. The operational state the TUI
renders lives on `UiSession`.

Two things about this boundary surprise people, and both are worth
knowing before you change it:

**The TUI has an update channel of its own.** `App` mints an
`update_tx` / `update_rx` pair for `forge-tui`'s own async work, and a
few modules (plugin inventory and update runs, slash command executors,
the service-status check, the input-submit cancel path) emit their
presentation events through it rather than making a command round-trip.
Both feeds reach the same reducer, and that pair is not part of what a
frontend has to reproduce: that is `dispatch()` and `subscribe()`.

**`forge-workspace` is a thin facade, not strong isolation.** The
boundary is enforced by the dependency graph, not by visibility:
workspace wildcard-re-exports whole `forge-agent` submodules, so the
TUI sees the agent's surface verbatim under a different name. Do not
read "the TUI cannot touch the agent" into it.

## The in-process MCP server

forge exposes one MCP server, named `forge`, to every spawned session.
It is not a subprocess: it is hosted inside forge and reached over the
CLI's own MCP transport. Its tools are grouped by submodule and render
to the model as `mcp__forge__<group>__<tool>`, with six groups today:
`agents`, `review`, `cron`, `tasks`, `gotify` and `slack`.

`review`, `cron`, `tasks`, `gotify` and `slack` are registered for every
session. The split that varies by session kind is inside `agents`: any session may
`list`, `tell`, `ask` and read its own identity, while the four verbs
that act on the caller's own project - `spawn`, `despawn`, `update` and
`capacity` - are lead-only. Reach is the same for both: a target is a
slot, `(org, project, label)`, so a worker addresses another project's
agent as directly as its own lead.

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

## The UI surface pages

The [UI surface pages](./ui/workspace.md) under `ui/` in this book are
the visual reference for every surface `forge-tui` can currently
render, with mockups, glyph tables and colour tables. They are scoped
to current state only.
