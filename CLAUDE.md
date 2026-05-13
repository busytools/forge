# forge — project guide

A Rust workspace for personal-use agentic tooling around Anthropic's
`claude` CLI. Six components, layered acyclically:

```
forge-primitives ──── leaf (pure data, no logic)
forge-sdk        ──→ primitives
forge-agent      ──→ primitives + sdk
forge-workspace  ──→ primitives + agent           (the MVVM orchestrator)
forge-tui        ──→ primitives + workspace       (no direct agent dep)
```

- **`forge-primitives`** — workspace-shared wire-shape types. Message
  envelopes, content blocks, hook/permission/option/subagent data,
  render-side views, channel commands, IDs. No logic, no I/O, no
  async. Every type that crosses any forge-* crate boundary lives
  here.
- **`forge-sdk`** — wraps the `claude` CLI subprocess. Owns the
  stream-json codec, transport, control dispatch, in-process MCP
  host, callback registries (Hooks/HooksBuilder + CanUseToolCallback)
  and Options/OptionsBuilder. Single responsibility: speak
  stream-json with the long-lived subprocess and dispatch its
  callbacks.
- **`forge-agent`** — drives one `forge-sdk` Client behind a
  channel-based `Agent`/`AgentHandle` API. Owns userdata (settings,
  trust, sessions catalog, memory, plugins), cloud (oauth/usage/
  account/service-status), env (git context), translate (event ↔
  message conversions), and tooling (tool-result helpers). The brain
  between SDK and workspace.
- **`forge-workspace`** — multi-session orchestrator. Owns
  `DomainSession` per active session (authoritative operational
  state: lifecycle, cwd, turn_state, account_info, runtime liveness,
  pending interactions). Drives per-session `SessionTask` actors that
  pump events from `AgentHandle::take_events()` and route Commands
  back. Single TUI-facing facade.
- **`forge-tui`** — native terminal interface. Pure view layer; no
  multi-session logic, no agent internals, no operational state.
  Holds per-session presentation buckets (`UiSession`: messages list,
  viewport, input editor, hover hints). Has no direct `forge-agent`
  dependency.
- **`forge-test-harness`** — wire-conformance harness. `sdk_wire` scope
  (forge-sdk ↔ claude CLI). Replay-based offline tests + opt-in live
  capture.

Multiple sessions = one `forge` process per tmux/zellij pane, with
multiple `Workspace`-managed sessions inside that process. No daemon,
no shared state across processes.

## Crate placement guide (where does my new code go?)

When adding a feature, work top-down through this decision tree.
First match wins.

1. **Is it a type that crosses a crate boundary?** (a message envelope,
   a snapshot struct, a hook payload, anything serialised over a
   channel or stored in a field that more than one crate touches)
   → `forge-primitives`. No logic, no I/O, no async — pure data
   shapes only. New trait-impls on these types go elsewhere; the type
   definition itself lives here.

2. **Does it speak stream-json to the `claude` CLI subprocess?**
   (decoder additions, new control_request subtype, transport-layer
   change, in-process MCP host, OptionsBuilder fields)
   → `forge-sdk`. Single responsibility. Pair with a wire-conformance
   scenario in `forge-test-harness`.

3. **Is it live state about the user's environment that the agent
   needs to know?** (git branch/diff watcher, cwd resolution, env
   probes, OAuth credentials, plugin manifest, settings IO, sessions
   catalog scan)
   → `forge-agent`. Specifically `forge-agent::env::*` for live
   environment state; `forge-agent::cloud::*` for Anthropic API /
   OAuth concerns; `forge-agent::userdata::*` for `~/.claude*` files
   (config, settings, plugins). Async, may shell out.

4. **Is it orchestration that knows about projects, sessions, accounts,
   the `forge.toml` schema, or the cross-session command bus?**
   → `forge-workspace`. The MVVM orchestrator. Adds methods to
   `Workspace`, dispatch variants to `Command`, presentation events
   to `SessionUpdate`. Re-exports forge-agent types as needed for the
   TUI's facade view (see "thin facade" note above).

5. **Is it a TUI widget, screen, key binding, mouse handler, or
   per-session presentation state?**
   → `forge-tui`. Consumes `forge-workspace` only — no direct
   `forge-agent` dependency. Render code in `ui/`, dispatch + state
   in `app/`. Per-session presentation lives on `UiSession`.

6. **Is it a wire-conformance scenario?**
   → `forge-test-harness`. Replay baselines under
   `baselines/sdk/<PINNED_CLI_VERSION>/`.

### Worked examples (recent + reference cases)

| Feature | Where it lives | Why |
|---|---|---|
| Git snapshot (branch + dirty check + file stats) | `forge-agent::env::git_diff` + `forge-workspace::scan_git_diff` wrapper + `forge-tui::app::git_diff` consumer + `forge-tui::ui::inspector_pane` GIT section | Subprocess + parsing = environment state → agent. Workspace mediates the async call. TUI owns the refresh cadence (1 s ticker, `snapshot.is_none() OR age ≥ 10 s → fetch`) and the render. Replaced the earlier `forge-agent::env::git` `notify::Watcher` + `forge-tui::app::git_context` cache — branch info folds into the same polled snapshot so a single subprocess invocation covers what the renderer needs. |
| Account LRU picker | `forge-agent::userdata::accounts` | Tracks per-account state from `~/.claude*` dirs. |
| `/resume` session list | `forge-agent::userdata::catalog::scan` for the disk read; `forge-tui::ui::session_picker` for the picker UI | Scan logic is environment-level; picker is pure UI. |
| TodoWrite chat rendering (suppressed) | `forge-tui::ui::tool_call::*` | UI decision about what to show; no logic change to the wire. |
| Mode / Effort / Model state | `forge-agent::state` for source-of-truth; `forge-tui::agent::model` for the observed-state cache | Agent owns the SDK-derived state; TUI mirrors for render. |
| New permission prompt variant | `forge-primitives::permission` for the shape; `forge-sdk::control` for the request; `forge-agent::cloud::auth_status` for the policy; `forge-tui::ui::tool_call::interactions` for the render | The classic 4-crate split when a single feature crosses every layer. |
| Inspector pane (TASKS section) | `forge-tui::ui::inspector_pane` | Pure UI; reads `app.todos()` (which lives on `UiSession`). |
| Bottom panel ETA right-justification | `forge-tui::ui::projects_pane` | Pure UI tweak. |
| `/usage` polling (the bug that created junk session files) | Should have been `forge-agent::cloud::usage` only (was `forge-agent::cloud::cli` which spawned `claude`); fixed by dropping the CLI fallback. | Cautionary tale: env probes that spawn `claude` itself create new sessions; design them to NOT pollute the user's session directory. |

### Anti-patterns (caught in review repeatedly)

- **Subprocess calls in `forge-tui`.** If you find yourself reaching for
  `tokio::process::Command` in a TUI module, stop. That belongs in
  `forge-agent::env::*` with a `forge-workspace::*` method exposing
  it. TUI calls the workspace method async (see
  `Workspace::oauth_usage` / `Workspace::scan_git_diff` for the
  precedent).
- **Adding a `SessionUpdate` variant for purely TUI-internal data.**
  The single-channel event bus is for cross-crate flow. If the
  producer and consumer are both inside forge-tui, use a separate
  mpsc channel (see `file_index_event_tx/rx` / `git_diff_event_tx/rx`
  for the pattern).
- **Cross-crate type duplication.** If forge-tui has a `Foo` and
  forge-agent has a `Foo` with the same fields, one of them is wrong
  and the type should be lifted to `forge-primitives` (or imported
  from the source crate via a forge-workspace re-export).
- **Workspace methods that bypass the Command bus for user actions.**
  User-initiated actions (Prompt, Cancel, SetMode, NewSession, etc.)
  go through `Workspace::dispatch(Command)`. Query-style refreshes
  (`refresh_status_snapshot`, `oauth_usage`, `scan_git_diff`) are
  direct inherent methods. Don't conflate the two.

### When the placement is genuinely ambiguous

Some features are split across multiple crates legitimately (the git
diff example above touches three). Use this rule of thumb:

- Logic / I/O / subprocess work → agent.
- Cross-crate envelope shape → primitives.
- Multi-session orchestration / cross-cutting state → workspace.
- Anything the user sees → TUI.

If unsure, ask the user. The default failure mode in this codebase
is "too much in forge-tui" — bias placement decisions toward the
deeper-down crate when in doubt.

## Communication contract (MVVM after #102)

After the MVVM refactor (PR #104) the TUI ↔ workspace contract is
**one channel pair** — single producer/consumer in each direction:

- **TUI → workspace:** `Workspace::dispatch(Command)`. One enum
  (`forge_workspace::protocol::Command`), one entry point. Every
  user-driven action (prompt, respond-to-permission, switch session,
  spawn project, etc.) is a `Command`.
- **workspace → TUI:** `SessionUpdate` via the channel returned by
  `Workspace::subscribe()`. TUI's `App.update_rx` consumes it. One
  enum (`forge_workspace::protocol::SessionUpdate`), one consumer.

This is the entire contract. No second channel for "control events"
vs "data events." No callback hooks. No shared mutable state. Just
two enum streams.

**Strict wiring (post Phase 6).** TUI no longer holds an
`Arc<AgentHandle>`. Every outbound agent call flows through
`Workspace::dispatch(Command)` — that's `Prompt`, `Cancel`,
`SetMode`/`SetModel`, `NewSession`/`ResumeSession`/`ResumeOrNew`,
`GenerateSessionTitle`/`RenameSession`, the full MCP suite
(`ReconnectMcpServer`, `ToggleMcpServer`, `AuthenticateMcpServer`,
`ClearMcpAuth`, `SetMcpServers`, `SubmitMcpOauthCallbackUrl`),
`RespondElicitation`, and the git-watch start/stop pair. Query-style
refreshes are direct `Workspace` methods rather than command
variants: `refresh_status_snapshot`, `refresh_oauth_credentials_snapshot`,
`refresh_context_usage`, `reload_plugins`, `refresh_mcp_snapshot`.
Direct-accessor facades (`settings_documents`, `write_settings_document`,
`project_memory_path`, `config_dir_for`, `oauth_usage`) also live as
inherent `Workspace` methods. `SessionUpdate::Connected` /
`SessionReplaced` / `AuthCompleted` no longer carry an
`Arc<AgentHandle>` payload — the handle is stamped onto the
workspace's `DomainSession`, never reaches TUI.

**Workspace-as-proxy realignment (final landing).** `DomainSession`
carries only workspace-internal routing metadata: `key`, `conn`,
`session_id` (mirror for `AgentHandle` dispatch), and
`pending_interactions` (oneshot mailbox for `Respond*` Commands).
All operational state TUI renders — `lifecycle_state`, `cwd_raw`,
`turn_state`, `session_scope_epoch`, `account_info`,
`active_account_display_name`, `runtime_session_state` — lives on
`UiSession`. TUI reducers update those fields directly from
`SessionUpdate` data; render reads `app.sessions[key].<field>`
straight (no workspace lookup, no per-frame lock). Workspace's
`apply_event_to_domain` only writes `session_id` — the field
workspace itself needs internally to call `AgentHandle` methods.

### Single-channel event bus (nuance worth knowing)

The same `SessionUpdate` channel TUI subscribes to is **also used as
an event bus for TUI-internal async work**. A few TUI-side modules
grab a sender via `Workspace::update_sender()` and emit their own
`SessionUpdate`s rather than dispatching a `Command` and waiting for
a round-trip:

- `forge-tui/src/app/plugins.rs` — local plugin install/uninstall
  side-effects.
- `forge-tui/src/app/slash/executors.rs` — `/help`, `/clear`, and
  other slash commands that run entirely TUI-side.
- `forge-tui/src/app/service_status_check.rs` — periodic Anthropic
  service-status polling.
- `forge-tui/src/app/input_submit.rs` — input-submit UI bookkeeping.

These don't violate the "workspace owns the domain" rule — they're
TUI-originated presentation events that reuse the existing channel
as a single event bus rather than spinning up a second one. The
alternative (separate TUI-internal channel) was rejected because it
adds plumbing without obvious benefit and forks the reducer
machinery.

Future-proofing watchlist: if the goal ever becomes "swap the TUI
for something else," the only contract a replacement should need to
honor is the two-enum-stream boundary. The leaky-emitter pattern
above is an *implicit* second contract that a replacement would
either have to replicate or have migrated away first. Tracking issue
at [busytools/forge#105](https://github.com/busytools/forge/issues/105) —
not high priority.

### `forge-workspace` is a thin facade, not strong isolation

The MVVM boundary between TUI and agent is enforced at the
**dependency graph** level (`forge-tui/Cargo.toml` has zero
`forge-agent` line), not at the Rust visibility level. Concretely:
`forge-workspace` does pass-through re-exports of forge-agent
submodules:

```rust
pub mod cloud      { pub use forge_agent::cloud::*; }
pub mod commands   { pub use forge_agent::commands::*; }
pub mod env::git_diff { pub use forge_agent::env::git_diff::*; }
pub mod session_lifecycle { pub use forge_agent::session_lifecycle::*; }
pub mod tooling    { pub use forge_agent::tooling::*; }
pub mod translate  { pub use forge_agent::translate::*; }
pub mod userdata   { pub use forge_agent::userdata::*; }
```

So `forge-tui` imports `forge_workspace::cloud::oauth::Token` (say),
but the type is *defined* in `forge_agent::cloud::oauth`. The
workspace exposes forge-agent's surface verbatim. The boundary is
"TUI can only reach forge-agent via the forge-workspace name" — not
"TUI sees a smaller, curated API."

This is the pragmatic shortcut. Tightening it (specific
`Workspace::method()` wrappers in place of each wildcard re-export)
is a "Phase 7 narrow agent surface" follow-up if the cost of the
current shape ever shows up.

## Project scope

**Personal use only.** Single user across multiple Macs (WireGuard
mesh between them). No public release planned. No multi-tenant
threat model. See `project_trust_model.md` in auto-memory before
running any audit or considering security hardening — findings whose
severity depends on adversarial assumptions get demoted or dropped.

## Vision: simple, efficient, capable — Rust-native

forge **is no longer a feature-parity port of Python's
`claude-agent-sdk`.** Both projects are reference implementations
that wrap the same `claude` CLI; they happen to share a wire
contract with that binary. forge gets to be its own thing — better,
simpler, and more efficient than the Python SDK where the language
permits.

Concretely:

- **Drop the public-API parity contract.** forge-sdk's API shape is
  whatever serves `forge-agent` (and through it, `forge-tui`) best.
  We don't carry single-task constraints from Python's async-generator
  pattern. We don't preserve method names that are awkward in Rust.
  We don't ship public types just because Python has them.
- **Lean into Rust.** Concurrent reads + writes + dispatch on the
  same Client (the actor pattern) is first-class, not an escape
  hatch. Channels-based public APIs are preferred over mutex-locked
  &mut self call sites. Internal mpsc-bridging is part of the SDK,
  not an agent-side workaround.
- **The `claude` CLI is still source of truth.** We spawn it as a
  subprocess and speak stream-json with it. We don't re-implement
  the agentic loop or hit the Anthropic API directly. That part of
  the parity story stays — it's how `claude` works.
- **Stream-json wire compatibility with the CLI is mandatory.** If
  forge-sdk's stdin/stdout to `claude` differs from what `claude`
  expects, that's a bug. The wire-conformance harness enforces this
  on every `cargo nextest run`.

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
    render. Scope is **current state only** — no future ideas, no
    aspirational sketches, no "v3+" sections. Anything new arrives
    in the same PR that lands the code.

    **The workflow when the user asks for a UI change** (start every
    such session with this — it's the recommended path):

    1. Read `docs/forge-map.html` first to confirm what's currently
       implemented and where it lives.
    2. Sketch the change in HTML — update the relevant section's
       mockup, prose, and any glyph/colour table entries.
    3. Apply the same change in the ratatui code.
    4. Verify the rendered HTML still matches the code (open the
       file in a browser, eyeball it).
    5. Push both files together — code + HTML in one PR.

    The HTML-first step matters because it forces a clear visual
    target before code edits begin, and it keeps the doc honest
    (the doc never describes something the code doesn't ship).
    When in doubt about whether the doc reflects reality, re-read
    the implementation and reconcile — never let prose drift from
    code.
12. **Gated actions still gated.** `cargo publish`, `git push
    --force` to `main`, `git tag` + push need explicit approval.
    Feature-branch pushes, PR creation, `gh pr comment`, non-force
    push to `main` for milestone landings, and `gh pr merge` are
    routine per project overrides.

## Weekly upstream-watch (NEW shape)

forge **does not feature-parity-track Python**. The weekly ritual is
now an *idea-scanning* exercise:

> **Every Monday** (or first working day of the week), proactively
> message the user:
>
> > "Upstream-watch Monday. Python `claude-agent-sdk` last reviewed at
> > <version>. Want me to scan for new features that might be worth
> > pulling in?"

The scan flow:

1. Diff Python `src/` and `tests/` against the previously-reviewed
   version (or against the recorded baseline in
   `~/.claude-subspace/plans/upstream-watch-<date>.md`).
2. For each new public API, hook event, control_request subtype, or
   stream-json variant: ask "does this make forge more capable for
   our use case?" If yes, propose a port — but **port it the
   forge-native way**, not by mirroring Python's API shape.
3. New stream-json shapes the CLI emits MUST be supported in the
   decoder (those are wire facts, not parity choices). Surface the
   addition to the user; commit the decoder + a wire-conformance
   scenario in the same week.
4. Old `PARITY.md` lineage is archived; the watch is now a forward-
   looking idea log.

There is no contract that says "every public Python type maps to a
Rust type". There is no test-mirroring requirement. Drop the
`crates/forge-sdk/tests/python_parity/` 1:1 mapping when it gets in
the way of a cleaner Rust API; keep the tests that genuinely cover
behaviour we care about.

## Style + Rust idiom

- **Workspace-level deps by default.** Pin once in
  `[workspace.dependencies]`, consume with `{ workspace = true }`.
- **Error types:** `thiserror` for library crates, `anyhow` for
  binaries / examples / tests. Never mix.
- **Tracing:** `tracing` crate for all structured logs. Never
  `println!` / `eprintln!` in library code (binaries can use
  `eprintln!` only when the tracing subsystem itself failed).
- **`#[non_exhaustive]`** on public struct + enum types expected to
  grow. Builder pattern with `#[must_use]` for configurable inputs.
- **Subprocess patterns:** `tokio::process::Command` for streaming
  I/O; `cmd_lib` for fire-and-forget shell.
- **Channels-based APIs over &mut self.** When a public type needs
  to be used across tasks (the daemon's typical pattern), prefer
  exposing channels / `&self` methods over `&mut self` methods that
  force a Mutex or actor wrapper at the call site. Internal bridging
  is the library's responsibility, not the consumer's.

## Quick-start for a new session

1. `git log main --oneline -10` — recent landings.
2. `just check` — full gate (fmt + clippy + nextest + docs).
3. Read the latest `handoff_*` in
   `~/.claude-granite/projects/-Users-vedhavyas-Projects-forge/memory/`
   for round-specific context.
4. Read `project_vision.md` in auto-memory for the
   simple/efficient/capable direction; read `project_trust_model.md`
   for personal-use threat-model context.

## Wire-conformance cheatsheet

- `crates/forge-test-harness/` holds the harness. Replay mode runs
  on every `cargo nextest run` — offline, no API cost.
- Live capture: `FORGE_WIRE_CAPTURE=1 cargo nextest run -p
  forge-test-harness --no-capture --run-ignored only sdk_<test>`.
- Baselines live under
  `crates/forge-test-harness/baselines/sdk/<PINNED_CLI_VERSION>/`.
- Adding a scenario: write a `tests/sdk_scenarios_<name>.rs`, run
  with the env var, `cp` the capture into the appropriate baselines
  dir, commit test + baseline together.

The `daemon_wire` scope was deleted in 2026-05-05 along with the
forge-daemon crate.

## Team context (for team-lead agents)

- Proactive memory lives at
  `~/.claude-*/projects/-Users-vedhavyas-Projects-forge/memory/`.
- Cross-project TIL goes to `~/.claude/memory/til/`.
- Team notes at `~/.claude/teams/forge/team-notes.md` if needed.
- Other team leads: aware, dotfiles, granite-backend, hub-modules,
  nf-core, subspace, trader-cc, architect. Dispatch via `ws teams
  ask <name> "APPROVED: ..."` when cross-project work is needed.

## Who to ask when in doubt

The user. Direct, concise; use `AskUserQuestion` for structured
decisions. Never silently work around ambiguities.
