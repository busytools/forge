//! TUI-side boundary to the `forge-agent` crate.
//!
//! Two distinct things share the name "agent" in this codebase:
//!
//! 1. The **`forge-agent` crate** — drives the SDK Client, runs
//!    callbacks, owns userdata/cloud/env. Lives in `crates/forge-agent`.
//! 2. **This module** (`forge_tui::agent`) — the TUI-side mapping
//!    onto the agent crate's surface. Holds:
//!    - [`events`] — `ClientEvent` enum: the UI's translated view of
//!      `forge_agent::AgentEvent` (with extra UI-side variants like
//!      `AuthCompleted`, `FatalError`).
//!    - [`model`] — UI-typed model describing agent state for the
//!      view layer to render.
//!    - Re-export shims (`agents`, `error_handling`, `state_parsing`,
//!      `tooling`, `client`, `state`, `commands`, `session_lifecycle`)
//!      that keep pre-restructure import paths
//!      (`crate::agent::error_handling::*` etc.) resolving without a
//!      mass rewrite — each shim points at the matching
//!      `forge_agent::*` module.
//!
//! Anything that's UI-tied (uses `crate::app::*`, `crate::error::AppError`,
//! ratatui types, etc.) lives in `events` / `model`. Everything else
//! is a passthrough to the agent crate.

pub mod events;
pub mod model;

// Translators that lifted to forge-agent::translate. Re-export shims
// keep `crate::agent::error_handling::*` paths resolving across
// existing call sites.
pub mod agents {
    #[allow(unused_imports)]
    pub use forge_agent::translate::agents::*;
}
pub mod error_handling {
    #[allow(unused_imports)]
    pub use forge_agent::translate::error_handling::*;
}
pub mod state_parsing {
    #[allow(unused_imports)]
    pub use forge_agent::translate::state_parsing::*;
}

pub mod tooling {
    pub use forge_agent::tooling::*;
}

// Re-export the bits forge-tui actually consumes from forge-agent so
// existing import paths (`crate::agent::AgentEvent`, etc.) keep
// resolving without crawling through every importer.
pub use forge_agent::{AgentEvent, PermissionMode, PromptResponse, SessionLaunchSettings};

pub mod client {
    //! Re-export shim — `crate::agent::client::*` paths resolve
    //! into `forge_agent::client::*`.
    #[allow(unused_imports)]
    pub use forge_agent::client::*;
}
pub mod state {
    //! Re-export shim — `crate::agent::state::*` paths resolve
    //! into `forge_agent::state::*`.
    #[allow(unused_imports)]
    pub use forge_agent::state::*;
}
pub mod commands {
    #[allow(unused_imports)]
    pub use forge_agent::commands::*;
}
pub mod session_lifecycle {
    #[allow(unused_imports)]
    pub use forge_agent::session_lifecycle::*;
}
