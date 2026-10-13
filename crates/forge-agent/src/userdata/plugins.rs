//! Plugin registry types + claude-CLI wrappers.
//!
//! Wire-shape data (InstalledPluginEntry, MarketplaceEntry, etc.)
//! lives in `forge_primitives::plugins` so a view's plugin UI
//! doesn't need to reach into claude CLI invocation code. Renderable
//! plugin state lives with the view.

pub mod cli;
pub mod components;

pub use forge_primitives::plugins::{
    InstalledPluginEntry, MarketplaceEntry, MarketplaceSourceEntry, PluginCapability,
    PluginRunRowStatus, PluginUpdateAvailability, PluginUpdateRecord, PluginUpdateRun,
    PluginUpdateRunRow, PluginUpdateTrigger, PluginsInventorySnapshot, classify_update_row,
    update_availability,
};
