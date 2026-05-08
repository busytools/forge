//! `forge-workspace` — multi-session orchestrator.
//!
//! Pools [`forge_agent::Agent`] instances behind a single
//! [`Workspace`] handle. See spec at
//! `~/.claude-subspace/plans/2026-05-09-forge-tui-phase-1a-workspace-design.md`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::unimplemented,
    clippy::unused_async
)]
#![allow(dead_code)]
// ↑ TEMPORARY: removed at the end of Task 5 once stub bodies are
//   replaced. Keeps this scaffolding commit lint-clean.

mod config;
mod error;
mod target;
mod views;
mod workspace;

pub use error::WorkspaceError;
pub use target::{ProjectKey, SessionKey, SessionTarget};
pub use views::{ProjectView, SessionView};
pub use workspace::Workspace;

// Re-export forge-agent types that public surface returns, so
// callers can write `use forge_workspace::AgentHandle` if they
// prefer.
pub use forge_agent::AgentHandle;
pub use forge_agent::client::SessionLaunchSettings;
