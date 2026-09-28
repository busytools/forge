//! `plugins()`: the plugin inventory and update records.

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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use forge_primitives::plugins::{PluginUpdateRecord, PluginUpdateTrigger};

    use super::*;

    /// Catches the verb being pointed at a different store row than the
    /// call it replaces, which would render stale or empty update
    /// records in the Extensions pane.
    #[test]
    fn plugins_agrees_with_the_call_it_replaces() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let record = PluginUpdateRecord {
            plugin_id: "night-market".to_owned(),
            marketplace: "abstract".to_owned(),
            scope: "user".to_owned(),
            cwd_raw: String::new(),
            from_version: Some("1.0.0".to_owned()),
            to_version: Some("2.0.0".to_owned()),
            marketplace_ref_before: Some("abc123".to_owned()),
            updated_at: "2026-09-25T00:00:00Z".to_owned(),
            trigger: PluginUpdateTrigger::Manual,
        };
        workspace.record_plugin_updates(std::slice::from_ref(&record));

        let view = ViewSurface::new(Arc::clone(&workspace)).plugins();

        assert_eq!(
            view.update_records,
            workspace.plugin_update_records(),
            "the records are the call's own",
        );
        assert_eq!(view.update_records, vec![record], "the seeded record is what the read returns");
    }
}
