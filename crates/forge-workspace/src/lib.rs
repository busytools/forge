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

// Re-export forge-agent sub-surfaces consumed by `forge-tui` so the
// TUI crate doesn't need a direct `forge-agent` dep. Each entry below
// is the workspace-side facade for a forge-agent module forge-tui
// reads from. Most types here are themselves re-exports from
// `forge_primitives`; the modules also surface a handful of helper
// functions / network fetchers that genuinely live in `forge-agent`.
//
// Production code in forge-tui consumes data via `SessionUpdate`
// events; these re-exports back the small set of helpers + types the
// TUI still calls directly (translate::*, tooling::*, commands::*,
// session_lifecycle::*, cloud::*, env::git::*, userdata::*).
pub mod cloud {
    pub use forge_agent::cloud::*;
}
pub mod commands {
    pub use forge_agent::commands::*;
}
pub mod env {
    pub mod git {
        pub use forge_agent::env::git::*;
    }
}
pub mod session_lifecycle {
    pub use forge_agent::session_lifecycle::*;
}
pub mod tooling {
    pub use forge_agent::tooling::*;
}
pub mod translate {
    pub use forge_agent::translate::*;
}
pub mod userdata {
    pub use forge_agent::userdata::*;
}
pub use forge_agent::state::PermissionMode;

// Test-only re-exports. The smoke-test suite at
// `crates/forge-tui/tests/forge_sdk_smoke.rs` needs `Agent::spawn`
// and the `AgentEvent` enum to drive a real `claude` subprocess
// end-to-end; production code uses `Workspace`'s facade and consumes
// `SessionUpdate`s, never these raw types. Gating these symbols
// behind the `testing` feature keeps the production build's surface
// minimal — `cargo check --no-default-features -p forge-workspace`
// won't carry `forge_workspace::Agent` or `forge_workspace::AgentEvent`.
#[cfg(feature = "testing")]
pub use forge_agent::Agent;
#[cfg(feature = "testing")]
pub use forge_agent::AgentEvent;
