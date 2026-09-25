//! The plugin inventory as a view reads it.

use forge_primitives::plugins::PluginUpdateRecord;

use super::ViewSurface;

/// The plugin inventory and its update records.
pub struct PluginsView {
    /// Every remembered plugin update, latest write per installed entry.
    pub update_records: Vec<PluginUpdateRecord>,
}

impl ViewSurface {
    pub fn plugins(&self) -> PluginsView {
        PluginsView { update_records: self.workspace.plugin_update_records() }
    }
}
