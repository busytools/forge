# forge — project onboarding

You're looking at a Rust workspace that is **pre-v0.0.1** — the design and implementation plans are complete, the code is not yet written. Your job, if you're the agent picking this up, is to execute the plans task-by-task, in order.

## What forge is

A Rust workspace for Claude-assisted tooling. The first deliverable is **`forge-sdk`** — a Rust port of Anthropic's [`claude-agent-sdk`](https://github.com/anthropics/claude-agent-sdk-python) at feature parity with Python v0.1.64+. Daemon (`forged`) and TUI client (`forge-tui`) follow in later phases.

This project was planned during an architect team session on 2026-04-21. The user (Vedhavyas Singareddi) is Rust-fluent, runs multiple Claude Code sessions concurrently, prefers precision over speed, and communicates mostly via SuperWhisper — interpret transcription artefacts charitably.

## What has been done

- Project directory created (`~/Projects/forge/`).
- Git repository initialised (`main` branch).
- Top-level `README.md`, `CLAUDE.md` (this file), `.gitignore` seeded.
- All design + planning artefacts written and saved at user level.

That's it. No Rust crates yet. No CI. No LICENSE. All of those are **Plan 1** tasks.

## Where to find the plans

**All plans live at `~/.claude-subspace/plans/`** (user-level, deliberately outside this repo). Read in this order:

1. **`~/.claude-subspace/plans/2026-04-21-forge-sdk-README.md`** — index + handoff guide. Workflow, invariants, parity-check ritual, environment assumptions. Read first.
2. **`~/.claude-subspace/plans/2026-04-21-forge-sdk-port-design.md`** — design spec covering all 7 milestones. Scope, decisions, non-goals, success criteria. Read second.
3. **`~/.claude-subspace/plans/2026-04-21-forge-sdk-m0-m1-plan.md`** — Plan 1. 20 tasks. M0 (scaffolding) + M1 (core transport). **Start executing here.**
4. **`~/.claude-subspace/plans/2026-04-21-forge-sdk-m2-m3-plan.md`** — Plan 2. 30 tasks. M2 (permissions) + M3 (in-process MCP). Execute after Plan 1 merges.
5. **`~/.claude-subspace/plans/2026-04-21-forge-sdk-m4-m7-plan.md`** — Plan 3. 40 tasks. M4 (hooks) + M5 (session features) + M6 (recent additions) + M7 (polish + publish). Execute after Plan 2 merges.

## Starting point for the very next agent

- **Plan 1 Task 1 Step 1 says:** `mkdir -p ~/Projects/forge && cd ~/Projects/forge && git init -b main`. This is **already done**. Skip Step 1. Proceed with Step 2 (write `.gitignore` — also already done, verify contents match Plan 1's gitignore snippet) and Step 4 (the commit). If the `.gitignore` matches, stage the current skeleton files and commit with Plan 1's message: `git add .gitignore CLAUDE.md README.md && git commit -m "chore: initialize repository"`.
- **Plan 1 Task 2 onwards** runs as written.

## Non-negotiable invariants

From `2026-04-21-forge-sdk-README.md`:

1. **Feature-parity target is Python `claude-agent-sdk`.** Not a subset, not a re-imagining. Every public type and function in Python has a Rust counterpart.
2. **The `claude` binary is source of truth.** We spawn it as a subprocess and speak stream-json — same as Python does. We never re-implement the agentic loop or hit the Anthropic API directly.
3. **Stream-json wire compatibility is byte-identical with Python's SDK.** If our stdin/stdout differs given the same inputs, that's a bug.
4. **No placeholders in plans.** Every step has complete code, exact commands, expected output. If you find a `TBD` or `# similar to above` while executing, that's a plan failure — fix the plan (by adding the missing content) before coding.
5. **TDD discipline.** Every task pattern: failing test → run it → watch it fail → implement → run it → watch it pass → commit. Do not write code before tests.
6. **Frequent commits.** One task = one commit. Commits are small and reversible. Commit messages come from the plan — don't paraphrase.
7. **No `mod.rs`.** Module files sit next to their directory (`foo.rs` + `foo/`).
8. **Nightly Rust, pinned.** `rust-toolchain.toml` locks a specific nightly date.
9. **Clippy pedantic, deny `unwrap_used`, `expect_used`, `panic`, `exit`, `todo`, `unimplemented`** in non-test code. CI enforces.
10. **`cargo nextest run`, not `cargo test`.**

## Execution workflow

For each plan file:

1. Open the plan — read header, architecture, file structure, out-of-scope sections before any task.
2. Execute tasks in order. Each task's steps are atomic (2–5 min each). Tick checkboxes as you go.
3. Commit per task using the plan's supplied commit message.
4. Between tasks, run `cargo nextest run && cargo clippy --all-targets -- -D warnings && cargo fmt --check`. Everything must be green.
5. If a test doesn't fail when expected (red step) or doesn't pass when expected (green step) — **stop.** Diagnose: plan wrong, understanding wrong, or environment wrong. Fix the first; re-read the second; investigate the third.
6. When a plan is fully executed — all tasks ticked, CI green, tag pushed per the final task — move to the next plan.
7. Record deviations in `~/.claude-subspace/plans/2026-04-21-forge-sdk-execution-log.md` (create the file the first time you need it).

## When to consult external references

- **Python SDK** (`~/.venv/lib/python3.14/site-packages/claude_agent_sdk/` via architect's venv, or clone `anthropics/claude-agent-sdk-python` to `/tmp/`) — authoritative spec. Consult frequently.
- **tyrchen/claude-agent-sdk-rs** (MIT, pure Rust) — cross-check reference for idiomatic translation patterns only. It's 2+ months behind upstream; do NOT treat as ground truth for recent features.
- **MCP spec** — <https://spec.modelcontextprotocol.io/> — for Task 16+ of Plan 2.

## Weekly parity check (applies after Plan 1 merges)

Every Monday, run through `docs/parity-check.md` (landed in Plan 3 Task 25). In short: diff upstream releases since last tracked, classify changes, open issues, port in the same week. This ritual is how forge-sdk stays at parity with Python — the whole point of owning this layer.

## Hard rules

- This is a **greenfield user project** — no other projectRoot currently depends on it. You are the primary owner for now.
- **Never commit LLM-generated planning docs to this repo.** The plans at `~/.claude-subspace/plans/` stay user-level. If you need to reference them from code, use a URL or a relative comment, not a committed copy.
- **Gated actions still gated:** `gh pr merge`, `git push` to `main`, `git tag` + push, `cargo publish`. Each requires explicit user approval per the global CLAUDE.md. Feature-branch pushes are routine.
- **Every PR creates a new commit per task** — never squash before review unless explicitly requested. Task-level commits are the review surface.

## Team context (for team-lead agents)

If you're operating as the lead of a `forge` team:

- Proactive memory lives at `~/.claude-*/projects/-Users-vedhavyas-Projects-forge/memory/` (create on first write).
- Cross-project TIL still goes to `~/.claude/memory/til/`.
- Team notes go to `~/.claude/teams/forge/team-notes.md` (create if missing).
- Other team leads available: aware, dotfiles, granite-backend, hub-modules, nf-core, subspace, trader-cc, architect. Dispatch via `ws teams ask <name> "APPROVED: ..."` when you need cross-project work.

## Who to ask when in doubt

The user. Direct communication, concise, via `AskUserQuestion` for structured decisions. Do not silently work around ambiguities — surface them, record the resolution in the execution log.

Now: open `~/.claude-subspace/plans/2026-04-21-forge-sdk-README.md` and `~/.claude-subspace/plans/2026-04-21-forge-sdk-m0-m1-plan.md` and start.
