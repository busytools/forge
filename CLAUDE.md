# forge - project guide

A Rust workspace for personal-use agentic tooling around Anthropic's
`claude` CLI. Six crates, layered acyclically:

```
forge-primitives ──── leaf (pure data, no logic)
forge-sdk        ──→ primitives
forge-agent      ──→ primitives + sdk
forge-workspace  ──→ primitives + agent           (the MVVM orchestrator)
forge-tui        ──→ primitives + workspace       (no direct agent dep)
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
  scope): replay-based offline tests + opt-in live capture.

**Single-instance per config dir.** One `forge` owns a config dir
(`$CLAUDE_CONFIG_DIR`, else `~/.claude`) and manages many sessions in
it; a second is refused at boot with the holder's PID. The `flock`
lives on a machine-local lockfile under forge's app-support dir, NOT
the synced config dir, because Syncthing's rename-on-sync would swap
the lock inode out from under a running forge on another Mac.

**Config vs state.** `forge.toml` (under `<config_dir>/forge/`) is the
only file forge reads for config: read-only, hand-authored, safe to
sync. All runtime state (durable crons, Gotify subs, dynamic workers,
`/spinner` override, usage cache) lives in one machine-local redb DB
beside the lock. None of it belongs in the synced config dir: the DB
churns roughly once a minute, redb's binary file can't be
Syncthing-merged, and the lock's inode must stay put.

## Crate placement guide (where does my new code go?)

Work top-down; first match wins.

1. **A type that crosses a crate boundary?** (envelope, snapshot
   struct, hook payload, anything sent over a channel or touched by
   more than one crate) → `forge-primitives`. Pure data shapes only.
2. **Speaks stream-json to the `claude` subprocess?** (decoder,
   control_request subtype, transport, MCP host, OptionsBuilder)
   → `forge-sdk`. Pair with a wire-conformance scenario.
3. **Live state about the user's environment?** (git watcher, cwd
   resolution, env probes, OAuth, plugins, settings IO, catalog scan)
   → `forge-agent`: `env::*` for environment, `cloud::*` for Anthropic
   API / OAuth, `userdata::*` for `~/.claude*` files. Async, may shell
   out.
4. **Orchestration across projects, sessions, accounts, `forge.toml`,
   or the command bus?** → `forge-workspace`. Adds `Workspace` methods,
   `Command` variants, `SessionUpdate` events.
5. **A widget, screen, key binding, mouse handler, or per-session
   presentation state?** → `forge-tui`. Render in `ui/`, dispatch +
   state in `app/`.
6. **A wire-conformance scenario?** → `forge-test-harness`.

Legitimate splits are common (a git-diff feature touches agent +
workspace + tui). Rule of thumb: logic/IO/subprocess → agent;
cross-crate shape → primitives; multi-session state → workspace;
anything the user sees → TUI. The default failure mode here is "too
much in forge-tui", so bias toward the deeper crate when unsure.

### Anti-patterns (caught in review repeatedly)

- **Subprocess calls in `forge-tui`.** `tokio::process::Command`
  belongs in `forge-agent::env::*`, exposed by a workspace method the
  TUI awaits (precedent: `Workspace::scan_git_diff`).
- **A `SessionUpdate` variant for purely TUI-internal data.** If
  producer and consumer are both in forge-tui, use a separate mpsc
  channel (see `git_diff_event_tx/rx`).
- **Cross-crate type duplication.** Same-shaped `Foo` in two crates
  means one is wrong; lift to primitives or import the re-export.
- **Workspace methods bypassing the Command bus for user actions.**
  User-initiated actions go through `dispatch(Command)`; query-style
  refreshes are direct inherent methods. Don't conflate them.

## Communication contract (MVVM)

The TUI ↔ workspace contract is **one channel pair**, single
producer/consumer each direction:

- **TUI → workspace:** `Workspace::dispatch(Command)`. One enum, one
  entry point, every user-driven action.
- **workspace → TUI:** `SessionUpdate` via `Workspace::subscribe()`,
  consumed by `App.update_rx`.

That is the whole contract: no second channel, no callback hooks, no
shared mutable state. TUI holds no `Arc<AgentHandle>`; query-style
refreshes (`refresh_status_snapshot`, `refresh_context_usage`,
`refresh_mcp_snapshot`, …) are direct `Workspace` methods rather than
Command variants. `DomainSession` keeps only workspace-internal routing
metadata; all operational state the TUI renders lives on `UiSession`.

**Two nuances that surprise people:**

- The `SessionUpdate` channel doubles as an event bus for TUI-internal
  async work: a few TUI modules (`app/plugins.rs`,
  `app/slash/executors.rs`, `app/service_status_check.rs`,
  `app/input_submit.rs`) grab `Workspace::update_sender()` and emit
  their own updates instead of a Command round-trip. This is an
  implicit second contract a non-TUI frontend would have to replicate.
  Tracked in issue #105, not high priority.
- `forge-workspace` is a **thin facade, not strong isolation**. The
  boundary is enforced at the dependency graph (forge-tui has no
  forge-agent dep), not by visibility: workspace wildcard-re-exports
  forge-agent submodules, so TUI sees agent's surface verbatim under a
  different name.

Full detail lives in `project_mvvm_communication_contract.md` in
auto-memory. Read it before touching this boundary.

## Project scope

**Personal use only.** Single user across multiple Macs. No public
release, no multi-tenant threat model. Read `project_trust_model.md`
in auto-memory before any audit or security-hardening work: findings
whose severity depends on adversarial assumptions get demoted or
dropped.

Direction: forge is converging on multi-agent peer coordination (peers
+ workers MCP, git worktrees as a first-class primitive). Epics #114
and #115.

**Claude Code worktree interop facts** (non-guessable external
conventions, so forge doesn't reinvent them):

- `--worktree [name]` defaults to in-repo
  `<repo>/.claude/worktrees/<name>/`, NOT a sibling dir. Forge matches.
- `EnterWorktree` / `ExitWorktree` are built-in tools the LLM can call;
  forge decides per session whether to block them in favour of
  `mcp__forge__*`.
- `AgentInput.isolation: "worktree"` is Claude's native auto-worktree
  for Task subagents, separate from forge's MCP worktree path.
- The wire envelope carries `worktree: {name, path, branch,
  original_cwd, original_branch}`; hook events `WorktreeCreate` /
  `WorktreeRemove` are first-class.
- `.worktreeinclude` (repo-root, gitignore syntax) is the convention
  for copying gitignored files into a new worktree.
- `worktree.baseRef = "fresh" | "head"` picks origin/HEAD vs local HEAD.

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
- **Stream-json wire compatibility is mandatory.** If our stdin/stdout
  differs from what `claude` expects, that's a bug.

## Hard rules

1. **The `claude` binary is source of truth.** Spawn it, speak
   stream-json, never reach the Anthropic API directly.
2. **Stream-json wire-compatibility with `claude`.** Byte-identical
   to what `claude` expects on stdin and what we decode from its
   stdout. The wire-conformance harness is the enforcement
   mechanism.
3. **TDD discipline.** Failing test → run it → watch it fail →
   implement → run it → watch it pass → commit. Apply when the test
   shape is obvious; for exploratory refactors, integration tests
   are sufficient.
4. **Frequent commits.** One logical unit = one commit. Commits are
   small and reversible.
5. **No `mod.rs`.** Module files sit next to their directory
   (`foo.rs` + `foo/`).
6. **Nightly Rust, pinned.** `rust-toolchain.toml` locks a specific
   nightly date. Bump deliberately.
7. **Clippy pedantic + deny `unwrap_used` / `expect_used` /
   `panic` / `exit` / `todo` / `unimplemented`** in non-test code.
8. **`cargo nextest run`, not `cargo test`.** Locally use `just
   check` to run fmt + clippy + nextest + docs in one shot.
9. **Wire-conformance harness is mandatory for new wire surface.**
   New control_request subtypes, message types, hook events, tool
   integrations ship with: (a) a live-capture scenario, (b) the
   captured baseline trace under
   `crates/forge-test-harness/baselines/sdk/<PINNED_CLI_VERSION>/`,
   (c) clean replay so every inbound line round-trips through the
   decoder without `DecodedLine::Unknown` or decode errors.
10. **Never commit LLM-generated planning docs.** User plans stay at
    `~/.claude-subspace/plans/`.
11. **`docs/forge-map.html` is visual truth.** This file is the
    source of truth for every UI surface forge-tui can currently
    render. Scope is **current state only** - no future ideas, no
    aspirational sketches, no "v3+" sections. Anything new arrives
    in the same PR that lands the code.

    **The workflow when the user asks for a UI change** (start every
    such session with this - it's the recommended path):

    1. Read `docs/forge-map.html` first to confirm what's currently
       implemented and where it lives.
    2. Sketch the change in HTML - update the relevant section's
       mockup, prose, and any glyph/colour table entries.
    3. Apply the same change in the ratatui code.
    4. Verify the rendered HTML still matches the code (open the
       file in a browser, eyeball it).
    5. Push both files together - code + HTML in one PR.

    The HTML-first step matters because it forces a clear visual
    target before code edits begin, and it keeps the doc honest
    (the doc never describes something the code doesn't ship).
    When in doubt about whether the doc reflects reality, re-read
    the implementation and reconcile - never let prose drift from
    code.
12. **Gated actions still gated.** `cargo publish` and `git push
    --force` to `main` need explicit approval. Routine per project
    override: feature-branch pushes, PR creation, `gh pr comment`,
    non-force push to `main` for milestone landings, `git tag` +
    push, and `gh pr merge`.
13. **Diagnostics are self-serve** (now a global rule; forge specifics
    only). The artifacts: perf log + tracing log under
    `~/Library/Application Support/forge-tui/logs/`, and JSONL session
    captures. Telemetry that needs user opt-in is a forge bug - fix
    the always-on instrumentation instead of asking the user to
    enable it.
14. **Diagnostic-improvement TODOs go to GitHub issues, not inline.**
    When a diagnostic feature ships but follow-on improvements are
    deferred (rotation, configurable thresholds, …), file a GitHub
    issue for the deferred work. Do NOT scatter aspirational
    `TODO:` comments through the source. Inline TODOs rot, mislead
    future readers, and have no central triage view. When the issue
    is resolved, remove any code references to it. Concrete-fix
    `TODO:` comments naming a specific 1-3 line change are still
    fine; vague "consider adding X someday" TODOs belong in issues.
15. **`forge.toml` is the source of truth. Never read project state
    or any other behaviour-shaping value from the launch directory.**
    Project paths, names, accounts, auto_start pins, log paths,
    settings paths, trust keys, file-index roots - everything that
    influences forge's behaviour must come from `forge.toml`, the
    active session's `cwd_raw`, or other fixed values. The binary's
    launch directory (`std::env::current_dir()`) MUST NOT influence
    anything. Strict binary trigger: **"does forge behave identically
    when launched from `~/Projects/forge` and from `/tmp`?"** If
    there is ANY observable difference - different project loaded,
    different settings file, different trust prompt, different log
    directory, different welcome banner cwd, anything - that's a
    bug, fix it. Cosmetic differences count too. The correct fallback
    when a needed user-dir (state / cache / home) is unavailable is to
    FAIL the operation, not to substitute a cwd-derived alternative.
    The active session bucket carries its own `cwd_raw` (sourced from
    `forge.toml` at boot, or from the agent's reported cwd
    post-Connect); read from there, or look up the project via
    `Workspace::list_projects()` / `find_project_view_by_name`. When
    you spot an env-derived project-state read while doing other work,
    fix it on the spot and audit for the same pattern - they cluster.
    The ONLY env reads still allowed are ones that do NOT vary with
    launch directory: `$CLAUDE_CONFIG_DIR` (set by the `claude_*`
    wrappers, identical for every `cd`), `$HOME` /
    `dirs::home_dir()`, `$RUST_LOG` / `$NO_COLOR`, `env::vars_os()`
    for terminal capability detection (`$TERM_PROGRAM`,
    `$ITERM_SESSION_ID`), and user-supplied `--effort` overrides like
    `$CLAUDE_CODE_EFFORT_LEVEL`.
16. **Wire classification matches reality. Forge sessions appear as
    `cli` (interactive subscription) on every Anthropic and Datadog
    signal channel - byte-equivalent to a real interactive `claude`
    session.** The wire-classification rewriter
    (`forge-sdk::transport::proxy`) is the enforcement mechanism: it
    intercepts every spawned `claude` child's HTTPS traffic via
    `HTTPS_PROXY` + `NODE_EXTRA_CA_CERTS` and recursively normalises
    `entrypoint` / `client_type` / `is_interactive` to `cli` / `cli` /
    `true`, drops `agent_sdk_version` keys, and rewrites the
    bootstrap-query and User-Agent surfaces. Hard-fail at
    `Workspace::new`: if the proxy can't bind, load CA, or build TLS,
    forge refuses to spawn any session.

    Concrete invariants any future change MUST preserve:
    - `forge-sdk::transport::process::spawn` does NOT stamp
      `CLAUDE_CODE_ENTRYPOINT`. The CLI self-classifies (`sdk-cli` for
      piped stdout) and the rewriter handles the wire shape.
      Re-introducing the stamp leaks an unknown entrypoint string
      through telemetry surfaces the rewriter doesn't yet cover.
    - The rewriter scope is anthropic.com + datadoghq.com hosts. Don't
      narrow further (each removed host is a leaked channel) and
      don't widen to third-party MCP hosts without explicit need.
    - The recursive normaliser
      (`normalize_classification_fields`) is the source of truth for
      what gets rewritten. Per-channel functions
      (`rewrite_event_logging`, `rewrite_statsig_features`,
      `rewrite_datadog_logs`) are thin wrappers - if you reach for a
      hard-coded JSON path instead, the recursive walker is doing the
      work; keep that path.
    - When `HTTPS_PROXY` is set in the parent env at forge launch, the
      rewriter chains its outbound HTTPS through that upstream proxy
      (and extends its trust store via `NODE_EXTRA_CA_CERTS` if set).
      This keeps the mitmproxy capture recipe symmetric: the same env
      vars that capture from a bare `claude` invocation also capture
      from forge. Breaking that symmetry is a regression - the goal is
      "indistinguishable from real `claude` on the wire", which
      requires identical capture ergonomics for any observer.
    - The defensive scanner (`scan_and_warn`) runs on every Anthropic
      / Datadog body and logs at `warn` when an `sdk-*` value,
      non-`cli` `entrypoint`/`client_type`, false `is_interactive`,
      or `agent_sdk_version` slips through. Treat any non-empty scan
      output as a drift signal; the fix is to extend the recursive
      normaliser, not to silence the scan.
17. **A scoped change must not alter anything else observable.** A
    performance fix changes only speed. A UI change changes only that
    surface. If the scoped change *requires* touching behaviour
    elsewhere, that part is a separate PR, presented on its own terms
    with the behaviour effect as the headline - not a side note in the
    original. **Splitting it into a second commit is not sufficient**:
    that reads as disclosure while still bundling the decision.

    At plan time, state what the change may and may not alter. At
    review time, ask what the user would SEE that is different, and
    treat any non-empty answer on a perf or refactor change as a
    blocker until it is split out and decided on its own.

    Worked example: #543 fixed a 6.5s startup stall in two commits.
    The first suspended internal accounting during resume replay for a
    3x, provably identical final state, nothing observable. The second
    gave the retention budget 12.5% slack for a further 6x - and
    retained ~780 more messages of scrollback and moved the
    history-hidden marker. The second was dropped, not deferred. It
    was small and arguably an improvement; the scope was "make startup
    faster" and it changed scrollback.

## Style + Rust idiom

Baseline Rust conventions (toolchain, error types, lints, commands)
live in `~/.claude/memory/code-conventions.md`. forge-specific:

- **Attributes are rare; default to none.** `#[non_exhaustive]`,
  `#[must_use]`, `#[allow(...)]`, `#[deprecated]`, `#[doc(hidden)]`
  need a specific documentable reason. forge is workspace-internal, so
  compile breaks on enum/struct evolution are the point, not something
  `#[non_exhaustive]` should paper over. `#[allow(...)]` on production
  code is dead-code or stale-lint debt: fix the cause, drop the marker.
- **Channels-based APIs over `&mut self`.** For types used across
  tasks, expose channels or `&self` methods rather than forcing a
  Mutex or actor wrapper on the caller.
- **Subprocess:** `tokio::process::Command` for streaming I/O,
  `cmd_lib` for fire-and-forget shell.
- **Tracing only.** Never `println!` / `eprintln!` in library code;
  binaries may use `eprintln!` only when tracing itself failed.

## Workflows in skills

- `.claude/skills/claude-cli-upgrade/` - CLI version bumps, baseline
  regeneration, and the wire-conformance cheatsheet (capture command,
  baseline layout, adding a scenario).
- `.claude/skills/wire-equivalence-check/` - proving forge is
  wire-indistinguishable from native `claude`.
- `.claude/skills/upstream-watch/` - the weekly Python
  `claude-agent-sdk` idea-scan (forge does not feature-parity-track it).

## Team context (for team-lead agents)

- Proactive memory: `~/.claude-*/projects/-Users-vedhavyas-Projects-forge/memory/`.
- Cross-project TIL: `~/.claude/memory/til/`.
- Cross-project dispatch goes through the peer MCP
  (`mcp__forge__peers__tell_agent` / `ask_agent`), not a shell command.
  Other project agents: aware, dotfiles, granite-backend, hub-modules,
  nf-core, subspace, trader-cc, architect.

## Who to ask when in doubt

The user. Direct, concise; use `AskUserQuestion` for structured
decisions. Never silently work around ambiguities.
