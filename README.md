# forge

A Rust workspace for Claude-assisted tooling.

## Status

**Pre-v0.0.1.** Project skeleton only. All code is yet to be written; the design and implementation plans are complete and ready to execute.

The first deliverable is **`forge-sdk`** — a Rust port of Anthropic's [`claude-agent-sdk`](https://github.com/anthropics/claude-agent-sdk-python) at feature parity with Python v0.1.64+. Daemon (`forged`) and TUI client (`forge-tui`) follow in subsequent phases.

## Where the plans live

All design and planning documents are at `~/.claude-subspace/plans/` with the `2026-04-21-forge-sdk-*` prefix. They are **user-level** and not committed to this repo (per the project's convention against committing LLM-generated planning docs).

| Document | Purpose |
|---|---|
| `2026-04-21-forge-sdk-README.md` | **Start here.** Index + handoff guide. |
| `2026-04-21-forge-sdk-port-design.md` | Design spec covering all 7 milestones. |
| `2026-04-21-forge-sdk-m0-m1-plan.md` | Plan 1: scaffolding + core transport. |
| `2026-04-21-forge-sdk-m2-m3-plan.md` | Plan 2: permissions + in-process MCP. |
| `2026-04-21-forge-sdk-m4-m7-plan.md` | Plan 3: hooks, session features, recent additions, polish, publish. |

## How to work on this project

Open a Claude Code session in this directory. The session will load `CLAUDE.md` automatically, which gives you everything needed to pick up where the planning phase left off.

## Licence

MIT (will be added when the first crate lands — see Plan 1 Task 6).
