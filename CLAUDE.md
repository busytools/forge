# forge - project guide

A Rust workspace that wraps Anthropic's `claude` CLI in a multi-session
terminal UI. Nine crates, layered acyclically:

```
forge-primitives ───── leaf (pure data, no logic)
forge-dictate    ───── leaf (dictation; depends on no forge-* crate)
forge-providers  ───→ primitives
forge-connectors ───→ primitives
forge-sdk        ───→ primitives
forge-agent      ───→ primitives + sdk + providers
forge-workspace  ───→ primitives + agent + sdk + dictate + providers + connectors
forge-tui        ───→ primitives + workspace      (no direct agent dep)
forge-test-harness ─→ primitives + sdk + workspace
```

- **`forge-primitives`** - every type that crosses a forge-* crate
  boundary. No logic, no I/O, no async.
- **`forge-dictate`** - the dictation primitive: audio in, text out.
  Owns its model files, speech recognition and normalization. Depends
  on no forge-* crate and knows nothing about a host, so it must not
  grow one; a doc comment mentioning a keypress, a composer or a
  session is a bug.
- **`forge-providers`** - one backend per `forge.toml` provider token:
  credential resolution, the usage probe's HTTP + payload mapping,
  billing shape, the OpenRouter model catalog. Depends on
  forge-primitives only; the keychain, the `claude --version` user
  agent and the TLS-trust client arrive through the `ProviderHost`
  port forge-agent implements, so the crate stays HTTP + mapping and
  never spawns the CLI.
- **`forge-connectors`** - one module per inbound connector: the
  stream client, REST lookups, subscription matching and subsystem
  pump for one external integration (Gotify today). Depends on
  forge-primitives only; the subscription set, the app index and
  message dispatch into sessions arrive through the `GotifyHost` port
  forge-workspace implements, so the crate stays stream + mapping and
  holds no workspace state. No generic connector trait: one connector
  exists, and a trait waits until the variety is real.
- **`forge-sdk`** - owns the `claude` subprocess: stream-json codec,
  transport, control dispatch, in-process MCP host, Options.
- **`forge-agent`** - drives one SDK Client behind a channel-based
  `Agent`/`AgentHandle`. Owns userdata, cloud, env, translate, tooling.
- **`forge-workspace`** - multi-session orchestrator. Owns
  `DomainSession` + per-session `SessionTask` actors. Single TUI-facing
  facade.
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
sync. All runtime state (durable crons, Gotify subs, dynamic workers,
`/spinner` override, usage cache) lives in one machine-local redb DB
beside the lock. None of it belongs in a synced config dir: the DB
churns roughly once a minute, redb's binary file cannot be merged, and
the lock's inode must stay put.

## Crate placement guide (where does my new code go?)

Work top-down; first match wins.

1. **Audio, speech recognition, or turning either into text?** (capture,
   model fetch, transcription, transcript normalization)
   -> `forge-dictate`. A leaf: it may not depend on any forge-* crate,
   and its own types stay there even once another crate reads them.
   Wanting a forge-* dependency here is a design problem, not a
   dependency problem.
2. **A type that crosses a crate boundary?** (envelope, snapshot
   struct, hook payload, anything sent over a channel or touched by
   more than one crate) -> `forge-primitives`. Pure data shapes only.
3. **Provider credential, probe, usage mapping, billing or repair?**
   (how one `forge.toml` provider token authenticates, what endpoint
   its usage probe hits, how the payload maps to a snapshot, what a
   failure allows) -> `forge-providers`, one backend per token.
4. **Inbound connector I/O for an external integration?** (its stream
   client, REST lookups, subscription matching, reconnecting
   subsystem pump) -> `forge-connectors`, one module per connector.
   The connector holds no workspace state; what it needs from the
   workspace arrives through the `GotifyHost` port forge-workspace
   implements.
5. **Speaks stream-json to the `claude` subprocess?** (decoder,
   control_request subtype, transport, MCP host, OptionsBuilder)
   -> `forge-sdk`. Pair with a wire-conformance scenario.
6. **Live state about the user's environment?** (git watcher, cwd
   resolution, env probes, OAuth, plugins, settings IO, plugin catalog
   scan)
   -> `forge-agent`: `env::*` for environment, `cloud::*` for Anthropic
   API / OAuth, `userdata::*` for `~/.claude*` files. Async, may shell
   out.
7. **Orchestration across projects, sessions, accounts, `forge.toml`,
   or the command bus?** -> `forge-workspace`. Adds `Workspace` methods,
   `Command` variants, `SessionUpdate` events.
8. **A widget, screen, key binding, mouse handler, or per-session
   presentation state?** -> `forge-tui`. Render in `ui/`, dispatch +
   state in `app/`.
9. **A wire-conformance scenario?** -> `forge-test-harness`.

Legitimate splits are common (a git-diff feature touches agent +
workspace + tui). Rule of thumb: logic/IO/subprocess -> agent;
cross-crate shape -> primitives; multi-session state -> workspace;
anything the user sees -> TUI. The default failure mode here is "too
much in forge-tui", so bias toward the deeper crate when unsure.

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
  one exception is `forge-dictate`, which may not depend on primitives
  at all: its types stay in it and consumers import them from there.
- **Provider dispatch outside `forge-providers`.** A match on
  `Provider` in workspace or tui is the thing this crate exists to
  delete; route through `forge_providers::backend(token)` instead.
- **Workspace methods bypassing the Command bus for user actions.**
  User-initiated actions go through `dispatch(Command)`; query-style
  refreshes are direct inherent methods. Don't conflate them.

## Communication contract (MVVM)

The TUI to workspace contract is **one channel pair**, single
producer/consumer each direction:

- **TUI -> workspace:** `Workspace::dispatch(Command)`. One enum, one
  entry point, every user-driven action.
- **workspace -> TUI:** `SessionUpdate` via `Workspace::subscribe()`,
  consumed by `App.update_rx`.

That is the whole contract: no second channel, no callback hooks, no
shared mutable state. TUI holds no `Arc<AgentHandle>`; query-style
refreshes (`refresh_status_snapshot`, `refresh_context_usage`,
`refresh_mcp_snapshot`, and friends) are direct `Workspace` methods
rather than Command variants. `DomainSession` keeps only
workspace-internal routing metadata; all operational state the TUI
renders lives on `UiSession`.

**Two nuances that surprise people:**

- The `SessionUpdate` channel doubles as an event bus for TUI-internal
  async work. `App` caches the sender from `Workspace::update_sender()`
  at construction, and four modules (`app/plugins.rs`,
  `app/slash/executors.rs`, `app/service_status_check.rs`,
  `app/input_submit.rs`) emit their own updates through it instead of a
  Command round-trip. That is an implicit second contract a non-TUI
  frontend would have to replicate.
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
- **Credentials come from the user's own `~/.claude*` dirs** and the
  system keychain. forge reads them; it does not manage or isolate them.
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
   the unicode-punctuation gate, clippy, nextest and docs in one shot;
   it must be green before a PR.
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
11. **`docs/forge-map.html` is visual truth.** It is the source of truth
    for every UI surface forge-tui can currently render. Scope is
    **current state only** - no future ideas, no aspirational sketches.
    Anything new arrives in the same PR that lands the code.

    **The workflow for a UI change**, and the recommended path for any
    session that starts with one:

    1. Read `docs/forge-map.html` first to confirm what is currently
       implemented and where it lives.
    2. Sketch the change in HTML - update the relevant section's
       mockup, prose, and any glyph or colour table entries.
    3. Apply the same change in the ratatui code.
    4. Open the HTML in a browser and check it still matches the code.
    5. Push both files together, code + HTML in one PR.

    The HTML-first step forces a clear visual target before code edits
    begin, and it keeps the doc honest. When in doubt about whether the
    doc reflects reality, re-read the implementation and reconcile.
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
    correct state.** The despawn trigger is the worked example. Some
    surfaces name a merged PR among examples; some stop at "truly done",
    including `workers__spawn`, in the same file well above the hunk
    #717 edited; and the charter separately says "once they have
    delivered". #717 wrote both the first of those and the last: it
    removed "once its work is merged" because that exempted every worker
    whose output was not a PR, and cited `LEAD_DELEGATION_PREAMBLE`'s
    softer wording without adopting it, leaving the preamble untouched.
    An odd phrasing is not evidence it was overlooked; check each site's
    history rather than its wording.

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
    map, with a different audience.** Rule 11 owns
    `docs/forge-map.html`, the maintainer's visual record. This rule
    covers `docs/book/`, the site users read at
    https://busytools.github.io/forge/.

    **The publish is automatic and the content is not.**
    `.github/workflows/docs.yml` builds `docs/book` and deploys to
    Pages on every push to `main`, while a pull request skips the
    deploy. mdbook renders hand-written markdown and no page
    derives from the code, so a change that edits no page republishes
    the old description within minutes of the merge, with a green
    build and a green deploy beside it. The site is never out of date
    with the repo, and it can be confidently wrong about the code.

    The pages, and the usual way each goes false:

    - `configuration.md` - every `forge.toml` key, its default and its
      error strings. The most exposed page in the book: a new key, a
      changed default or a reworded load failure falsifies it.
    - `install.md` - prerequisites, the CLI flag table, and the fullest
      copy of `just check`'s composition. A changed flag, prerequisite
      or step falsifies it; a recipe missing from its avowedly partial
      `just` list does not.
    - `index.md` - what forge is, the surfaces it renders, the scope
      caveats.
    - `architecture.md` - crate count, layering diagram, crate table,
      placement guide, the TUI-to-workspace contract, the MCP tool
      groups, the single-instance guard, the pointer to the surface
      map. Its content is mirrored in `README.md` and this file.
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
    first Enter, which did owe `docs/forge-map.html` and correctly
    touched no book page, because no page describes per-key prompt
    behaviour. User-visible is not the test; a page reading false is.

    #751 is the other shape, and it is not clean. Adding the seventh
    crate falsified `architecture.md`'s crate count, layering diagram,
    crate table and placement guide: four edits on one page, where
    stopping at the table leaves the other three reading false. It also
    falsified `install.md` and did not update it. The crate builds
    under every `--workspace` command and needs ALSA headers on Linux,
    so CI grew a `libasound2-dev` step in four jobs while the
    prerequisites list named four things and not that one.

    **The layering diagrams differ in grain deliberately. Do not
    reconcile them.** This file draws `forge-test-harness` on
    primitives + sdk + workspace; `README.md` and `architecture.md`
    draw it on primitives + sdk. `forge-workspace` is a dev-dependency
    of the harness, so both are true and neither is stale. Naming
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
    already has the right path and skips it. `notification_plan`
    (`crates/forge-tui/src/app/notify.rs`) sets `send_desktop:
    osc9_text.is_none()`, so believing the terminal speaks OSC 9
    suppresses the `notify-rust` desktop notification, which reaches
    the OS without crossing the terminal at all. That belief comes
    from `terminal_capabilities_from_env` reading `TERM_PROGRAM` and
    `ITERM_SESSION_ID`. Measured: both reach a pane under zellij
    0.44.3 and under GNU screen 4.00.03 unchanged, while an OSC 9
    emitted inside either does not reach the outer pty, with plain
    text written on both sides of it arriving normally. shpool 0.9.8
    drops it deliberately; its vendored vterm carries the literals
    `ignoring OSC 9 (desktop notification)` and `ignoring OSC 777`.
    So the check answers whether the terminal supports OSC 9 when
    the question is whether an OSC 9 survives to it. The outcome is
    silence rather than a degraded notification: `Iterm2` and
    `Ghostty`, two of the five channels and the default among them,
    leave `ring_bell` and `send_desktop` both false when OSC 9 is
    believed available - `Iterm2` keys both on it, `Ghostty`'s bell
    is off regardless - so an eaten escape leaves nothing at all.

    What crosses is decided per sequence by the thing in the middle,
    and no one capability answers it for every sequence. tmux
    re-emits some pane-originated OSC from its own terminfo: OSC 8
    when the outer terminal carries `Hls`, and since 3.7 the OSC 9;4
    progress bar via `Spb`. The OSC 9 NOTIFICATION form is not among
    them - `input_osc_9` returns on any payload not starting `4` -
    and that is the form forge emits. Carrying an arbitrary sequence
    out takes a DCS envelope, and the price differs per multiplexer:
    tmux wants a `tmux;` prefix, `allow-passthrough` at `on` for a
    visible pane or `all` for any (tri-state since 3.4, default
    `off`), and every ESC in the payload doubled, the doubling being
    the one requirement `tmux.1` never states. screen forwards a bare
    DCS-wrapped OSC 9 with no opt-in and no doubling. The screen half
    is measured; the tmux half is read from source, at 3.7c except
    where an earlier tag is named.

    **Where no multiplexer-independent path exists, state the
    requirement and detect its absence.** Depending on a sequence is
    allowed. Depending on it silently is not, because a feature that
    quietly does nothing reads as forge being broken rather than as
    the multiplexer eating it.

    **forge has no example of that clause to copy, and #767 is the
    gap.** `resume_terminal` (`crates/forge-tui/src/app.rs`) pushes
    the kitty enhancement flags because `SUPER` arrives no other way,
    discards the result, and never asks whether they took;
    `supports_keyboard_enhancement` is crossterm public API and
    appears nowhere in `crates/`. #767 is open on exactly that: under
    a byte-transparent session manager the flags live on the terminal
    rather than the session, so a reattach silently stops delivering
    what the protocol provides. `is_cmd_shortcut` in `app/keys.rs`,
    which accepts `CONTROL` where `SUPER` cannot arrive, is worth
    reading, but accepting a substitute is a fallback and not a
    detection, and treating one as the other is how this rule gets
    satisfied on paper.

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
  binaries may use `eprintln!` only when tracing itself failed.
- **Comments earn their place.** What the code does, never. Why, only
  when a reader would otherwise ask and cannot infer it from names or
  surrounding code. Non-obvious gotchas, external constraints and API
  quirks are exactly what comments are for.
- **No unicode punctuation.** Em-dashes, en-dashes, horizontal bars and
  curly quotes are rejected by `just check`. Where a codepoint is
  functionally required, use the escape form (`"\u{2014}"`). The
  ellipsis U+2026 is allowed - it is a real truncation glyph in the TUI.

## Releases

`just release <version>` bumps the workspace version, commits and tags
locally. It deliberately stops there: pushing a tag and cutting a
release are the maintainer's call, and `just check-release` gates the
recipe because `cargo install` builds in release mode and would
otherwise find the error after the tag exists.

## Workflows in skills

- `.claude/skills/claude-cli-upgrade/` - CLI version bumps, baseline
  regeneration, and the wire-conformance cheatsheet.
