# forge - project guide

A Rust workspace around Anthropic's `claude` CLI: a server that runs
multi-session orchestration, and the clients that draw it. Eleven crates,
layered acyclically:

```
forge-primitives ───── leaf (pure data, no logic)
forge-capture    ───── leaf (microphone capture; depends on no forge-* crate)
forge-dictate    ───→ capture (dictation)
forge-system-one ───── leaf (decision-model client; depends on no forge-* crate)
forge-gateway    ───→ primitives
forge-connectors ───→ primitives
forge-sdk        ───→ primitives
forge-agent      ───→ primitives + sdk + gateway
forge-workspace  ───→ primitives + agent + sdk + dictate + gateway + connectors
forge-server     ───→ primitives + workspace + sdk
forge-test-harness ─→ primitives + sdk + workspace + server
```

- **`forge-primitives`** - every type that crosses a forge-* crate
  boundary. No logic, no I/O, no async.
- **`forge-capture`** - microphone capture: the device, the input plan,
  the downmix and resampler, and the mono frames a host reads from it.
  A leaf: no forge-* dependency, and no knowledge of a host, a take or
  a wire - so a client's shell captures through it and `forge-dictate`
  takes its sample rate.
- **`forge-dictate`** - the dictation primitive: audio in, text out.
  Owns its model files, speech recognition and normalization, and takes
  audio as mono frames a host pushes in. Its one forge-* dependency is
  the capture leaf, for the sample rate; it knows nothing about a host,
  so it must not grow one, and a doc comment mentioning a keypress, a
  composer or a session is a bug.
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
  `DomainSession` + per-session `SessionTask` actors. Single core-facing
  facade.
- **`forge-server`** - the server: what a client reads of the core, and
  no view of its own. The read surface a client uses, the session records
  as a view sees them, the peer envelope parsing, forge's own slash
  commands, the transcript
  fold that says where a conversation's turns sit and the task endings it
  carries, the sub-agent instance list the session task's own fold keeps and
  pushes, the `ReadCallOutput` verb (with the workspace's tail reader
  re-exported for views), the boot path the `forge` binary runs, and the
  socket that carries all of it to whatever is drawing.
  Sits between `forge-workspace` and the clients, so another client
  attaches beside the one that exists rather than duplicating it.
  Nothing here may depend on a view. It reaches the workspace for the one
  thing a session record cannot answer alone - whether a tool's input
  parses into a lifecycle block - and does that through `forge-workspace`
  rather than `forge-agent`, so the agent layer stays behind the
  workspace facade. This crate ships the `forge` binary; forge's
  in-process MCP server is unrelated and is named as the `forge` MCP
  server.
- **`forge-test-harness`** - wire-conformance harness (`sdk_wire`
  scope): replay-based offline tests + opt-in live capture. Dev tooling,
  not a published crate.

**The client** lives in `client/` - a Svelte page (`client/src/`) inside
a Rust shell (`client/src-tauri/`) - and is its own workspace root, drawn
entirely off the socket.

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

1. **Audio, speech recognition, or turning either into text?** (model
   fetch, transcription, transcript normalization)
   -> `forge-dictate`. It leans on the capture leaf below and on nothing
   else of the workspace, and its own types stay there even once another
   crate reads them. Wanting a forge-* dependency here beyond the capture
   leaf is a design problem, not a dependency problem.
2. **Opening a microphone?** (the device, its config negotiation, the
   downmix and resampler, the frames a host reads)
   -> `forge-capture`. A leaf: it may not depend on any forge-* crate and
   knows nothing about a host, so a host that captures - a client's own
   shell - builds on it.
3. **System One decision-model I/O?** (the `[systemone]` config shape,
   the wire question and answer types with their validation, the HTTP
   call) -> `forge-system-one`. A leaf: it may not depend on any forge-*
   crate, and its own types stay there even once another crate reads
   them.
4. **A type that crosses a crate boundary?** (envelope, snapshot
   struct, hook payload, anything sent over a channel or touched by
   more than one crate) -> `forge-primitives`. Pure data shapes only.
5. **Provider credential, probe, usage mapping, billing or repair?**
   (how one `forge.toml` provider token authenticates, what endpoint
   its usage probe hits, how the payload maps to a snapshot, what a
   failure allows), or is it which account a session spawns under,
   whether an account is healthy, or when it is next probed? ->
   `forge-gateway`, one backend per token and the account pool.
6. **Inbound connector I/O for an external integration?** (its stream
   client, REST lookups, subscription matching, reconnecting
   subsystem pump) -> `forge-connectors`, one module per connector.
   The connector holds no workspace state: Gotify reaches the
   workspace through the `GotifyHost` port forge-workspace implements,
   and Slack holds only its Web API client.
7. **Speaks stream-json to the `claude` subprocess?** (decoder,
   control_request subtype, transport, MCP host, OptionsBuilder)
   -> `forge-sdk`. Pair with a wire-conformance scenario.
8. **Live state about the user's environment?** (git watcher, cwd
   resolution, env probes, OAuth, plugins, settings IO, plugin catalog
   scan)
   -> `forge-agent`: `env::*` for environment, `cloud::*` for Anthropic
   API / OAuth, `userdata::*` for `~/.claude*` files. Async, may shell
   out.
9. **Orchestration across projects, sessions, accounts, `forge.toml`,
   or the command bus?** -> `forge-workspace`. Adds `Workspace` methods,
   `Command` variants, `SessionUpdate` events. A read a VIEW needs is a
   verb on the view surface below, not a bare method.
10. **A session record as a view sees it, or a decision any view would
   make over one?** (the render-ready record, the reducer that derives
   it, the transcript fold that says where a conversation's turns sit, the
   peer envelope parsing) -> `forge-server`. Sits
   between workspace and the views; the test is "does this render?" -
   if it does, it is the view's.
11. **A widget, screen, layout, or per-view presentation state?** -> the
    client: draw it in `client/src/`, and take anything that needs the
    OS, a process or the socket to the shell behind the typed invoke
    surface. A further Rust view is a crate of its own, built against
    the socket in `forge-server` rather than against the core: it reads
    through the view surface in `forge-server` and never
    `forge-workspace`, and starts no subsystem of its own.
12. **A wire-conformance scenario?** -> `forge-test-harness`.

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
core. All of them exist in `forge-server`, and the client reads its
project roster and agent rows, the
task board's fleet and per-project rows through `board`, the
account pool, the worker registry, connector subscriptions and dictation
state, the claude version through `cli_version`, its
composer's data through `slash_commands`, `forge_commands`, `subagents`,
`file_index` and `respect_gitignore`, the plugin inventory, its PATH
checks and its last update run through `extensions`, and the conversation,
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

Legitimate splits are common (a git-diff feature touches agent +
workspace + server). Rule of thumb: logic/IO/subprocess -> agent;
cross-crate shape -> primitives; multi-session state -> workspace;
a session record as a view sees it -> forge-server; anything a person
sees -> the client. When unsure between a shallow crate and a deeper
one, bias toward the deeper crate.

### Anti-patterns (caught in review repeatedly)

- **Work in a view that belongs behind the port.** A page draws; anything
  that needs the OS, a process, the socket or the browser host goes to
  the shell's typed invoke surface. In Rust, `tokio::process::Command`
  belongs in `forge-agent::env::*` and arrives through workspace's
  re-export - a scan awaited off the render path, its result returned
  over a channel of its own.
- **A `SessionUpdate` variant for purely view-internal data.** If the
  producer and the only consumer are the same view, use a channel
  between its own halves rather than the shared vocabulary every view
  subscribes to.
- **Cross-crate type duplication.** Same-shaped `Foo` in two crates
  means one is wrong; lift to primitives or import the re-export. The
  exceptions are the leaf crates that may not depend on any forge-*
  crate, `forge-capture`, `forge-system-one` and `forge-dictate` (which
  leans on the first and on nothing else of the workspace): their types
  stay in them and consumers import them from there.
- **Provider dispatch outside `forge-gateway`.** A match on
  `Provider` in workspace or a view is the thing this crate exists to
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
- **The client stack** is whatever draws: the Svelte page in `client/`
  today, whatever lands beside it after. Glyphs, colours, weights,
  spacing, an order
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
- **A presentation the client needs moves into the client; it is never
  dropped.** A thing both need keeps its home on the server with only
  the presentation half leaving, so the server stays the full statement
  of what the data is and any view draws it from there.

## Communication contract (MVVM)

The core's contract to a view is **one entry point in each direction**:

- **view -> core:** `Workspace::dispatch(Command)`. One enum, one
  entry point, every user-driven action.
- **core -> view:** `SessionUpdate` via `Workspace::subscribe()`. Every
  caller gets a stream of its own, so a second view attaches beside the
  one that exists; a stream carries what
  the workspace emits after that call, and the first caller to attach
  is handed what was emitted before it as well, so a notice raised
  during boot is not lost.

That is the whole contract: no callback hooks, no shared mutable state.
A view holds no `Arc<AgentHandle>`; query-style
refreshes (`refresh_context_usage`, `refresh_mcp_snapshot`) are direct
`Workspace` methods
rather than Command variants. `DomainSession` keeps only
workspace-internal routing metadata; all operational state a view
renders lives on the session record `forge-server` derives for it.

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

**One nuance that surprises people:** `forge-workspace` is a **thin
facade, not strong isolation**. The boundary is enforced at the
dependency graph (the client is out of process entirely), not by
visibility: workspace re-exports the forge-agent submodules the server
reads, so the server sees agent's surface under the workspace's name.

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
   ending on a verdict line that names the first failing step. It must
   be green before a PR, and so must `just check-feature-configs`, which
   compiles the configuration `install.sh` builds: `just check` runs
   with `--all-features`, so it turns every feature on regardless and
   never exercises which configuration enables what. Read that line, not
   a pipeline's exit status - `just check | tail` reports tail's.
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
11. **The client's book pages are visual truth.** The book's client
    surface pages under `docs/book/src/ui/client/` - each page with the
    `web-*.html` drawing beside it - are the source of truth for every
    surface the client can currently draw. Scope is
    **current state only** - no future ideas, no aspirational sketches.
    Anything new arrives in the same PR that lands the code.

    **The workflow for a client change**, and the recommended path for
    any session that starts with one:

    1. Read the relevant `docs/book/src/ui/client/` page first to
       confirm what is currently implemented and where it lives.
    2. Sketch the change in the page - update the drawing's markup,
       its prose, and any table entries.
    3. Apply the same change in the client.
    4. Rebuild the book (`mdbook build docs/book`) and check the page
       still matches the code.
    5. Push them together, code + page in one PR.

    The page-first step forces a clear visual target before code edits
    begin, and it keeps the doc honest. When in doubt about whether a
    page reflects reality, re-read the implementation and reconcile.
12. **Diagnostics are self-serve.** The artifacts are the tracing log
    under forge's app-support `logs/` dir, the client's own frame log
    (`forge-perf.log` beside its `forge.log`), plus the JSONL session
    captures. Telemetry that needs a user to opt in is a forge bug: fix
    the always-on instrumentation instead of asking for a repro recipe.
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
    `$RUST_LOG` / `$NO_COLOR`, and effort overrides like
    `$CLAUDE_CODE_EFFORT_LEVEL`.
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
    `DYNAMIC_WORKER_RESTART_NOTE`, so the search is not confined to the
    crates named here. The
    cross-project rule shows the cost: it lived only in peer tool
    descriptions, read once a peer tool was already in hand, until #733
    added an always-on copy stating it applies when you decide. The
    peer copies stayed.

    **Passing the test is not sufficient: contributor-facing text stays
    in this file.** `scripts/install.sh`'s build recipe (`--locked`, the
    provenance marker) and the unicode gate both pass both halves but
    exist only in this repo. Out for
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
      placement guide, the view contract, the MCP tool
      groups, the single-instance guard, the pointer to the client's
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
    separate `docs/forge-map.html`; today the composer's drawing in the
    client's pages (`docs/book/src/ui/client/web-composer.html`) - and
    touched no other book page. User-visible is not the test; a page
    reading false is.

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
19. **Struck 2026-10-13 with the terminal.** This rule was about the
    terminal forge draws into - multiplexer detection, escape
    forwarding, the OSC 777 notification path and the OSC 9 probe
    history. The terminal crate is deleted; a windowed client owns its
    own notifications, and nothing here carried over. Kept as a number
    so citations do not renumber.

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
    directives in `crates/forge-server/src/logging.rs` are where a record
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
      maintained script vendored beside the client or a maintained
      package beats writing either, because the maintenance we avoid is the
      point - the size of the web ecosystem is the reason this is a web
      view at all.
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
    check` runs the client's steps too - the shell crate's rustfmt check
    (`client-tauri-fmt`), Prettier, ESLint on
    typescript-eslint's type-checked configs, `svelte-check`, `tsc
    --noEmit`, then vitest, then the shell's android warnings check
    (`client-android-warnings`:
    `cargo check --target aarch64-linux-android` with warnings
    denied, the thing that catches desktop-only code going dead on
    Android) - so one command decides both stacks and its
    verdict line names the first failing step. **The shell under
    `client/src-tauri/` is its own workspace root**, so the workspace's
    cargo steps never reach it and no CI job compiles it beyond the
    rustfmt check the `cargo fmt --check` job runs against its manifest;
    `just check`'s `client-tauri-fmt` and `client-android-warnings` reach
    it, and the Unicode punctuation gate
    and the client's Prettier step reach it too.
    `just client-tauri-check` builds it in the shipping configuration
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
    URL, it opens on the last address a connection was kept on and shows the
    connect screen when none does, and nothing bundled ever stands in for a
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
24. **Struck 2026-10-13 with the terminal.** The terminal was the
    reference implementation until it was deleted, and the deletion is
    what this rule's own text said would end it: a parity obligation,
    not a permanent architecture. The inventory that outlived it -
    every surface the terminal drew and how the client covers it - is
    recorded in the removal plan and its issues; a port owed at the
    time kept what it derived. Kept as a number so citations do not
    renumber.
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

    Audience: everything that draws.
26. **One feature, one issue, one fix, one shot: a work cut lands
    whole.** Ved, 2026-10-11: *"one feature, one issue, one fix, one
    shot. I do not want intermediate steps... they should start up a
    dev stack for that same reason, because I want end to end."*

    **A work cut - one feature, one issue, one fix - lands whole: one
    branch, one review, one look, one merge.** Never sliced into
    pieces that land one after another; no intermediate step is shown
    or merged on the way. The look is the one that cut is owed: a
    feature waits for the user's own look before merge; a fix merges
    after the review loop as usual. A plan that sequences the cut as
    "land A, then B, then C", and a first PR that lands part of it
    while the rest waits behind, are both this rule's subject. A cut
    that genuinely cannot land whole is decided with the user BEFORE
    anything is cut down - a smaller PR followed by the rest is the
    failure this names.

    **A slice is reviewed wrong as well as late.** Feedback on a piece
    arrives after part of the cut has merged, each intermediate
    landing spends a review round on a state nobody wanted, and the
    whole is never exercised end to end. The review page's a/b/c plan
    and the board's original landing in pieces are the two cases that
    produced the rule.

    **The dev stack exists so the whole cut is exercised end to end
    before it lands** - for the feature-scale web work that skill is
    scoped to (`.claude/skills/dev-stack/`); every other cut still
    lands whole, without a stack. Rule 11's same-PR book obligation
    rides the whole cut too: the page lands with the thing it
    describes, not with the first slice of it.

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
  ellipsis U+2026 is allowed - it is a real truncation glyph a view uses.

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
