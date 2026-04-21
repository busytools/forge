# forge — project onboarding

You're looking at a Rust workspace that is **pre-v0.0.1** — the design and implementation plans are complete, the code is not yet written. Your job, if you're the agent picking this up, is to execute the plans task-by-task, in order.

## What forge is

A Rust workspace for Claude-assisted tooling. The first deliverable is **`forge-sdk`** — a Rust port of Anthropic's [`claude-agent-sdk`](https://github.com/anthropics/claude-agent-sdk-python) at feature parity with Python v0.1.64+. Daemon (`forged`) and TUI client (`forge-tui`) follow in later phases.

This project was planned during an architect team session on 2026-04-21. The user (Vedhavyas Singareddi) is Rust-fluent, runs multiple Claude Code sessions concurrently, prefers precision over speed, and communicates mostly via speech-to-text — interpret transcription artefacts charitably.

## What has been done

- Project directory created (`~/Projects/forge/`).
- Git repository initialised (`main` branch).
- Top-level `README.md`, `CLAUDE.md` (this file), `.gitignore`, `PARITY.md` seeded.
- All design + planning artefacts written and saved at user level, **including a corrections document that MUST be read before executing Plans 2 or 3** — two parallel reviews (protocol verification + code-review consistency) surfaced structural issues in the plans that need correction during execution.

That's it. No Rust crates yet. No CI. No LICENSE. All of those are **Plan 1** tasks.

## CRITICAL pre-read — corrections document

Before touching Plan 2 or Plan 3, read `~/.claude-stargate/plans/2026-04-21-forge-sdk-corrections.md`. That document captures the review findings: wrong wire shapes, an entirely-wrong MCP architecture, compile blockers, version-bump gaps, and a handful of dependency-consistency issues. The plans as written contain real errors — not executing without the corrections sheet.

Plan 1 is largely clean — one small correction (adding `"fs"` to tokio features). Plan 2 has structural fixes (MCP rewrite, permission wire shape). Plan 3 has structural fixes (hook dispatch mechanism, SessionStore protocol, CLI flag mechanics).

## Where to find the plans

**All plans live at `~/.claude-stargate/plans/`** (user-level, deliberately outside this repo). Read in this order:

1. **`~/.claude-stargate/plans/2026-04-21-forge-sdk-README.md`** — index + handoff guide. Workflow, invariants, parity-check ritual, environment assumptions. Read first.
2. **`~/.claude-stargate/plans/2026-04-21-forge-sdk-port-design.md`** — design spec covering all 7 milestones. Scope, decisions, non-goals, success criteria. Read second.
3. **`~/.claude-stargate/plans/2026-04-21-forge-sdk-m0-m1-plan.md`** — Plan 1. 20 tasks. M0 (scaffolding) + M1 (core transport). **Start executing here.**
4. **`~/.claude-stargate/plans/2026-04-21-forge-sdk-m2-m3-plan.md`** — Plan 2. 30 tasks. M2 (permissions) + M3 (in-process MCP). Execute after Plan 1 merges.
5. **`~/.claude-stargate/plans/2026-04-21-forge-sdk-m4-m7-plan.md`** — Plan 3. 40 tasks. M4 (hooks) + M5 (session features) + M6 (recent additions) + M7 (polish + publish). Execute after Plan 2 merges.
6. **`~/.claude-stargate/plans/2026-04-21-forge-sdk-corrections.md`** — **MANDATORY pre-read** before Plans 2 / 3. Consolidated corrections from two reviews. Search this file for every task number before executing it.

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
7. Record deviations in `~/.claude-stargate/plans/2026-04-21-forge-sdk-execution-log.md` (create the file the first time you need it).

## When to consult external references

- **Python SDK** (`~/.venv/lib/python3.14/site-packages/claude_agent_sdk/` via architect's venv, or clone `anthropics/claude-agent-sdk-python` to `/tmp/`) — authoritative spec. Consult frequently.
- **tyrchen/claude-agent-sdk-rs** (MIT, pure Rust) — cross-check reference for idiomatic translation patterns only. It's 2+ months behind upstream; do NOT treat as ground truth for recent features.
- **MCP spec** — <https://spec.modelcontextprotocol.io/> — for Task 16+ of Plan 2.

## Weekly parity check — PROACTIVE OWNERSHIP

`forge-sdk`'s purpose is feature parity with Python `claude-agent-sdk`. Anthropic ships the Python SDK at a ~3–4/month cadence; if forge-sdk falls behind, we've re-created the exact problem (rusty community crates lagging upstream) that forge-sdk exists to solve. The weekly check is the non-negotiable forcing function.

### State tracking

**Read `PARITY.md` at the root of this repo first.** It's the single source of truth for:
- The Python SDK version forge-sdk is currently at parity with (last fully-ported upstream commit + release).
- The specifications that parity is based on (the user-level plans at `~/.claude-stargate/plans/2026-04-21-forge-sdk-*.md`).
- Parity-run log: one entry per weekly check, most recent on top.

Every parity check writes a new entry to `PARITY.md` and updates its "current state" table.

### Proactive reminder — your job as forge lead

**Every Monday (or first working day of the week), you proactively message the user with:**

> "It's parity-check Monday. Python `claude-agent-sdk` upstream last reviewed at <version>. Want me to run the check now?"

The user may defer, batch, or green-light it. But it is **your job to surface the prompt**, not theirs to remember. Anthropic is very active; missing a week compounds.

When in an architect- or `ws teams ask forge` dispatch, the same applies — if a week has passed since the last `PARITY.md` entry, surface the reminder before taking any other action for that session.

### When authorised, run the check

Follow `docs/parity-check.md` (this file is created in Plan 3 Task 25 — until then, fall back to the abbreviated runbook in `PARITY.md`'s "How to run a parity check" section).

### Test mirroring — the parity gate

`PARITY.md` documents a key strategy: **the Python SDK's own `tests/` directory is our executable spec for behavioural parity.** We maintain a `crates/forge-sdk/tests/python_parity/` subdirectory that mirrors Python's tests into Rust. A weekly parity check includes diffing `tests/` as well as source — every new/changed Python test should translate to a corresponding Rust test in the same week.

Read `PARITY.md`'s "Test-mirroring strategy" section before running any parity-check or landing any new behavioural feature. The mirrored tests are additive — they don't replace the Rust-specific tests from Plans 1–3; they complement them.

## Hard rules

- This is a **greenfield user project** — no other projectRoot currently depends on it. You are the primary owner for now.
- **Never commit LLM-generated planning docs to this repo.** The plans at `~/.claude-stargate/plans/` stay user-level. If you need to reference them from code, use a URL or a relative comment, not a committed copy.
- **Gated actions still gated:** `gh pr merge`, `git push` to `main`, `git tag` + push, `cargo publish`. Each requires explicit user approval per the global CLAUDE.md. Feature-branch pushes are routine.
- **Every PR creates a new commit per task** — never squash before review unless explicitly requested. Task-level commits are the review surface.

## Team context (for team-lead agents)

If you're operating as the lead of a `forge` team:

- Proactive memory lives at `~/.claude-*/projects/-Users-dev-Projects-forge/memory/` (create on first write).
- Cross-project TIL still goes to `~/.claude/memory/til/`.
- Team notes go to `~/.claude/teams/forge/team-notes.md` (create if missing).
- Other team leads available: aware, dotfiles, gateway-backend, data-modules, nf-core, stargate, web-api, architect. Dispatch via `ws teams ask <name> "APPROVED: ..."` when you need cross-project work.

## Who to ask when in doubt

The user. Direct communication, concise, via `AskUserQuestion` for structured decisions. Do not silently work around ambiguities — surface them, record the resolution in the execution log.

Now: open `~/.claude-stargate/plans/2026-04-21-forge-sdk-README.md` and `~/.claude-stargate/plans/2026-04-21-forge-sdk-m0-m1-plan.md` and start.
