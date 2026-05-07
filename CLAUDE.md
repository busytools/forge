# forge — project guide

A Rust workspace for personal-use agentic tooling around Anthropic's
`claude` CLI. Five components, layered acyclically:

```
forge-primitives ──── leaf (pure data, no logic)
forge-sdk        ──→ primitives
forge-agent      ──→ primitives + sdk
forge-tui        ──→ primitives + agent
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
  account/service-status), and env (git context). The brain between
  SDK and TUI.
- **`forge-tui`** — native terminal interface. Consumes `AgentEvent`s,
  emits `Command`s. No direct dep on `forge-sdk` — primitives + agent
  cover everything the UI needs.
- **`forge-test-harness`** — wire-conformance harness. `sdk_wire` scope
  (forge-sdk ↔ claude CLI). Replay-based offline tests + opt-in live
  capture.

Multiple sessions = multiple `forge` processes (one per tmux/zellij
pane). No daemon, no shared state.

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
