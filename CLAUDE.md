# forge - project guide

A Rust workspace that wraps Anthropic's `claude` CLI in a multi-session
terminal UI. Twelve crates, layered acyclically:

```
forge-primitives ───── leaf (pure data, no logic)
forge-dictate    ───── leaf (dictation; depends on no forge-* crate)
forge-system-one ───── leaf (decision-model client; depends on no forge-* crate)
forge-gateway    ───→ primitives
forge-connectors ───→ primitives
forge-sdk        ───→ primitives
forge-agent      ───→ primitives + sdk + gateway
forge-workspace  ───→ primitives + agent + sdk + dictate + gateway + connectors
forge-server     ───→ primitives + workspace
forge-web        ───→ primitives + server (parked: nothing depends on it yet)
forge-tui        ───→ primitives + workspace + server (no direct agent dep)
forge-test-harness ─→ primitives + sdk + workspace + server
```

- **`forge-primitives`** - every type that crosses a forge-* crate
  boundary. No logic, no I/O, no async.
- **`forge-dictate`** - the dictation primitive: audio in, text out.
  Owns its model files, speech recognition and normalization. Depends
  on no forge-* crate and knows nothing about a host, so it must not
  grow one; a doc comment mentioning a keypress, a composer or a
  session is a bug.
- **`forge-system-one`** - the System One decision-model client: the
  `[systemone]` config shape, the wire question/answer types with their
  validation, and the HTTP call with its timeout and error mapping. A
  leaf: depends on no forge-* crate, and its own types stay there even
  once another crate reads them.
- **`forge-gateway`** - the account pool: one backend per `forge.toml`
  provider token (credential resolution, the usage probe's HTTP +
  payload mapping, billing shape), plus account selection by declared
  models, account health, probe scheduling and backoff.
  Depends on forge-primitives only; the `claude --version` user agent
  and the TLS-trust client arrive through the `ProviderHost` port
  forge-agent implements, so the crate stays HTTP + mapping and never
  spawns the CLI.
- **`forge-connectors`** - one module per inbound connector: the
  stream client, REST lookups, subscription matching and subsystem
  pump for one external integration (Gotify and Slack today). Depends
  on forge-primitives only. Gotify's subscription set, app index and
  message dispatch into sessions arrive through the `GotifyHost` port
  forge-workspace implements; Slack's client is called by workspace
  code directly. Either way the crate stays stream + mapping and holds
  no workspace state. No generic connector trait: the variety is still
  two, and a trait waits until it is real.
- **`forge-sdk`** - owns the `claude` subprocess: stream-json codec,
  transport, control dispatch, in-process MCP host, Options.
- **`forge-agent`** - drives one SDK Client behind a channel-based
  `Agent`/`AgentHandle`. Owns userdata, cloud, env, translate, tooling.
- **`forge-workspace`** - multi-session orchestrator. Owns
  `DomainSession` + per-session `SessionTask` actors. Single TUI-facing
  facade.
- **`forge-server`** - the server: what a client reads of the core, and
  no view of its own. The read surface a client uses, the session records
  as a view sees them, the peer envelope parsing in both directions, the
  tool family table, forge's own slash commands, the policy that folds a
  run of blocks, the transcript
  fold that turns a conversation's messages into the units a view draws,
  the sub-agent instance list the session task's own fold keeps and
  pushes, the `ReadCallOutput` verb (with the workspace's tail reader
  re-exported for views), and the socket
  that carries all of it to whatever is drawing.
  Sits between `forge-workspace` and the clients, so a second client
  attaches beside the TUI rather than duplicating it. Nothing here may
  depend on a view. It reaches the workspace for the one thing a session
  record cannot answer alone - whether a tool's input parses into a
  lifecycle block - and does that through `forge-workspace` rather than
  `forge-agent`, so the agent layer stays behind the workspace facade the
  way it does for the TUI. The name is this crate; forge's in-process MCP
  server is unrelated and is named as the `forge` MCP server.
- **`forge-web`** - the web view, parked. It served a page per session
  from the process that owns the sessions; the socket took the port those
  pages were on, so nothing serves them until a client lands. It stays a
  workspace member and keeps compiling, and nothing depends on it: the
  client is built against the socket, and this crate is deleted then. Its
  pages were server-rendered markup over axum, and it never named
  `forge-workspace`: reads of the core and of a working tree both went
  through `forge-server`, which re-exports what a view needs.
- **`forge-tui`** - pure view layer. Per-session presentation on
  `UiSession`. No multi-session logic, no agent internals.
- **`forge-test-harness`** - wire-conformance harness (`sdk_wire`
  scope): replay-based offline tests + opt-in live capture. Dev tooling,
  not a published crate.

**Single-instance per config dir.** One `forge` owns a config dir
(`$CLAUDE_CONFIG_DIR`, else `~/.claude`) and manages many sessions in
it; a second is refused at boot with the holder's PID. The `flock` lives
on a machine-local lockfile under forge's app-support dir, not the
config dir, because `flock` binds the inode and the config dir's files
are rewritten by rename. A file-sync daemon applying an incoming change
the same way would swap the lock inode out from under a running forge
and let a second instance start. `single_instance.rs` has the full
reasoning.

**Config vs state.** `forge.toml` (under `<config_dir>/forge/`) is the
only file forge reads for config: read-only, hand-authored, safe to
sync. All runtime state (durable crons, Gotify subs, Slack subs and
sweep watermarks, dynamic workers, session ids, usage cache)
lives in one machine-local redb DB beside the lock. None of it belongs
in a synced config dir: the DB churns roughly once a minute, redb's
binary file cannot be merged, and the lock's inode must stay put.

## Crate placement guide (where does my new code go?)

Work top-down; first match wins.

1. **Audio, speech recognition, or turning either into text?** (capture,
   model fetch, transcription, transcript normalization)
   -> `forge-dictate`. A leaf: it may not depend on any forge-* crate,
   and its own types stay there even once another crate reads them.
   Wanting a forge-* dependency here is a design problem, not a
   dependency problem.
2. **System One decision-model I/O?** (the `[systemone]` config shape,
   the wire question and answer types with their validation, the HTTP
   call) -> `forge-system-one`. A leaf: it may not depend on any forge-*
   crate, and its own types stay there even once another crate reads
   them.
3. **A type that crosses a crate boundary?** (envelope, snapshot
   struct, hook payload, anything sent over a channel or touched by
   more than one crate) -> `forge-primitives`. Pure data shapes only.
4. **Provider credential, probe, usage mapping, billing or repair?**
   (how one `forge.toml` provider token authenticates, what endpoint
   its usage probe hits, how the payload maps to a snapshot, what a
   failure allows), or is it which account a session spawns under,
   whether an account is healthy, or when it is next probed? ->
   `forge-gateway`, one backend per token and the account pool.
5. **Inbound connector I/O for an external integration?** (its stream
   client, REST lookups, subscription matching, reconnecting
   subsystem pump) -> `forge-connectors`, one module per connector.
   The connector holds no workspace state: Gotify reaches the
   workspace through the `GotifyHost` port forge-workspace implements,
   and Slack holds only its Web API client.
6. **Speaks stream-json to the `claude` subprocess?** (decoder,
   control_request subtype, transport, MCP host, OptionsBuilder)
   -> `forge-sdk`. Pair with a wire-conformance scenario.
7. **Live state about the user's environment?** (git watcher, cwd
   resolution, env probes, OAuth, plugins, settings IO, plugin catalog
   scan)
   -> `forge-agent`: `env::*` for environment, `cloud::*` for Anthropic
   API / OAuth, `userdata::*` for `~/.claude*` files. Async, may shell
   out.
8. **Orchestration across projects, sessions, accounts, `forge.toml`,
   or the command bus?** -> `forge-workspace`. Adds `Workspace` methods,
   `Command` variants, `SessionUpdate` events. A read a VIEW needs is a
   verb on the view surface below, not a bare method.
9. **A session record as a view sees it, or a decision any view would
   make over one?** (the render-ready record, the reducer that derives
   it, the policy that decides how a run of blocks folds, the peer
   envelope parsing in both directions) -> `forge-server`. Sits
   between workspace and the views; the test is "does this render?" -
   if it does, it is the view's.
10. **A widget, screen, key binding, mouse handler, or per-session
    presentation state?** -> `forge-tui`. Render in `ui/`, dispatch +
    state in `app/`.
11. **A view that is not the TUI?** (its pages, its markup, its own
    per-view state) -> a crate of its own, built against the socket in
    `forge-server` rather than against the core. `forge-web` is the one
    that exists, parked until it is rebuilt that way. Either way it sits
    beside `forge-tui` on the same core, reads through the view surface
    in `forge-server` and never `forge-workspace`, and starts no
    subsystem of its own.
12. **A wire-conformance scenario?** -> `forge-test-harness`.

**The view surface is built, reads and writes.** A view reads the core
through named verbs by subject - `roster`, `session`, `agents`,
`accounts`, `plugins`, `reviews`, `workers`, `connectors`, `dictate`,
`dictate_models`, `cli_version`, `conversation`, `slash_commands`,
`forge_commands`, `subagents`, `has_dispatches`, `subagent_cards`,
`file_index`, `walk_file_index`, `respect_gitignore`, `header`, `mcp_servers`,
`processes`, `work`, `background_tasks`, `monitors`, `pending_asks`,
`fatal_error`, `service_status`
and `usage` - receives changes through
`subscribe()`, and acts through `dispatch()`, which is a verb rather than
an accessor so a view is handed the commands it needs and not the whole
core. All thirty exist in `forge-server`, and the TUI reads
its project roster, session scan cwd, worker registry, account pool,
plugin records, review threads,
connector subscriptions, dictation state and the session's process walk
through them; a second view reads its project roster and agent rows, the
account pool, the worker registry, connector subscriptions and dictation
state through those, the claude version through `cli_version`, its
composer's data through `slash_commands`, `forge_commands`, `subagents`,
`file_index` and `respect_gitignore`, and the conversation,
header, inspector and the asks it answers through
`conversation`, `header`, `mcp_servers`, `processes`, `monitors` and
`pending_asks`, and dispatches its composer's send, its prompt answers and
its take's controls. `walk_file_index` is the one walk left on the
surface: a page that holds no seat has no loop behind it. `subagents` is the
CLI's catalogue of the agent types that exist, not a record of the ones
that ran: an instance's cards and the calls under them are folded from
the conversation it appears in. What the migration has
not reached is the five refreshes that ask the core for a new snapshot:
they are still direct
`Workspace` calls, so `forge-tui` keeps its `forge-workspace` dependency
and the arrow above is not yet one-way. The surface carries two of them for
the socket - `refresh_context_usage`, asked on a seat's read, on a turn
finishing on a seat a page holds, and on a compaction settling, and
`refresh_mcp_snapshot`, asked on the seat's read when it reports no snapshot -
while the TUI's own call sites are the ones still direct. A read a second view
would want goes on that surface; a read only the TUI makes stays a plain
method.
Routing the remaining direct calls through the surface is its own piece
of work, not a prerequisite for adding to the crates.

Legitimate splits are common (a git-diff feature touches agent +
workspace + tui). Rule of thumb: logic/IO/subprocess -> agent;
cross-crate shape -> primitives; multi-session state -> workspace;
a session record as a view sees it -> forge-server; anything the user
sees -> TUI. The default failure mode here is "too much in
forge-tui", so bias toward the deeper crate when unsure.

### Anti-patterns (caught in review repeatedly)

- **Subprocess calls in `forge-tui`.** `tokio::process::Command` belongs
  in `forge-agent::env::*`. The TUI reaches it through workspace's
  re-export and awaits it off the render thread - see
  `app/git_diff.rs`, which calls `forge_workspace::env::git_diff::scan`
  from a spawned task and returns the result over its own channel.
- **A `SessionUpdate` variant for purely TUI-internal data.** If
  producer and consumer are both in forge-tui, use a separate mpsc
  channel (see `git_diff_event_tx/rx`).
- **Cross-crate type duplication.** Same-shaped `Foo` in two crates
  means one is wrong; lift to primitives or import the re-export. The
  exceptions are the leaf crates that may not depend on any forge-*
  crate, `forge-dictate` and `forge-system-one`: their types stay in
  them and consumers import them from there.
- **Provider dispatch outside `forge-gateway`.** A match on
  `Provider` in workspace or tui is the thing this crate exists to
  delete; route through `forge_gateway::backend(token)` instead.
- **Workspace methods bypassing the Command bus for user actions.**
  User-initiated actions go through `dispatch(Command)`; query-style
  refreshes are direct inherent methods. Don't conflate them.

## The server stack and the client stack

Two stacks, and one test decides which side a thing is on: **does
removing it change what the data IS, or only how it is DRAWN?** Only
drawn means it belongs to the client.

- **The server stack** is `forge-server` and everything under it: what a
  client reads of the core, the records as a view sees them, the folds,
  the working tree as state, and the socket that carries it. It is what a
  client cannot work out for itself, and it is the same for every client.
- **The client stack** is whatever draws: `forge-tui` today, whatever
  lands beside it after. Glyphs, colours, weights, spacing, an order
  chosen for display, the label a row is spelled with, the stripping of a
  command's escape sequences - any choice about appearance rather than a
  fact about the session.

Three things hold at the line:

- **The client relies on the server completely, and `forge.toml` stays
  the single source of truth.** So the server MAY hold a catalogue, a
  set, an index or a setting that a client reads - that is the server
  doing its job. What it may NOT hold is the decision about how any of it
  appears.
- **The server owns no git-level presentation.** It carries the working
  tree as state - the branch, what changed, and the changed files' raw
  hunks - and nothing that renders any of it: no colouring, no tree, no
  folding, no highlighted excerpt. A hunk crosses as lines with their
  kinds and line numbers, which is what changed; how any of it appears
  is the client's.
- **A thing the TUI needs moves into `forge-tui`; it is never dropped.**
  A thing both need keeps its home on the server with only the
  presentation half leaving, and the terminal works out of the box at
  every step - which is what makes the move safe to take in small pieces.

## Communication contract (MVVM)

The TUI to workspace contract is **one entry point in each direction**:

- **TUI -> workspace:** `Workspace::dispatch(Command)`. One enum, one
  entry point, every user-driven action.
- **workspace -> TUI:** `SessionUpdate` via `Workspace::subscribe()`,
  consumed by `App.workspace_rx`. Every caller gets a stream of its
  own, so a second view attaches beside the TUI; a stream carries what
  the workspace emits after that call, and the first caller to attach
  is handed what was emitted before it as well, so a notice raised
  during boot is not lost.

That is the whole contract: no callback hooks, no shared mutable state.
TUI holds no `Arc<AgentHandle>`; query-style
refreshes (`refresh_status_snapshot`, `refresh_context_usage`,
`refresh_mcp_snapshot`, and friends) are direct `Workspace` methods
rather than Command variants. `DomainSession` keeps only
workspace-internal routing metadata; all operational state the TUI
renders lives on `UiSession`.

**Both enums address a session by its slot.** A slot is the
`(org, project, label)` triple `forge-primitives`' `SessionSlot`
carries: it names the seat, and the claude session id names the
occupant. `/new` and `/resume` swap the occupant and leave the slot
alone, so every `Command` carries a slot and every `SessionUpdate`
routes on one. A session id survives only where the `claude` CLI or the
child's own address needs it: the `--session-id` / `--resume`
arguments, the gateway binding's URL segment, the stream-json
`session_id` field, and the transcript filenames (with the caches that
mirror them). `SessionUpdate::Connected` and `SessionReplaced` also
carry one, since they announce a new occupant: the id is their payload,
the slot is still their address.

**Two nuances that surprise people:**

- The TUI has an update channel of its own for its own async work.
  `App` mints an `update_tx` / `update_rx` pair, and four modules
  (`app/extensions.rs`, `app/slash/executors.rs`,
  `app/service_status_check.rs`, `app/input_submit.rs`) emit their
  presentation events through it instead of a Command round-trip. The
  event loop drains both feeds into the same reducer, and that pair is
  not part of what a non-TUI frontend reproduces: that is `dispatch()`
  and `subscribe()`.
- `forge-workspace` is a **thin facade, not strong isolation**. The
  boundary is enforced at the dependency graph (forge-tui has no
  forge-agent dep), not by visibility: workspace wildcard-re-exports
  forge-agent submodules, so the TUI sees agent's surface verbatim under
  a different name.

## Scope and threat model

forge is built for **one trusted user driving their own machine**, and
several design decisions assume it. A contributor should know what those
are, and should not quietly widen them:

- **State is unencrypted and machine-local.** The redb DB and the
  captured session JSONL hold conversation content in the clear, at
  filesystem permissions.
- **Credentials come from the user's own `~/.claude*` dirs** (env
  tokens in `forge.toml`). forge reads them; it does not manage or
  isolate them.
- **Sessions are not sandboxed from each other.** One config dir, one
  process, many sessions, shared state store.

None of that is a licence to discount a security finding. It bounds what
forge currently claims, and a change that breaks one of these
assumptions needs to say so plainly rather than arrive as a side effect.

## Vision: simple, efficient, capable - Rust-native

forge is **not** a feature-parity port of Python's
`claude-agent-sdk`. Both wrap the same `claude` CLI; they share a wire
contract with that binary, nothing more.

- **No public-API parity contract.** forge-sdk's shape is whatever
  serves forge-agent best. We don't carry Python's async-generator
  constraints, awkward method names, or types we don't need.
- **Lean into Rust.** Concurrent reads + writes + dispatch on one
  Client (actor pattern) is first-class. Channels-based APIs beat
  mutex-locked `&mut self` call sites; internal bridging is the
  library's job, not the caller's.
- **The `claude` CLI is still source of truth.** We spawn it and speak
  stream-json; we never re-implement the agentic loop or hit the
  Anthropic API directly.

The wire behaviour forge-sdk was built against is recorded in the
wire-conformance baselines under
`crates/forge-test-harness/baselines/sdk/`, which are live captures
rather than prose. Replay guarantees that every inbound line still
round-trips through the decoder without `DecodedLine::Unknown` or a
decode error. Apart from the `initialize` handshake's
`protocolVersion`, which is re-dispatched through today's code and
compared, it does not check recorded content against what forge would
send or receive now. A stale copy of forge's own MCP tool names
therefore survives in both directions: outbound because nothing
compares it, and inbound because the `init` frame carrying it is a
whitelisted generic system subtype whose `tools` array is never
inspected.

## Hard rules

1. **The `claude` binary is source of truth.** Spawn it, speak
   stream-json, never reach the Anthropic API directly.
2. **Stream-json wire-compatibility with `claude`.** Byte-identical to
   what `claude` expects on stdin and what we decode from its stdout.
   The wire-conformance harness is the enforcement mechanism.
3. **TDD discipline.** Failing test, run it, watch it fail, implement,
   run it, watch it pass, commit. Apply when the test shape is obvious;
   for exploratory refactors, integration tests are sufficient.
4. **One logical unit per commit.** Small and reversible.
5. **No `mod.rs`.** Module files sit next to their directory
   (`foo.rs` + `foo/`).
6. **Nightly Rust, pinned.** `rust-toolchain.toml` locks a specific
   nightly date. Bump deliberately.
7. **Clippy pedantic + deny `unwrap_used` / `expect_used` / `panic` /
   `exit` / `todo` / `unimplemented`** in non-test code. The workspace
   lint tables in `Cargo.toml` are the source of truth, including which
   pedantic lints are exempted and why.
8. **`cargo nextest run`, not `cargo test`.** `just check` runs fmt,
   the unicode-punctuation gate, clippy, nextest and docs in one shot,
   ending on a verdict line that names the first failing step; it must
   be green before a PR. Read that line, not a pipeline's exit status -
   `just check | tail` reports tail's.
9. **Wire-conformance harness is mandatory for new wire surface.** New
   control_request subtypes, message types, hook events and tool
   integrations ship with: (a) a live-capture scenario, (b) the captured
   baseline under
   `crates/forge-test-harness/baselines/sdk/<PINNED_CLI_VERSION>/`, and
   (c) clean replay, so every inbound line round-trips through the
   decoder without `DecodedLine::Unknown` or decode errors.

   A committed capture carries whatever the capture machine printed.
   Run a fresh one through `sdk_reredact_capture` before committing;
   `sdk_capture_hygiene` fails the build otherwise.
10. **Generated planning docs stay out of the repo.** Design notes an
    agent produced for one piece of work are not documentation; add the
    path to `.git/info/exclude` rather than committing it.
11. **`docs/book/src/ui/` is visual truth.** The book's UI surface
    pages (the "UI surfaces" part in `SUMMARY.md`) are the source of
    truth for every UI surface forge-tui can currently render. Scope is
    **current state only** - no future ideas, no aspirational sketches.
    Anything new arrives in the same PR that lands the code.

    **The workflow for a UI change**, and the recommended path for any
    session that starts with one:

    1. Read the relevant `docs/book/src/ui/` page first to confirm
       what is currently implemented and where it lives.
    2. Sketch the change in the page - update the relevant surface's
       mockup, prose, and any glyph or colour table entries.
    3. Apply the same change in the ratatui code.
    4. `mdbook serve docs/book` and check the page still matches the
       code.
    5. Push them together, code + page in one PR.

    The page-first step forces a clear visual target before code edits
    begin, and it keeps the doc honest. When in doubt about whether a
    page reflects reality, re-read the implementation and reconcile.
12. **Diagnostics are self-serve.** The artifacts are the perf log and
    tracing log under forge's app-support `logs/` dir, plus the JSONL
    session captures. Telemetry that needs a user to opt in is a forge
    bug: fix the always-on instrumentation instead of asking for a
    repro recipe.
13. **Deferred work goes to an issue, not an inline TODO.** A
    concrete-fix `TODO(<name>):` naming a specific one-to-three-line
    change is fine. "Consider adding X someday" is not: it rots,
    misleads, and has no triage view.
14. **`forge.toml` is the source of truth. Never read project state or
    any other behaviour-shaping value from the launch directory.**
    Project paths, names, accounts, auto_start pins, log paths, settings
    paths, trust keys and file-index roots must come from `forge.toml`,
    the active session's `cwd_raw`, or other fixed values.
    `std::env::current_dir()` must not influence anything.

    The binary test: **does forge behave identically launched from the
    repo and from `/tmp`?** Any observable difference - different
    project loaded, different settings file, different trust prompt,
    different log directory, different welcome-banner cwd - is a bug.
    Cosmetic differences count. When a needed user dir (state, cache,
    home) is unavailable, FAIL the operation rather than substituting a
    cwd-derived alternative.

    Read the session's own `cwd_raw`, or look the project up via
    `Workspace::list_projects()` / `find_project_view_by_name`. The only
    env reads still allowed are ones that do not vary with launch
    directory: `$CLAUDE_CONFIG_DIR`, `$HOME` / `dirs::home_dir()`,
    `$RUST_LOG` / `$NO_COLOR`, `env::vars_os()` for terminal-capability
    detection, and effort overrides like `$CLAUDE_CODE_EFFORT_LEVEL`.
    Spotting one of these while doing other work means fixing it on the
    spot and auditing for the same pattern; they cluster.
16. **A scoped change must not alter anything else observable.** A
    performance fix changes only speed. A UI change changes only that
    surface. If the scoped change *requires* touching behaviour
    elsewhere, that part is a separate PR presented on its own terms
    with the behaviour effect as the headline. **A second commit is not
    sufficient**: that reads as disclosure while still bundling the
    decision.

    State at plan time what the change may and may not alter. At review
    time, ask what a user would SEE that is different, and treat any
    non-empty answer on a perf or refactor change as a blocker until it
    is split out and decided on its own.
17. **Behaviour forge depends on belongs in the text forge ships, and
    gets there through an issue, never a direct edit.** A rule living
    only in a contributor's own `CLAUDE.md` is not shipped: every other
    install runs without it, and the text reaches every one of them.

    The test has two halves and both must hold: **does forge machinery
    depend on it, AND is it unknowable from outside forge - not merely
    good advice a competent person reaches unaided?** Judge that at the
    grain of the sentence that would ship, not the principle it sits
    under, and never against text already shipped in a misleading form,
    which qualifies regardless of whether the right behaviour was
    independently reachable.

    **A third question decides what a fix can look like: who authored
    the depended-on text?** Text forge owns can be edited in place.
    Text forge inherits from the `claude` binary arrives first in the
    prompt and can be appended to but never removed, so it can only be
    addressed by forge's own text stating which instruction governs;
    quoting the inherited line verbatim creates a wording dependency
    forge does not control.

    **Establish every claim about shipped text by grep, when you write
    it.** The same instruction recurs across surfaces at different
    strengths, so read every hit, not the first; these constants wrap
    mid-sentence, so quote only what sits on one source line; and check
    each surface's own history before calling the shipped side wrong.

    **Divergence counts, not just absence - and a divergence can be the
    correct state.** The despawn trigger is the worked example. The
    two-case structure now lives in `agents__spawn`, the
    `agents__despawn` description and the charter's despawn step, while
    `LEAD_DELEGATION_PREAMBLE` stops at "truly done"; the charter
    separately says "once they have delivered". The preamble's softness
    is #717's: it removed "once its work is merged" because that
    exempted every worker whose output was not a PR, and cited
    `LEAD_DELEGATION_PREAMBLE`'s softer wording without adopting it,
    leaving the preamble untouched, while `agents__spawn`'s old clause
    was text #717 never decided - #750 settled it, and #820 carried the
    same structure into `agents__despawn`. An odd phrasing is not
    evidence it was overlooked; check each site's history rather than
    its wording.

    **Placement decides whether the text fires at all**, and the
    audience is a forge SESSION at runtime. The shipped surfaces are a
    SET; read all of them first, or the grep that finds nothing files a
    duplicate. Always-on
    blocks (`crates/forge-agent/src/forge_sdk_worker.rs`) carry what
    every session needs; the charter
    (`crates/forge-workspace/src/spawn/lead_charter.md`) and
    `LEAD_DELEGATION_PREAMBLE` in
    `crates/forge-workspace/src/workspace.rs` both append to a lead's
    prompt; a tool description carries what a caller needs at the
    moment it reaches for that tool, while its result and error prose
    is read after it has already acted, which is where correction
    belongs; and forge delivers some text as a turn, like
    `DYNAMIC_WORKER_RESTART_NOTE` or forge-tui's `continuation_prompt`,
    so the search is not confined to the crates named here. The
    cross-project rule shows the cost: it lived only in peer tool
    descriptions, read once a peer tool was already in hand, until #733
    added an always-on copy stating it applies when you decide. The
    peer copies stayed.

    **Passing the test is not sufficient: contributor-facing text stays
    in this file.** `perf.rs` runs enabled on a hot path, which
    `scripts/install.sh` turns on rather than Cargo, and the unicode
    gate passes both halves but only ever runs in this repo. Out for
    failing the test itself: approval-scope tables, timezone
    presentation, prose punctuation preferences, PR-body voice, commit
    conventions, release workflow. Shipped text also never names a
    user-scope skill, command or plugin, since a fresh install has none;
    the charter is guarded token by token by
    `bundled_lead_charter_assumes_no_local_environment`
    (`crates/forge-workspace/src/spawn.rs`) and other surfaces are
    guarded unevenly, so grep for an assertion rather than assuming
    either way.
18. **The published documentation is a separate obligation from the
    UI surface pages, with a different audience.** Rule 11 owns the
    book's `ui/` pages, the maintainer's visual record. This rule
    covers the rest of `docs/book/` and the rustdoc published beside
    it, the site users read at
    https://busytools.github.io/forge/.

    **The publish is automatic and the content is not.**
    `.github/workflows/docs.yml` builds `docs/book` and the
    workspace's rustdoc - the book at the site root, rustdoc under
    `/rustdoc/` - and deploys both to Pages on every push to `main`,
    while a pull request skips the deploy. The book pages are
    hand-written markdown and none of them derives from the code, so
    a change that edits no page republishes the old description
    within minutes of the merge, with a green build and a green
    deploy beside it. Those pages are never out of date with the
    repo, and they can be confidently wrong about the code. The
    rustdoc is generated from the tree, so it tracks the code rather
    than lagging it; the obligation it carries is that the docs build
    publishes every crate's rustdoc, private items included, and the
    raw source of every file it documents, so a doc comment in a
    documented file reaches a public URL whether or not rustdoc
    renders an item for it.

    The pages, and the usual way each goes false:

    - `configuration.md` - every `forge.toml` key, its default and its
      error strings. The most exposed page in the book: a new key, a
      changed default or a reworded load failure falsifies it.
    - `install.md` - prerequisites, the CLI flag table, and the fullest
      copy of `just check`'s composition. A changed flag, prerequisite
      or step falsifies it; a recipe missing from its avowedly partial
      `just` list does not.
    - `index.md` - what forge is, the surfaces it renders, the scope
      caveats, and the pointer to the rustdoc.
    - `architecture.md` - crate count, layering diagram, crate table,
      placement guide, the TUI-to-workspace contract, the MCP tool
      groups, the single-instance guard, the pointer to the UI
      surface pages. Its content is mirrored in `README.md` and this
      file.
    - `wire-contract.md` - capture and replay modes, baseline layout.
    - `contributing.md` - the short-version house rules: `just check`'s
      composition, the denied lints, the gates. A changed recipe, lint
      or hard rule falsifies it, and it is the page nobody remembers.
    - `SUMMARY.md` - when a page is added, removed or renamed. A
      deleted page is the one case CI catches, since `book.toml` sets
      `create-missing = false`. Nothing catches the deep links from
      `CONTRIBUTING.md` and `README.md`, which a rename 404s.

    `README.md` carries the crate table and layering diagram;
    `CONTRIBUTING.md` the house rules and a prose placement summary
    that links out; this file the diagram, the placement guide and the
    hard rules. Not published, same test, same PR.

    Rule 11's test, sharpened: **does the document now say something
    false about main?** Not "could it be improved". Prose that is
    merely old-fashioned is owed nothing.

    **Same PR, never a follow-up.**

    **Most changes owe the book nothing, and a rule read as owing
    something every time produces noise forever.** #744 is the clean
    example: a user-visible change to whether a question answers on the
    first Enter, which owed the visual-truth surface - then the
    separate `docs/forge-map.html`; today the unified-prompt section of
    `ui/input.md` - and touched no other book page. User-visible is not
    the test; a page reading false is.

    #751 is the other shape. Adding the seventh crate falsified
    `architecture.md`'s crate count, layering diagram, crate table and
    placement guide: four edits on one page, where stopping at the
    table leaves the other three reading false.

    **The layering diagrams differ in grain deliberately. Do not
    reconcile them.** This file draws `forge-test-harness` on
    primitives + sdk + workspace + server; `README.md` and
    `architecture.md` draw it on primitives + sdk. `forge-workspace`
    and `forge-server` are dev-dependencies of the harness, so both are
    true and neither is stale. Naming
    these documents as a set is what invites someone to make them
    agree, which would quietly change what two of them mean.
19. **A terminal multiplexer is the common case, not the edge case.**
    forge is expected to run behind one, so a capability forge detects
    is not a capability forge has. Recommending a particular
    multiplexer is not the same as depending on one.

    **Build the multiplexer-independent path first.** Where one
    exists it is the primary and an escape sequence is the fallback,
    never the preference. An escape sequence is a request to whatever
    sits between forge and the terminal. Some of that middle
    announces itself and some of it does not: zellij sets `ZELLIJ`,
    screen sets `STY` and shpool sets `SHPOOL_SESSION_NAME`, while
    dtach's whole source contains no `setenv` at all, which is why
    #767 names it beside shpool. forge reads none of them today, and
    reading one would not settle the question anyway - identifying
    the manager does not say what it forwards.

    #778 is the worked example, and it is a good one because forge
    already had the right path and skipped it: believing the terminal
    spoke OSC 9 used to suppress the `notify-rust` desktop
    notification, which reached the OS without crossing the terminal
    at all. That path is gone (2026-09-13) - it was suppressed in
    every setup the maintainer uses, so nothing ever crossed without
    the terminal and the escape is now the only channel. The
    capability detection that gated the escape is gone too (deleted
    2026-09-13, with `notifications_osc9`): no terminal's
    self-description decides whether a notification is emitted, so the
    escape is written whether or not anything is known to render it.
    Measured 2026-08-29: `TERM_PROGRAM` and `ITERM_SESSION_ID` both
    reach a pane under zellij 0.44.3 and under GNU screen 4.00.03
    unchanged, while an OSC 9 emitted inside either does not reach the
    outer pty, with plain text written on both sides of it arriving
    normally.

    Corrected 2026-09-07 by a live three-probe matrix on the user's
    mac-studio (Ghostty + ws/shpool 0.9.8, one probe at a time, the
    user as the only observer): shpool 0.9.8 FORWARDS OSC 9 - the
    vendored-vterm literals were a source reading, not a behaviour;
    Ghostty renders the escape into a system banner. The earlier
    shpool-strip claims in this rule were wrong. The real limit is
    Ghostty's own frontmost suppression: with Ghostty focused,
    `Ghostty.App.swift`'s `shouldPresentNotification` returns false,
    so the notification lands silently in Notification Centre and the
    user sees only a dock bounce; defocused, the banner presents.
    That suppression is hardcoded in Ghostty 1.3.1 with no config
    (upstream discussion #10691 proposes options; none shipped). The
    user-facing "notifications completely gone" report was this
    suppression plus forge's text carrying no context - not a dead
    delivery path. There is no seam left to retreat to: with the
    desktop path gone and `notifications_osc9` removed, forge writes
    the escape unconditionally and whatever sits in the middle decides
    whether it crosses.

    What crosses is decided per sequence by the thing in the middle,
    and no one capability answers it for every sequence. tmux
    re-emits some pane-originated OSC from its own terminfo: OSC 8
    when the outer terminal carries `Hls`, and since 3.7 the OSC 9;4
    progress bar via `Spb`. The OSC 9 NOTIFICATION form is not among
    them - `input_osc_9` returns on any payload not starting `4` -
    and `9;4` is the one shape that would arrive as something else,
    which is the collision a notification escape used to be exposed
    to. forge emits `777;notify`: `777` is not a number tmux
    re-emits from terminfo, so the escape has no form to be mistaken
    for. Carrying an arbitrary sequence out takes a DCS envelope, and
    the price differs per multiplexer: tmux wants a `tmux;` prefix,
    `allow-passthrough` at `on` for a visible pane or `all` for any
    (tri-state since 3.4, default `off`), and every ESC in the
    payload doubled, the doubling being the one requirement `tmux.1`
    never states. screen forwards a bare DCS-wrapped OSC 9 with no
    opt-in and no doubling; that was measured with OSC 9, and the
    777 form is unmeasured there. shpool forwards the 777 form,
    measured 2026-09-13 with both terminators. The screen half is
    measured; the tmux half is read from source, at 3.7c except where
    an earlier tag is named.

    **Where no multiplexer-independent path exists, state the
    requirement and detect its absence.** Depending on a sequence is
    allowed. Depending on it silently is not, because a feature that
    quietly does nothing reads as forge being broken rather than as
    the multiplexer eating it.

    **The notification escape is a deliberate exception to that, and
    the reason is the sentence above it.** forge writes OSC 777
    unconditionally (2026-09-13) and detects nothing about whether it
    will cross. Nothing in the middle reports what it forwarded, so
    there is no absence to detect and no true requirement to state:
    reading `ZELLIJ`, `STY` or `SHPOOL_SESSION_NAME` names the
    manager and still does not say whether that manager passes the
    escape, and the same reads guessed wrong in both directions when
    detection existed. A terminal that ignores the sequence is
    harmless, not degraded, so the failure this rule exists to
    prevent - a feature that quietly does nothing - is not the one the
    user meets. This is a case the binary test below resolves to
    silence on purpose: the explanation lives in `configuration.md`
    rather than in a runtime message, since nothing the middle reports
    could inform one.

    **The keyboard-enhancement negotiation is the example to copy.**
    `resume_terminal` (`crates/forge-tui/src/app.rs`) pushes the
    kitty enhancement flags because `SUPER` arrives no other way, and
    then asks whether they took: `report_keyboard_enhancement_support`
    calls `supports_keyboard_enhancement` (crossterm public API),
    warns when the answer is no or the query fails, and records the
    verdict in `KEYBOARD_ENHANCEMENT_SUPPORTED`. The TUI reads it back
    through `keyboard_enhancement_supported()` and surfaces it on the
    dictate preflight row, so a terminal that ate the flags says so
    where the user is looking instead of leaving a dead key. A
    reattach under a byte-transparent session manager arrives as a
    resize, which rewrites the flags. `is_cmd_shortcut` in
    `app/keys.rs`, which accepts `CONTROL` where `SUPER` cannot
    arrive, is worth reading, but accepting a substitute is a
    fallback and not a detection, and treating one as the other is
    how this rule gets satisfied on paper.

    **Where the implementation must be multiplexer-specific, it owes
    three things**: why the generic path was not possible, which
    multiplexers it works under and which it does not, and a seam - a
    `forge.toml` key plus enough structure that a second multiplexer
    is a config entry and an implementation rather than a rewrite.
    Hardcoding one multiplexer's behaviour with no way to add another
    is the thing this forbids. The first two belong in the pull
    request and beside the `forge.toml` key, never as a support
    matrix in a comment, which rots exactly as hard rule 13
    describes.

    The binary test: when a terminal multiplexer intercepts it, does
    the user get a degraded experience or an explained one? Silence
    is the failure.

20. **`WARN` and `ERROR` are for forge's own problems.** A session's own
    work is information about that session, not a warning about forge:
    its tool call failing, its command exiting non-zero, its history
    replaying on resume. Those are `debug`, and so is any condition a
    reader cannot act on, however real it is. A git probe missing
    `origin/HEAD` on a repo whose remote was added rather than cloned is
    real, and the level still says it is not forge's health.

    **A line that does claim a problem carries what acting on it
    needs**: the session, the org, the model, the account, the path,
    whichever apply. A warning naming none of them costs a reader as
    much as no warning at all.

    **A new log site names an `event_name` and says which of the two it
    is.** A demoted level is not a silenced one: the default filter
    directives in `crates/forge-tui/src/logging.rs` are where a record
    that no longer claims a problem stays readable.

21. **The web view is ONE theme built from reusable components.** Ved,
    2026-09-26: *"Everything reusability, single theme. It's a hard
    project scope rule."* Not a preference, and it applies to every page
    from the home onward.

    - **One theme.** Every page reads the same token set, and that set is
      `[client] theme` in `forge.toml` - the key already exists and is what
      a second palette will hang off. A page carrying its own palette,
      its own spacing scale or its own copy of a mark is the defect this
      rule names.
    - **Reusable components.** The pages that exist are the reference for
      the page that does not. The state marks, the row, the chip, the
      band card, the hairline grouping and the live stream wiring are
      shared pieces, not one-offs. Before adding a page, name what it
      takes from the existing ones rather than re-deriving it; when it
      needs something new, build it so the others could use it too and
      put it where they can reach it.
    - **The review test:** could the NEXT page be written by reusing this
      one's pieces, or would it copy them? A second copy of a row or a
      mark is a finding, not a style choice.
    - **Prefer upstream to building, and verify what is pulled in.** A
      maintained script vendored into `forge-web/assets` or a maintained
      crate beats writing either, because the maintenance we avoid is the
      point - the size of the web ecosystem is the reason this is a web
      view at all. `pulldown-cmark` for markdown and `syntect` for
      highlighting are the two already chosen for the session page.
      **But upstream does not mean anything on npm:** a dependency nobody
      maintains is worse than the fifty lines it replaced, because it is
      fifty lines that cannot be fixed here. Before adding one, check it
      is genuinely maintained - a real release history rather than one
      commit, an issue tracker that gets answered, a version that is not
      years behind - and say in the pull request what was checked. A
      single maintainer is fine and often right; dormant is not. Ved's
      words: *"we don't want to rely on random things. We want to verify,
      see how well it is maintained, and all that stuff... it needs to be
      genuinely good."* Reach for the framework before the helper, and
      hand-roll only what nothing maintains.
    - **Keeping all the information is not in tension with this.** A
      session page shows everything a session has - context, git and PR,
      tasks, MCP servers, processes, monitors, subagents, gotify, slack,
      schedules - because nothing here is dropped to look modern. What
      changes is the presentation, not the content.

22. **The client is gated like the Rust side, and it is a shell.** `just
    check` runs the client's steps too - Prettier, ESLint on
    typescript-eslint's type-checked configs, `svelte-check`, `tsc
    --noEmit`, then vitest - so one command decides both stacks and its
    verdict line names the first failing step. **The shell under
    `client/src-tauri/` is its own workspace root**, so `just check`'s
    Rust steps and CI's cargo jobs do not reach it; the Unicode
    punctuation gate, which CI runs too, and the client's Prettier step
    do. `just client-tauri-check` builds it in the shipping configuration
    and `just client-tauri-bundle` adds the bundles. **Denied as errors**, the
    analogue of the denied Rust lints: `any`, non-null assertion,
    `@ts-ignore`, `innerHTML`, `eval` and floating promises. **A waiver
    carries its reason, and which form it takes is not free:** an inline
    suppression comment in TypeScript is blocked by a hook in this repo,
    so a waiver there is a scoped entry in `client/eslint.config.js`
    saying why - and fixing the code so it needs none comes first. The
    `{@html}` pair in `Sprite.svelte` and `Brand.svelte` is the worked
    example of the inline form, which templates do allow.
    `client/tsconfig.json` keeps
    `noUncheckedIndexedAccess`, `noImplicitReturns`, `noUnusedLocals`,
    `noUnusedParameters` and `exactOptionalPropertyTypes`. **An unknown
    value from the wire is narrowed once, where it enters**
    (`client/src/wire/`), so every union downstream stays exhaustive and
    a variant the server adds is a compile error rather than a render
    crash; a cast at that boundary carries a line saying why. **Every
    state a page can be in - loading, empty, failed, unknown - has a
    rendering**; a page that draws a healthy state for an unknown one is
    a defect, not a gap. Accessibility is a rule rather than a review:
    semantic markup, every interactive element reachable by keyboard,
    focus moved and returned deliberately, no colour as the only carrier
    of a state, and axe over the rendered markup as a page test
    (`client/src/a11y.test.ts`) - contrast is not covered there, because
    jsdom performs no layout, so it stays a rule checked where the token
    set is. **Touch is a hard requirement, not a fallback.** The client
    runs on Android as well as the desktop. Every control a finger
    reaches takes a 44px target under `@media (pointer: coarse)` - keyed
    on the POINTER, not the width, because an Android tablet is wide and
    still finger-driven. No affordance may exist only on hover. Anything
    opened by a keyboard gesture needs a visible door for touch, and
    every overlay is pushed onto history so the hardware Back closes
    what it opened. **The design skills are picked up when the work is
    something
    a person will look at** - a page, a component, a layout, a theme, a
    mark, a drawing, a chart - **and before the code is written, never at
    review time.** `frontend-design` originates a look that has no
    drawing behind it; `ui-ux-pro-max` and its family carry the quality
    checklist, the tokens, the component specs and the identity work, and
    its UX guidelines are ordered accessibility first. **The order of
    authority is the mockup, then this standard, then the skills' generic
    guidance** - a salvaged or approved look is not re-litigated by a
    database of styles - and **that order holds whether or not they are
    installed**, because they are user-level plugins rather than
    something this repo carries: a contributor without them applies the
    checks above from this rule alone. The page reviewer's brief and the
    fan-out charters carry the order, so a worker meets it before it is
    writing. The structural comparison against the mock and measured
    geometry at 1600 and 430 go in the PR body, and a page's final word
    is Ved looking at it. **And the shipped app carries no fixture, no
    mock data and no dev-only default**: its only input is the server
    URL, it opens on the last address that answered and shows the connect
    screen when none does, and nothing bundled ever stands in for a
    server. The fixtures live under `client/src/dev/` and
    `client/src/dev/fixture.test.ts` builds the app and fails if one
    reaches the bundle.

23. **Upstream before building, and a minimal core on both sides.** Ved,
    2026-09-29: *"keep server minimal, keep the client minimal, depend on
    upstream as much as we can, and provide as much flexibility."* This is
    project-level rather than rule 21's web-view framing: prefer a
    maintained dependency to hand-rolled code on every surface, held to
    rule 21's own bar - which lives there, so edit it there rather than
    restating it here.

    **Minimal means a small surface, not a small client.** The server
    carries what a client cannot work out for itself and nothing that
    decides how any of it appears; the catalogue carve-out belongs to the
    two-stacks section above, not to this rule, so a label the server
    spells is not a violation of it. The client carries the appearance and
    reaches upstream rather than re-deriving it. Work that widens the
    server so a client does not have to is the trade to look at hardest,
    because every view reads the server and a change there is one the
    clients have to be told about.
24. **The terminal is the reference implementation until it is deleted.**
    Ved, 2026-09-30: *"for any of this, take a look at how CLI is done.
    TUI has done a very tight integration. We can either pull things out
    in the shared crate or we can port it... have it as a project scope
    rule to check for any of these issues to see how TUI has solved it.
    And if I was not aware, maybe suggest some changes if they are
    required. That's a goal until we move the TUI completely."*

    **The step is a read, not an intention.** Before fixing a client
    issue, read how the terminal meets the same problem, and say in the
    pull request what it does and whether this change matches it. The
    terminal shares a process with the whole core and has been the only
    view for forge's life, so it has usually met the problem already -
    and its answer is the vocabulary the client is being brought to
    parity with, which is why a difference is a decision to state rather
    than an accident to discover.

    **Two moves, and the choice between them is the interesting part.**
    **Lift** the shared piece into a crate both views can read, which is
    the better answer when the piece is a decision about the data - a
    fold, a mapping, a turn boundary. **Port** it when the piece is
    presentation and the two views legitimately differ, which is the
    answer for anything a character grid decided. Rule 22's split is the
    test, and the two-stacks section above is where it lives.

    **A divergence that survives is worth naming in the pull request,
    not hiding.** The client is meant to differ where the grid forced
    the terminal's hand - a glyph, an arrow, a width - and those are
    decisions. What the rule forbids is the third case: a client
    answering a question the terminal already answered, differently,
    because nobody looked.

    **The rule expires with the crate.** It is a parity obligation
    rather than a permanent architecture, so the TUI's deletion removes
    it rather than converting it.
25. **No message the CLI sends may be dropped, on the server or on the
    client.** Ved, 2026-09-30: *"all the updates that we are getting,
    not only for the server and the client, for both, is that we do not
    want to drop any message updates coming from CLI. Every CLI, every
    message... No message should be dropped on the server side and also
    on the client side as well when it is rendering. If we are dropping
    it, rather, show them. When I notice that, I will ask you what it
    is. We can look back in the transcript and we can find a better way
    to represent that."*

    **The default for a frame with no vocabulary is to draw it as
    itself, not to drop it.** A frame that arrives and draws nothing is
    indistinguishable from a frame that never arrived, and a reader
    will assume the second. A frame drawn plainly is odd on screen and
    can be improved; a frame dropped is a silence nobody can act on.

    **This is a rendering obligation, not a promise to render well.**
    The rule exists so the gap is visible, and finding the right shape
    for it is the next question rather than this one. So an unknown
    frame gets a row, and the row is allowed to be ugly.

    **A filtered frame is a drop.** `filter_map` returning `None`, a
    `retain`, a placeholder string standing in for content, an arm with
    no matching case: each of those is this rule's subject whether or
    not the code calls itself a filter.

    **Rows are never skipped either.** Ved, 2026-10-08: "rows must
    never be skipped, either on the client or on the server. Everything
    should be shown to me, so that is how I know if a row looks off -
    that means it is new, and I need to capture the style for it; if
    not, the existing one, then it is already stylized." A new kind of
    row drawing plainly is the mechanism working, not a gap: seeing it
    is how its style gets designed, and a skipped row is a style nobody
    knows is missing.

    **The read path and the live path are one obligation.** The same
    message arrives once as a frame and once replayed from the
    transcript, so a transform that rewrites content on one side and
    not the other is a defect of this rule as much as a missing arm is,
    and it is the harder one to see because both sides look complete
    from where you are standing.

    Audience: everything that draws. The terminal is not exempt in
    principle - it is exempt only because it is being deleted.

## Claude Code worktree interop

Non-guessable external conventions, recorded so forge does not reinvent
them:

- `--worktree [name]` defaults to in-repo
  `<repo>/.claude/worktrees/<name>/`, not a sibling dir. forge matches.
- `EnterWorktree` / `ExitWorktree` are built-in tools the model can
  call; forge decides per session whether to block them in favour of
  `mcp__forge__*`.
- `AgentInput.isolation: "worktree"` is Claude's native auto-worktree
  for Task subagents, separate from forge's MCP worktree path.
- The wire envelope carries `worktree: {name, path, branch,
  original_cwd, original_branch}`; hook events `WorktreeCreate` /
  `WorktreeRemove` are first-class.
- `.worktreeinclude` (repo-root, gitignore syntax) is the convention for
  copying gitignored files into a new worktree.
- `worktree.baseRef = "fresh" | "head"` picks origin/HEAD vs local HEAD.

## Style + Rust idiom

- **Attributes are rare; default to none.** `#[non_exhaustive]`,
  `#[must_use]`, `#[allow(...)]`, `#[deprecated]` and `#[doc(hidden)]`
  need a specific documentable reason. Nothing here is a published API,
  so a compile break on enum or struct evolution is the point rather
  than something `#[non_exhaustive]` should paper over. An `#[allow]` on
  production code is dead-code or stale-lint debt: fix the cause and
  drop the marker. Where one is genuinely warranted, it carries a
  one-line reason.
- **Channels-based APIs over `&mut self`.** For types used across tasks,
  expose channels or `&self` methods rather than forcing a Mutex or
  actor wrapper on the caller.
- **Errors:** `thiserror` for library error enums, `anyhow` at binary
  and orchestration boundaries. No `unwrap` / `expect` outside tests.
- **Subprocess:** `tokio::process::Command` for streaming I/O,
  `cmd_lib` for fire-and-forget shell.
- **Tracing only.** Never `println!` / `eprintln!` in library code;
  binaries may use `eprintln!` only when tracing itself failed. Which
  level a site takes is hard rule 20.
- **Comments earn their place.** What the code does, never. Why, only
  when a reader would otherwise ask and cannot infer it from names or
  surrounding code. Non-obvious gotchas, external constraints and API
  quirks are exactly what comments are for.
- **No unicode punctuation.** Em-dashes, en-dashes, horizontal bars and
  curly quotes are rejected by `just check`. Where a codepoint is
  functionally required, use the escape form (`"\u{2014}"`). The
  ellipsis U+2026 is allowed - it is a real truncation glyph in the TUI.

## Releases

`just release <version>` is the whole release, and running it is the
maintainer's act: it bumps the workspace version and the client's own
manifest to the same number, commits and tags them together, installs the
server binary first, through `install` (the same `scripts/install.sh`
`just install` runs), installs the client over `/Applications/forge.app`,
stages the Android release APK and the web archive, then pushes `main` with
the tag and publishes the GitHub release with every asset the update path
reads. One number and one tag name every half.

The server install goes first, so the client's refusal - the one that
names `just client-release` as its recovery - cannot leave the binary
behind. It is unconditional and fails rather than skipping: an install
that cannot complete aborts the recipe with the tag cut and no OK line,
and each half re-runs alone with the tree still at the tag (`just
install`, `just client-release <version>`,
`just client-android-release <version>`, `just client-web-release
<version>`); a release that failed after its tag was cut finishes with
`git push --follow-tags origin main` and `just publish <version>`, while a
fresh `just release` refuses on the existing tag. The push and the publish
come last, because they are the only irreversible steps.

The client bundle is the app alone
(`--bundles app`), so no disk image is mounted and no Finder window
opens, and a client running from the installed bundle is a refusal rather
than a replace: replacing a live bundle underneath itself is the one way
this fails quietly. The bundle also carries the update tarball the desktop
updater installs from, signed with the minisign keypair under `~/.tauri/`
that the client README documents (nothing in the repo) - losing that
private key ends updates for every installed desktop, which would then
need a manual reinstall onto a new key. The signed pair is read back
against the pinned pubkey before anything is swapped in, the way the
Android half reads its APK's signer back. `client-android-release` then
stages the release APK: arm64, release-signed from the local keystore the
client README documents (nothing in the repo), at
`client/src-tauri/target/release/bundle/android/forge-<version>-arm64.apk`.
It fails rather than skipping when its toolchain or keystore is missing,
and reads the version, the ABI and the signer back off the built APK, so
one that never built, or was signed by anything else, cannot read as a
released half. `client-web-release` stages the web archive at the bundle
root, and then `publish` writes `latest.json` - the manifest the desktop,
the phone and the web half each read - and creates the release with the
five assets. The tag push also triggers the image workflow, which builds
the web client's image from the same tree and publishes it to ghcr under
the release version and `latest`. `just check-release`
and `just check-feature-configs` gate the recipe because `cargo install`
builds in release mode and would otherwise find the error after the tag
exists. The second is the one that compiles the configuration `install.sh`
builds; `check-release`'s `--all-features` enables the test-only features
that build leaves off.

## Workflows in skills

- `.claude/skills/claude-cli-upgrade/` - CLI version bumps, baseline
  regeneration, and the wire-conformance cheatsheet.
- `.claude/skills/dev-stack/` - the per-feature scratch stack: a second
  `forge` under `/tmp` with its own config, ports and store, plus a dev
  client pointed at it, for feature-scale work the maintainer verifies
  before merge.
