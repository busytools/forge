# forge — project guide

A Rust workspace hosting `forge-sdk` (a feature-parity Rust port of
Anthropic's [`claude-agent-sdk`](https://github.com/anthropics/claude-agent-sdk-python))
and — later — `forged` (daemon) and `forge-tui` (terminal client).

## Current state (2026-04-22)

- **`forge-sdk` at v0.1.64 parity-complete** with Python
  `claude-agent-sdk` v0.1.64. 764 tests + 107 ignored green; every
  in-scope Python test file (14/14) has a named Rust counterpart
  under `crates/forge-sdk/tests/python_parity/`. Only remaining
  parity gap is `AsyncHookJSONOutput` out-of-band delivery —
  upstream-blocked, tracked in the auto-memory weekly-watch entry.
- Full surface map: `docs/forge-sdk-parity-map.html` (interactive;
  `.git/info/exclude`-d).
- Full parity log + weekly runbook: `PARITY.md`.
- Release history: `docs/CHANGELOG.md`.

`forged` and `forge-tui` are downstream milestones that haven't
started.

## Non-negotiable invariants

1. **Feature-parity target is Python `claude-agent-sdk`.** Not a
   subset, not a re-imagining. Every public type and function in
   Python has a Rust counterpart.
2. **The `claude` binary is source of truth.** We spawn it as a
   subprocess and speak stream-json — same as Python does. We never
   re-implement the agentic loop or hit the Anthropic API directly.
3. **Stream-json wire compatibility is byte-identical with Python's
   SDK.** If stdin/stdout differ given the same inputs, that's a bug.
4. **TDD discipline.** Failing test → run it → watch it fail →
   implement → run it → watch it pass → commit. Do not write code
   before tests.
5. **Frequent commits.** One logical unit = one commit. Commits are
   small and reversible.
6. **No `mod.rs`.** Module files sit next to their directory
   (`foo.rs` + `foo/`).
7. **Nightly Rust, pinned.** `rust-toolchain.toml` locks a specific
   nightly date.
8. **Clippy pedantic + deny `unwrap_used` / `expect_used` / `panic`
   / `exit` / `todo` / `unimplemented`** in non-test code. CI
   enforces.
9. **`cargo nextest run`, not `cargo test`.** Locally use
   `just check` to run tests + clippy + fmt + docs in one shot.

## Weekly parity check — PROACTIVE OWNERSHIP

forge-sdk's purpose is feature parity. Anthropic ships the Python
SDK at ~3–4 releases/month; if forge-sdk falls behind, we've
re-created the exact problem (rusty community crates lagging
upstream) that forge-sdk exists to solve. The weekly check is the
non-negotiable forcing function.

### Cadence + proactive reminder

**Every Monday** (or first working day of the week), proactively
message the user:

> "It's parity-check Monday. Python `claude-agent-sdk` upstream last
> reviewed at <version>. Want me to run the check now?"

The user may defer, batch, or green-light it. But it is **the
agent's job to surface the prompt**, not the user's to remember.

### Runbook

Follow [`docs/parity-check.md`](docs/parity-check.md). State lives
in [`PARITY.md`](PARITY.md) — the weekly entry writes there.

### Outstanding watch items (see auto-memory)

- `AsyncHookJSONOutput` out-of-band hook-response delivery — the
  one open parity gap. Upstream hasn't shipped the follow-up frame
  spec. Weekly check must probe for it; if found, surface to the
  user and plan the port. Details in
  `~/.claude-*/projects/-Users-vedhavyas-Projects-forge/memory/project_asynchookjsonoutput_watch.md`.

### Test mirroring — the parity gate

Python's own `tests/` directory is forge-sdk's executable spec.
Every Python test has a named Rust counterpart in
`crates/forge-sdk/tests/python_parity/`. A weekly parity check
diffs `tests/` alongside source — every new/changed Python test
must translate to a Rust test in the same week.

## Hard rules

- **Never commit LLM-generated planning docs.** User-level plans
  stay at `~/.claude-subspace/plans/`. Reference by URL or relative
  comment, not a committed copy.
- **Gated actions still gated.** `gh pr merge`, `git push --force`
  to `main`, `git tag` + push, `cargo publish` — each needs
  explicit user approval per global CLAUDE.md. Feature-branch
  pushes, PR creation, `gh pr comment`, and non-force `git push`
  to `main` (for milestone landings) are routine per the project's
  `feedback_forge_git_override.md`.
- **One commit per logical unit.** Commit messages cite the round
  or unit (e.g. "feat(forge-sdk): query_stream() returns Stream…").
- **`docs/forge-sdk-parity-map.html` stays local.** Regenerate on
  surface changes; `.git/info/exclude`-d.

## Style + Rust idiom

- **Workspace-level deps by default.** Pin versions once in
  `[workspace.dependencies]`; per-crate manifests use
  `{ workspace = true }`.
- **Error types:** `thiserror` for library crates, `anyhow` for
  binaries / examples / tests. Never mix.
- **Tracing:** `tracing` crate for all structured logs. Never
  `println!` / `eprintln!` in library code.
- **`#[non_exhaustive]`** on public struct + enum types expected to
  grow. Builder pattern with `#[must_use]` for configurable inputs.
- **Subprocess patterns:** `tokio::process::Command` for streaming
  I/O; `cmd_lib` for fire-and-forget shell.
- **Don't reinvent upstream.** When porting, use idiomatic Rust
  patterns that preserve full behavioural parity — losing a feature
  to be "cleaner" is a regression.

## Team context (for team-lead agents)

If you're operating as the lead of a `forge` team:

- Proactive memory lives at `~/.claude-*/projects/-Users-vedhavyas-Projects-forge/memory/`.
- Cross-project TIL still goes to `~/.claude/memory/til/`.
- Team notes at `~/.claude/teams/forge/team-notes.md` (create if missing).
- Other team leads available: aware, dotfiles, granite-backend,
  hub-modules, nf-core, subspace, trader-cc, architect. Dispatch
  via `ws teams ask <name> "APPROVED: ..."` when cross-project work
  is needed.

## Who to ask when in doubt

The user. Direct, concise; use `AskUserQuestion` for structured
decisions. Never silently work around ambiguities — surface them,
record the resolution in the parity log.

## Quick-start for a new session

1. `git log main --oneline -10` — recent landings.
2. `just check` — full gate (nextest + clippy + fmt + docs).
3. `open docs/forge-sdk-parity-map.html` — surface + parity map.
4. `cat PARITY.md | head -50` — current parity state.
5. Read the latest handoff in the auto-memory directory for round-
   specific context (what landed, what's next).
