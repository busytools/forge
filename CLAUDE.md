# forge - project guide

A Rust workspace that wraps Anthropic's `claude` CLI in a multi-session
terminal UI. Six crates, layered acyclically:

```
forge-primitives ───── leaf (pure data, no logic)
forge-sdk        ───→ primitives
forge-agent      ───→ primitives + sdk
forge-workspace  ───→ primitives + agent + sdk    (the MVVM orchestrator)
forge-tui        ───→ primitives + workspace      (no direct agent dep)
forge-test-harness ─→ primitives + sdk + workspace
```

- **`forge-primitives`** - every type that crosses a forge-* crate
  boundary. No logic, no I/O, no async.
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

1. **A type that crosses a crate boundary?** (envelope, snapshot
   struct, hook payload, anything sent over a channel or touched by
   more than one crate) -> `forge-primitives`. Pure data shapes only.
2. **Speaks stream-json to the `claude` subprocess?** (decoder,
   control_request subtype, transport, MCP host, OptionsBuilder)
   -> `forge-sdk`. Pair with a wire-conformance scenario.
3. **Live state about the user's environment?** (git watcher, cwd
   resolution, env probes, OAuth, plugins, settings IO, catalog scan)
   -> `forge-agent`: `env::*` for environment, `cloud::*` for Anthropic
   API / OAuth, `userdata::*` for `~/.claude*` files. Async, may shell
   out.
4. **Orchestration across projects, sessions, accounts, `forge.toml`,
   or the command bus?** -> `forge-workspace`. Adds `Workspace` methods,
   `Command` variants, `SessionUpdate` events.
5. **A widget, screen, key binding, mouse handler, or per-session
   presentation state?** -> `forge-tui`. Render in `ui/`, dispatch +
   state in `app/`.
6. **A wire-conformance scenario?** -> `forge-test-harness`.

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
  means one is wrong; lift to primitives or import the re-export.
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

- **forge MITMs its own child's HTTPS.** To keep sessions
  wire-indistinguishable from a native `claude` run (see below), forge
  runs a local rewriting proxy and installs its CA into the system trust
  store. That CA is generated per machine and used only for the proxy,
  but it is real trust-store surface, and `scripts/install-cert.sh`
  exists to add and remove it.
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
compared, it does not compare recorded outbound content against what
forge would send today, so a baseline can carry a stale copy of
forge's own MCP tool names and stay green.

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
15. **Wire classification matches reality.** A forge session appears as
    `cli` - an interactive subscription session - on every Anthropic and
    Datadog signal channel, byte-equivalent to a real interactive
    `claude` run. The rewriter in `forge-sdk::transport::proxy` is the
    enforcement mechanism, and `Workspace::new` hard-fails if it cannot
    bind, load its CA, or build TLS: no proxy, no sessions.

    **`proxy.rs`'s own module docs are the authority on what it
    rewrites**, including which of the four rewrites are host-scoped.
    Do not restate that list here; a second copy is how this rule came
    to describe a scope the code does not have. Two things worth knowing
    before touching it:

    - The **User-Agent rewrite applies to every outbound request**,
      third-party MCP servers included, and that is deliberate: an
      un-normalised UA identifies forge to any observer. The other three
      rewrites are gated on Anthropic or Datadog hosts.
    - The recursive normaliser is the place to extend. If you find
      yourself adding a hard-coded JSON path, the walker was already
      going to reach it.

    Two invariants that live outside `proxy.rs` and so belong here:

    - `forge-sdk::transport::process::spawn` must NOT stamp
      `CLAUDE_CODE_ENTRYPOINT`. The CLI self-classifies and the rewriter
      handles the wire shape; re-introducing the stamp leaks an unknown
      entrypoint string through surfaces the rewriter does not cover.
    - When `HTTPS_PROXY` is set in the parent env, the rewriter chains
      its outbound HTTPS through that upstream proxy and extends its
      trust store from `NODE_EXTRA_CA_CERTS`. That symmetry is the
      point: the same env vars that capture a bare `claude` invocation
      capture forge too. "Indistinguishable on the wire" requires
      identical capture ergonomics for any observer, so breaking it is a
      regression. `.claude/skills/wire-equivalence-check/` is how this
      gets checked.
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
- `.claude/skills/wire-equivalence-check/` - proving forge is
  wire-indistinguishable from native `claude`.
