//! `forge-workspace` — multi-session orchestrator.
//!
//! Pools [`forge_agent::Agent`] instances behind a single
//! [`Workspace`] handle. The workspace loads
//! `<config_dir>/forge.toml` at construction and resolves session
//! targets against it.
//!
//! See `~/.claude-subspace/plans/2026-05-09-forge-tui-phase-1a-workspace-design.md`
//! for the full design.

// Module skeletons land in Task 2. lib.rs stays empty until then so
// the scaffolding commit compiles cleanly.
