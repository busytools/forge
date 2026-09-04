//! Plugin registry types + claude-CLI wrappers.
//!
//! Wire-shape data (InstalledPluginEntry, MarketplaceEntry, etc.)
//! lives in `forge_primitives::plugins` so forge-tui's plugin UI
//! doesn't need to reach into claude CLI invocation code. Renderable
//! PluginsState stays in forge-tui::app::plugins.

pub mod cli;

pub use forge_primitives::plugins::{
    InstalledPluginEntry, MarketplaceEntry, MarketplaceSourceEntry, PluginCapability,
    PluginUpdateAvailability, PluginUpdateOutcome, PluginUpdateOutcomeStatus, PluginUpdateRecord,
    PluginUpdateTarget, PluginUpdateTrigger, PluginsInventorySnapshot, update_availability,
};
