//! Tracing target constants used by the agent's bridge code.
//! Mirrors the subset of `forge_tui::logging::targets` referenced
//! after the phase-3 lift; keeps strings consistent across crates so
//! the TUI's `tracing-subscriber` env-filter picks up agent-side
//! logs.

pub mod targets {
    pub const APP_INPUT: &str = "app.input";
    pub const APP_PERMISSION: &str = "app.permission";
    pub const BRIDGE_LIFECYCLE: &str = "bridge.lifecycle";
}
