//! `vault_state()`: the shared vault session's lock state and idle bound.

use super::ViewSurface;

impl ViewSurface {
    /// The shared vault session as the settings page draws it, or `None` when
    /// no `[vault]` is configured.
    pub fn vault_state(&self) -> Option<forge_workspace::VaultState> {
        self.workspace.vault_state()
    }
}
