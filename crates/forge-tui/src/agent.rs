//! Agent-side modules that remain inside forge-tui after the
//! 2026-05-05 restructure. Anything that's UI-tied (uses
//! `crate::app::*`, `crate::error::AppError`, ratatui types, etc.)
//! lives here. Pure SDK-driver code lives in the sibling
//! `forge_agent` crate.

pub mod agents;
pub mod error_handling;
pub mod events;
pub mod model;
pub mod state_parsing;
pub mod types;

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
