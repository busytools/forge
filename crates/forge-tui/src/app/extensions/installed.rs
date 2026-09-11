//! Component-tab actions: Enter on a component row opens its owning
//! plugin's action overlay, an available row offers install, and an
//! uninstall confirms on the whole bundle - the CLI cannot remove one
//! component alone, so the confirm names the plugin and its component
//! count.

use crate::app::App;
use crate::app::config::{
    ConfigOverlayState, InstalledPluginActionKind, InstalledPluginActionOverlayState,
    UninstallConfirmState,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use forge_primitives::plugins::RowState;

use super::selected_extension_row;

/// Enter on a component tab: the row's action overlay. An installed
/// row opens its plugin's actions; an available row offers install of
/// the source plugin.
pub(crate) fn open_component_actions_overlay(app: &mut App) -> bool {
    let Some(row) = selected_extension_row(app).cloned() else {
        return false;
    };
    if row.state == RowState::AvailableNotInstalled {
        return super::open_plugin_install_overlay(app, &row.source);
    }
    let plugin_id = row.source;
    let Some(entry) = app.plugins.installed.iter().find(|entry| entry.id == plugin_id).cloned()
    else {
        return false;
    };
    super::open_installed_actions_for(app, entry)
}

/// The uninstall confirm: names the plugin and what it will remove.
pub(crate) fn open_uninstall_confirm(app: &mut App, overlay: &InstalledPluginActionOverlayState) {
    use forge_primitives::plugins::ExtensionKind;
    let count = |kind: ExtensionKind| {
        app.plugins
            .rows
            .iter()
            .filter(|row| row.kind == kind && row.source == overlay.plugin_id)
            .count()
    };
    let skills = count(ExtensionKind::Skill);
    let agents = count(ExtensionKind::Agent);
    let commands = count(ExtensionKind::Command);
    let hooks = count(ExtensionKind::Hook);
    let lsp = count(ExtensionKind::Lsp);
    let description = {
        let mut parts = Vec::new();
        if skills > 0 {
            parts.push(format!("{skills} skills"));
        }
        if agents > 0 {
            parts.push(format!("{agents} agents"));
        }
        if commands > 0 {
            parts.push(format!("{commands} commands"));
        }
        if hooks > 0 {
            parts.push(format!("{hooks} hook sets"));
        }
        if lsp > 0 {
            parts.push(format!("{lsp} LSP servers"));
        }
        match parts.as_slice() {
            [] => format!("Removes {}.", overlay.title),
            [only] => format!("Removes {} and its {only}.", overlay.title),
            [head, tail @ ..] => {
                let tail = tail.join(", ");
                format!("Removes {} and its {head}, {tail}.", overlay.title)
            }
        }
    };
    app.config.overlay = Some(ConfigOverlayState::UninstallConfirm(UninstallConfirmState {
        plugin_id: overlay.plugin_id.clone(),
        title: overlay.title.clone(),
        description,
        scope: overlay.scope.clone(),
        project_path: overlay.project_path.clone(),
    }));
}

/// Enter confirms and dispatches the uninstall; Esc backs out to the
/// action overlay's place with nothing removed.
pub(crate) fn handle_uninstall_confirm_key(app: &mut App, key: KeyEvent) {
    match (key.code, key.modifiers) {
        (KeyCode::Esc, KeyModifiers::NONE) => app.config.overlay = None,
        (KeyCode::Enter, KeyModifiers::NONE) => {
            let Some(confirm) = app.config.uninstall_confirm().cloned() else {
                return;
            };
            let overlay = InstalledPluginActionOverlayState {
                plugin_id: confirm.plugin_id,
                title: confirm.title,
                description: confirm.description,
                scope: confirm.scope,
                project_path: confirm.project_path,
                selected_index: 0,
                actions: vec![InstalledPluginActionKind::Uninstall],
            };
            app.config.overlay = Some(ConfigOverlayState::InstalledPluginActions(overlay.clone()));
            super::execute_selected_installed_overlay_action(app);
        }
        _ => {}
    }
}
