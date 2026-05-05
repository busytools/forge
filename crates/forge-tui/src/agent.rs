//! Agent-side modules that remain inside forge-tui after the
//! 2026-05-05 phase-3 lift. Anything that's UI-tied (uses
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

// Re-export the lifted bridge surface so existing call sites
// (`crate::agent::client::AgentBridge`, `crate::agent::ForgeSdkBridge`,
// etc.) keep resolving without crawling through every importer.
pub use forge_agent::{
    AgentBridge, AgentEvent, ForgeSdkBridge, PermissionMode, PromptResponse, SessionLaunchSettings,
};
pub mod client {
    //! Re-export shim — `crate::agent::client::*` paths now resolve
    //! into `forge_agent::client::*`.
    pub use forge_agent::client::*;
}
pub mod state {
    //! Re-export shim — `crate::agent::state::*` paths resolve
    //! into `forge_agent::state::*`.
    pub use forge_agent::state::*;
}
pub mod history {
    pub use forge_agent::history::*;
}
pub mod commands {
    pub use forge_agent::commands::*;
}
pub mod session_lifecycle {
    pub use forge_agent::session_lifecycle::*;
}
pub mod user_interaction {
    pub use forge_agent::user_interaction::*;
}
pub mod forge_sdk_bridge {
    pub use forge_agent::forge_sdk_bridge::*;
}
pub mod forge_sdk_worker {
    #[allow(unused_imports)]
    pub use forge_agent::forge_sdk_worker::*;
}
