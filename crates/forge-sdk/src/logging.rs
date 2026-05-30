//! Tracing target constants for forge-sdk.
//!
//! Same shape as `forge_agent::logging::targets` and
//! `forge_tui::logging::targets`  -  keep target strings consistent
//! across crates so the TUI's `tracing-subscriber` env-filter picks
//! up SDK-side logs under coherent prefixes.

pub mod targets {
    /// The reader-task that decodes stream-json frames coming back
    /// from the `claude` subprocess.
    pub const SDK_READER: &str = "sdk.reader";
}
