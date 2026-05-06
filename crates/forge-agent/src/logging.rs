//! Tracing target constants used by the agent's bridge code. Strings
//! stay consistent with `forge_tui::logging::targets` so the TUI's
//! `tracing-subscriber` env-filter picks up agent-side logs.

pub mod targets {
    pub const APP_INPUT: &str = "app.input";
    pub const APP_PERMISSION: &str = "app.permission";
    pub const BRIDGE_LIFECYCLE: &str = "bridge.lifecycle";
    pub const OAUTH_CREDENTIALS: &str = "agent.oauth_credentials";
    pub const SERVICE_STATUS: &str = "agent.service_status";
    pub const CATALOG_SCAN: &str = "agent.catalog_scan";
    pub const TOOLING: &str = "agent.tooling";
    pub const ENV_GIT: &str = "agent.env_git";
    pub const SETTINGS: &str = "agent.settings";
}
