# Architecture

Eleven crates, layered so the dependency graph stays acyclic.

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

| Crate                | What it owns                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| -------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `forge-primitives`   | Every type that crosses a crate boundary: message envelopes, content blocks, hook and permission payloads, IDs, render-side view structs. No logic, no I/O, no async.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| `forge-capture`      | Microphone capture: the input device, the input plan, the downmix and resampler, and the mono model-rate audio a host reads from it. A leaf: it depends on no forge-* crate and knows nothing about a host, a take or a wire, so a client's own shell captures through it and `forge-dictate` takes its sample rate.                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| `forge-dictate`      | The dictation primitive: audio in, text out. Owns its model files, speech recognition and transcript normalization, and takes audio as mono frames a host pushes in. Its one forge-* dependency is the capture leaf, for the sample rate, and it knows nothing about the program embedding it.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| `forge-system-one`   | The System One decision-model client: the `[systemone]` config shape, the wire question/answer types and their validation, the HTTP call with its timeout and error mapping. A leaf: depends on no forge-* crate, and its types stay there for consumers to import.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `forge-gateway`      | The account pool: one backend per provider token (credential resolution, the usage probe's HTTP and payload mapping, billing shape), plus account selection by declared models, account health, probe scheduling and backoff. Depends on forge-primitives only; the `claude --version` user agent and TLS-trust plumbing arrive through the host port forge-agent implements.                                                                                                                                                                                                                                                                                                                                                                                                          |
| `forge-connectors`   | One module per inbound connector: the stream client, REST lookups, subscription matching and subsystem pump for one external integration (Gotify and Slack today). Depends on forge-primitives only; Gotify's workspace state and message dispatch arrive through the host port forge-workspace implements.                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| `forge-sdk`          | The `claude` subprocess. Stream-json codec, transport, control dispatch, the in-process MCP host, and the options builder.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| `forge-agent`        | Drives one SDK client behind a channel-based `Agent` and `AgentHandle`. Owns user-data reads, cloud calls, environment probes, event translation and tooling. Async, may shell out.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `forge-workspace`    | The multi-session orchestrator and the server's single point of contact. Owns `forge.toml` loading, `DomainSession`, per-session actors, the machine-local state store, the in-process MCP server forge exposes to every spawned session, and the output-tail reader a call's own read answers from.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| `forge-server`       | The server: what a client reads of the core, and no view of its own. The read surface a client uses, the session records as a view sees them, the peer envelope parsing, forge's own slash commands, the transcript fold that says where a conversation's turns sit and the task endings it carries, the sub-agent instance list the session task's own fold keeps and pushes, the `ReadCallOutput` verb (with the workspace's tail reader re-exported for views), and the socket that carries all of it. It also ships the `forge` binary - the whole core booted through the one boot function (`forge_server::boot`). Nothing here draws, so another client attaches beside the one that exists rather than duplicating it. The name is this crate; forge's in-process MCP server is unrelated and is named as the `forge` MCP server. |
| `forge-test-harness` | The wire-conformance harness. Replay tests plus opt-in live capture. Dev tooling, not in the runtime path.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |

The client - a Svelte page inside a Rust shell - lives in `client/`,
its own workspace root, out of process entirely: it draws off the socket
and reaches the core only through it.

## Where new code goes

Work top-down; the first match wins.

1. **Audio, speech recognition, or turning either into text** (model
   fetch, transcription, transcript normalization) goes in
   `forge-dictate`. It leans on the capture leaf for the sample rate and
   on nothing else of the workspace, and its own types stay there even once
   another crate reads them. **The microphone itself** - opening a
   device, negotiating its config, downmixing and resampling, handing
   out frames - goes in `forge-capture`, a leaf a client's own shell can
   depend on without carrying dictation's models.
2. **System One decision-model I/O** (the `[systemone]` config shape,
   the wire question and answer types with their validation, the HTTP
   call) goes in `forge-system-one`. A leaf: it may not depend on any
   forge-* crate, and its own types stay there even once another crate
   reads them.
3. **A type that crosses a crate boundary** (an envelope, a snapshot
   struct, a hook payload, anything sent over a channel or touched by
   more than one crate) goes in `forge-primitives`. Data shapes only.
4. **Provider credential resolution, the usage probe, payload-to-snapshot
   mapping, billing shape or repair policy** goes in `forge-gateway`,
   as one backend per provider token. **Account selection, account
   health, probe scheduling or backoff** goes there as well: the gateway
   owns the account pool, and the workspace drives it.
5. **Inbound connector work for an external integration** (its stream
   client, REST lookups, subscription matching) goes in
   `forge-connectors`, one module per connector. The connector holds no
   workspace state; Gotify reaches the workspace through the
   `GotifyHost` port that forge-workspace implements, and Slack holds
   only its Web API client.
6. **Anything that speaks stream-json to the subprocess** (a decoder, a
   new control-request subtype, transport, the MCP host, the options
   builder) goes in `forge-sdk`, and ships with a wire-conformance
   scenario.
7. **Live state about the user's environment** (git watching, cwd
   resolution, environment probes, OAuth, plugins, settings I/O,
   plugin catalog scans) goes in `forge-agent`.
8. **Orchestration across projects, sessions, accounts, `forge.toml`
   or the command bus** goes in `forge-workspace`. A read a VIEW needs
   is a verb on the view surface below, not a bare method.
9. **A session record as a view sees it, or a decision any view would
   make over one** (the render-ready record, the reducer that derives
   it, the transcript fold that says where a conversation's turns sit, the
   peer envelope parsing) goes in `forge-server`. The
   test is "does this render?" - if it does, it is the view's.
10. **A widget, screen, layout or per-view presentation state** goes in
    the client: draw it in `client/src/`, and take anything that needs
    the OS, a process or the socket to the shell behind the typed invoke
    surface. A further Rust view is a crate of its own, built against
    the socket in `forge-server` rather than against the core, and a
    read of the core goes through the view surface in `forge-server`,
    never through `forge-workspace`.
11. **A wire-conformance scenario** goes in `forge-test-harness`.

**The view surface is built, reads and writes.** A view reads the core
through named verbs by subject - `roster`, `session`, `agents`,
`accounts`, `plugins`, `extensions`, `reviews`, `workers`, `connectors`,
`dictate`,
`dictate_models`, `cli_version`, `conversation`, `slash_commands`,
`forge_commands`, `subagents`, `subagent_cards`,
`file_index`, `walk_file_index`, `respect_gitignore`, `header`, `mcp_servers`,
`processes`, `work`, `board`, `background_tasks`, `monitors`, `pending_asks`,
`fatal_error`, `service_status`
and `usage` - receives changes through
`subscribe()`, and acts through `dispatch()`, which is a verb rather than
an accessor so a view is handed the commands it needs and not the whole
core. All of them exist in `forge-server`, and the client reads
its project roster and agent rows, the
task board's fleet and per-project rows through `board`, the
account pool, the worker registry, connector subscriptions and dictation
state, the claude version through `cli_version`, its
composer's data through `slash_commands`, `forge_commands`, `subagents`,
`file_index` and `respect_gitignore`, and the conversation,
header, inspector and the asks it answers through
`conversation`, `header`, `mcp_servers`, `processes`, `monitors` and
`pending_asks`, and dispatches its composer's send, its prompt answers and
its take's controls. `walk_file_index` is the one walk on the
surface: a page that holds no seat has no loop behind it. `subagents` is the
CLI's catalogue of the agent types that exist, not a record of the ones
that ran: an instance's cards and the calls under them are folded from
the conversation it appears in. Two refreshes ask the core for a new
snapshot - `refresh_context_usage`, asked on a seat's read, on a turn
finishing on a seat a page holds, and on a compaction settling, and
`refresh_mcp_snapshot`, asked on the seat's read when it reports no
snapshot. A read a further view
would want goes on that surface; a read only one view makes stays a plain
method.

Splits across several crates are normal; a git-diff feature naturally
touches agent, workspace and server. The rule of thumb is that logic, I/O
and subprocess work belong in the agent layer, cross-crate shapes in
primitives, multi-session state in workspace, a session record as a
view sees it in `forge-server`, and anything a person sees in the
client. When in doubt between a shallow crate and a deeper one,
push it down.

Five patterns get caught in review repeatedly:

- **Work in a view that belongs behind the port.** A page draws;
  anything needing the OS, a process or the socket belongs on the
  shell's side, behind the typed invoke surface. In Rust, spawning a
  subprocess belongs in `forge-agent`, exposed as a workspace method.
- **Adding a `SessionUpdate` variant for data that never leaves one
  view.** If both producer and consumer are the same view, use a
  separate channel.
- **Defining the same shape in two crates.** Lift it to primitives, or
  import the re-export.
- **Provider dispatch outside `forge-gateway`.** A match on
  `Provider` in workspace or a view is the thing `forge-gateway` exists
  to delete.
- **Reaching around the command bus for a user action.** User-initiated
  actions go through `dispatch`; query-style refreshes are direct
  methods.

## The server stack and the client stack

Two stacks, and one test decides which side a thing is on: **does removing
it change what the data IS, or only how it is DRAWN?** Only drawn means it
belongs to the client.

- **The server stack** is `forge-server` and everything under it: what a
  client reads of the core, the records as a view sees them, the folds, the
  working tree as state, and the socket that carries it. It is what a client
  cannot work out for itself, and it is the same for every client.
- **The client stack** is whatever draws: the Svelte page in `client/`
  today, whatever lands
  beside it after. Glyphs, colours, weights, spacing, an order chosen for
  display, the label a row is spelled with, the stripping of a command's
  escape sequences - any choice about appearance rather than a fact about
  the session.

Three things hold at the line:

- **The client relies on the server completely, and `forge.toml` stays the
  single source of truth.** So the server MAY hold a catalogue, a set, an
  index or a setting that a client reads - that is the server doing its job.
  What it may NOT hold is the decision about how any of it appears.
- **The server owns no git-level presentation.** It carries the working tree
  as state - the branch, what changed, and the changed files' raw hunks -
  and nothing that renders any of it: no colouring, no tree, no folding, no
  highlighted excerpt. A hunk crosses as lines with their kinds and line
  numbers, which is what changed; how any of it appears is the client's.
- **A presentation the client needs moves into the client; it is never
  dropped.** A thing both need keeps its home on the server with only the
  presentation half leaving, so the server stays the full statement of
  what the data is.

## The view contract

One entry point in each direction, and the workspace's stream fans out
to however many views subscribe.

```
the client  --  Workspace::dispatch(Command)  ->  forge-workspace
the client  <-  SessionUpdate via subscribe()  --  forge-workspace
```

`subscribe()` hands every caller a stream of its own, so a second view
attaches beside the one that exists rather than being refused the one
receiver. A stream carries what the workspace emits after that call, so
a view that attaches late is handed no backlog, and a view that drops
its subscription leaves the fan-out. The first caller to attach is handed
what was emitted before it as well, which is how a notice raised during
boot reaches a view.

A subscriber declares whether it can answer the workspace's prompts.
A view that renders a dock does, so a permission or question request
delivered to it keeps its turn alive; a consumer that only reads takes
an observing subscription instead, which for a view is
`subscribe_mirror()` through the surface (`subscribe_observer()` is the
same stream for an observer that wants the pre-attach backlog), and a
request that reaches no answering subscriber is resolved `Cancelled`
rather than parked on a reply nobody will send.

That is the whole contract. There are no callback hooks and no shared
mutable state. Query-style refreshes such
as context usage and the MCP snapshot are plain
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
bookkeeping the session actors need; the operational state a view
renders lives on the session record `forge-server` derives for it.

One thing about this boundary surprises people: **`forge-workspace` is a
thin facade, not strong isolation.** The boundary is enforced by the
dependency graph (the client is out of process entirely), not by
visibility: workspace re-exports the `forge-agent` submodules the server
reads, so the server sees the agent's surface under the workspace's
name. Do not read an isolation guarantee into it.

## The in-process MCP server

forge exposes one MCP server, named `forge`, to every spawned session.
It is not a subprocess: it is hosted inside forge and reached over the
CLI's own MCP transport. Its tools are grouped by submodule and render
to the model as `mcp__forge__<group>__<tool>`, with nine groups today:
`agents`, `browser`, `documents`, `review`, `cron`, `tasks`, `gotify`,
`slack` and `systemone`.

`browser` is the exception to the render pattern, crossing unprefixed:
its tools are upstream `@playwright/mcp`'s own -
`mcp__forge__browser_navigate`, not `mcp__forge__browser__navigate` -
which is what keeps a prompt written against a Playwright MCP server
resolving here unchanged. Every other group renders in full, the
document hand-off included: `documents__hand_off` crosses as
`mcp__forge__documents__hand_off`.

`browser`, `documents`, `review`, `cron`, `tasks`, `gotify` and `slack`
are registered for every session; `systemone` joins them when
`[systemone]` is configured and enabled. A worker's families can be
narrowed at spawn - `agents__spawn`'s `mcp_families` list, revised by
`agents__update` and stored on the worker's durable record - and each
withheld family is named in that worker's own prompt. The `agents` group
is always on, and so are `browser` and `documents`: the browser is one
machine-global client rather than a project's scope - the session asks
the client that holds the browser role, and with none connected the tool
answers the named error - and a hand-off reads the caller's own tree or
its project's `vault_roots`, so there is nothing a project could withhold
of either. Two groups vary by session kind: inside `agents`, any session
may `list`, `tell`, `ask` and read its own identity, while the four verbs
that act on the caller's own project - `spawn`, `despawn`, `update` and
`capacity` - are lead-only; and inside `tasks`, `tasks__approve` is the
lead's own door (it attests a look the user already gave) and is not
offered to a worker. Reach is the same for both: a target is a
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
being _replaced_, because `flock` binds to the inode rather than the
path. That is why it lives under forge's machine-local
application-support directory at `locks/<hash>.lock` and not in the
config directory: a sync tool that replaces the file
by rename would swap the lock out from under a running process on
another machine.

## The UI surface pages

The [client's pages](./ui/client/index.md) live under `ui/client/` and
are the visual reference for every surface the client can currently
draw, with the drawing each surface is held against beside it; the
mark-provenance sheets stay in `docs/mockups/`. They are
scoped to current state only, and a surface's page lands in the same
change as the surface.

## The client's standard

The client under `client/` is a Svelte app that connects to a running
forge and draws it. It is gated like the Rust side rather than beside
it: `just check` runs the shell crate's rustfmt check
(`client-tauri-fmt`), Prettier, ESLint on typescript-eslint's
type-checked configs, `svelte-check`, `tsc --noEmit`, then vitest, then
the shell crate's clippy and tests (`client-tauri-clippy` and
`client-tauri-test`), then its android warnings check
(`client-android-warnings`: the
crate compiled for `aarch64-linux-android` with warnings denied, which is
what catches desktop-only code going dead on Android),
so one command decides both stacks and its verdict line names the first
failing step whichever side it is on.

The shell under `client/src-tauri/` is its own workspace root, so
`just check`'s workspace-side steps never reach it; CI compiles it in the
`client-tauri` job (its clippy and tests), and the `cargo fmt --check`
job's second invocation carries its rustfmt check.
`client-tauri-fmt`, `client-tauri-clippy`, `client-tauri-test`
and `client-android-warnings` are the steps of `just check` that do, and
the shipping build is still outside both gates. The Unicode punctuation
gate, which CI runs too, and the client's Prettier step do.
`just client-tauri-check` builds the shell in the shipping configuration
and `just client-tauri-bundle` adds the bundles, and the first is the one
to run before handing over a change there.

The rules a gate cannot see are the same shape as the ones above. The
denied constructs are `any`, non-null assertion, `@ts-ignore`,
`innerHTML`, `eval` and floating promises, and a waiver carries its
reason the way an `#[allow]` does. The form is not free: a hook in this
repo blocks an inline suppression comment in TypeScript, so a waiver
there is a scoped entry in `client/eslint.config.js` - and fixing the
code so it needs none comes first. `client/tsconfig.json` keeps
`noUncheckedIndexedAccess`, `noImplicitReturns`, `noUnusedLocals`,
`noUnusedParameters` and `exactOptionalPropertyTypes`, because
`noImplicitReturns` being off is what once let a lifecycle fall out of a
match and return `undefined`, which threw at render instead of failing
the build.

An unknown value from the wire is narrowed once, where it enters, under
`client/src/wire/`, so every union downstream stays exhaustive and a
variant the server adds is a compile error rather than a render crash.
Every state a page can be in - loading, empty, failed, unknown - has a
rendering, and a page that draws a healthy state for an unknown one is
a defect rather than a gap. Accessibility is a rule: semantic markup,
every interactive element reachable by keyboard, no colour as the only
carrier of a state, and axe over the rendered markup as a page test.
The design skills are picked up when the work is something a person will
look at, and before the code is written rather than at review time:
`frontend-design` to originate a look that has no drawing behind it, and
`ui-ux-pro-max` with its family for the quality checklist, the tokens and
the component specs. The order of authority is the mockup, then this
standard, then the skills' generic guidance - a salvaged or approved look
is not re-litigated by a database of styles - and that order holds
whether or not they are installed, since they are user-level plugins
rather than something this repo carries.

**And the shipped app is a shell.** Its only input is the server URL: it
carries no fixture, no mock data and no dev-only default, opens on the
last address a connection was kept on and shows the connect screen when
none does, and never falls back to bundled data. The fixtures live under
`client/src/dev/` and `client/src/dev/fixture.test.ts` builds the app and
fails if one reaches the bundle.
