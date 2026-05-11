//! `forge-workspace` — multi-session orchestrator.
//!
//! Pools [`forge_agent::Agent`] instances behind a single
//! [`Workspace`] handle. See spec at
//! `~/.claude-subspace/plans/2026-05-09-forge-tui-phase-1a-workspace-design.md`.

mod account;
mod config;
mod domain_session;
mod error;
pub mod protocol;
mod session_task;
mod spawn;
mod state;
mod target;
mod views;
mod workspace;

pub use domain_session::DomainSession;
pub use error::WorkspaceError;
pub use protocol::{
    Command, DispatchError, PendingInteractionSlot, SessionUpdate, TurnErrorClass,
    TurnFinalizeStatus,
};
pub use target::{ProjectKey, SessionKey, SessionTarget};
pub use views::{ProjectView, SessionView};
pub use workspace::Workspace;

// Re-export forge-agent types that public surface returns, so
// callers can write `use forge_workspace::AgentHandle` if they
// prefer.
pub use forge_agent::AgentHandle;
pub use forge_agent::client::SessionLaunchSettings;
