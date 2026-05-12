//! TUI-side boundary to the agent layer.
//!
//! Two distinct things share the name "agent" in this codebase:
//!
//! 1. The **`forge-agent` crate** — drives the SDK Client, runs
//!    callbacks, owns userdata/cloud/env. Lives in `crates/forge-agent`.
//! 2. **This module** (`forge_tui::agent`) — the TUI-side mapping
//!    onto the agent layer. Holds:
//!    - [`events`] — `ClientEvent` enum: the UI's translated view of
//!      `forge_agent::AgentEvent` (with extra UI-side variants like
//!      `AuthCompleted`, `FatalError`).
//!    - [`model`] — UI-typed model describing agent state for the
//!      view layer to render.
//!    - Re-export shims (`agents`, `error_handling`, `state_parsing`,
//!      `tooling`, `client`, `state`, `commands`, `session_lifecycle`)
//!      that keep pre-restructure import paths
//!      (`crate::agent::error_handling::*` etc.) resolving without a
//!      mass rewrite — each shim points at the matching workspace
//!      re-export.
//!
//! Anything that's UI-tied (uses `crate::app::*`, `crate::error::AppError`,
//! ratatui types, etc.) lives in `events` / `model`. Everything else
//! is a passthrough to `forge_workspace::*` (which itself re-exports
//! from `forge_agent::*`).
//!
//! Phase 5 of the MVVM refactor (#102) flipped these shims to source
//! from `forge_workspace` so forge-tui no longer needs `forge-agent`
//! as a direct Cargo dep.

pub mod events;
pub mod model;

pub mod agents {
    pub use forge_workspace::translate::agents::*;
}
pub mod error_handling {
    pub use forge_workspace::translate::error_handling::*;
}
pub mod state_parsing {
    pub use forge_workspace::translate::state_parsing::*;
}

pub mod tooling {
    pub use forge_workspace::tooling::*;
}

pub use forge_workspace::{PermissionMode, SessionLaunchSettings};
// `AgentEvent` is only needed by the smoke-test suite at
// `crates/forge-tui/tests/forge_sdk_smoke.rs`; gate behind `testing`
// so production builds don't carry a public re-export of an internal
// wire type. forge-tui's production code consumes `SessionUpdate`s,
// never raw `AgentEvent`s.
#[cfg(feature = "testing")]
pub use forge_workspace::AgentEvent;

pub mod client {
    //! Re-export shim — `crate::agent::client::*` paths resolve into
    //! `forge_workspace::*` (which forwards to `forge_agent::client::*`).
    #[cfg(feature = "testing")]
    pub use forge_workspace::AgentEvent;
    pub use forge_workspace::SessionLaunchSettings;
}
pub mod state {
    //! Re-export shim — `crate::agent::state::*` paths resolve into
    //! `forge_workspace::*` (which forwards to `forge_agent::state::*`,
    //! itself a re-export of `forge_primitives::permission::PermissionMode`).
    pub use forge_workspace::PermissionMode;
}
pub mod commands {
    pub use forge_workspace::commands::*;
}
pub mod session_lifecycle {
    pub use forge_workspace::session_lifecycle::*;
}
