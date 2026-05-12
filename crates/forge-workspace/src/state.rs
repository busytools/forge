//! `forge-state.toml` — persisted picker state (last_used_at,
//! round_robin_next). Lives at `<config_dir>/forge-state.toml`,
//! same dir as forge.toml. Atomic-rename writes via `tempfile`.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// On-disk schema for `forge-state.toml`. Missing file → empty
/// state (zeroed). Parse failure → empty state + warn log; the
/// picker degrades to "everyone is least-recently-used" rather
/// than blocking startup.
#[derive(Debug, Default, Deserialize, Serialize)]
pub(crate) struct PersistedState {
    #[serde(default)]
    pub accounts: HashMap<String, PersistedAccountState>,
    #[serde(default)]
    pub selection: PersistedSelectionState,
    #[serde(default)]
    pub ui: PersistedUiState,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub(crate) struct PersistedAccountState {
    /// RFC 3339 UTC. `None` → never used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub(crate) struct PersistedSelectionState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round_robin_next: Option<usize>,
}

/// UI preferences persisted across forge launches. Scoped to the
/// Wide / Medium-tier side-pane visibility toggles (Ctrl+B for the
/// left Projects pane, Ctrl+E for the right Inspector pane). New
/// fields land with their own `#[serde(default = "...")]` defaults
/// so older `forge-state.toml` files keep round-tripping.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct PersistedUiState {
    /// Whether the Projects pane was visible at last shutdown.
    /// Default `true` so first-launch users see the pane.
    #[serde(default = "default_pane_visible")]
    pub projects_pane_visible: bool,
    /// Whether the Inspector pane was visible at last shutdown.
    /// Default `true` so first-launch users see the pane.
    #[serde(default = "default_pane_visible")]
    pub inspector_pane_visible: bool,
}

impl Default for PersistedUiState {
    fn default() -> Self {
        Self {
            projects_pane_visible: default_pane_visible(),
            inspector_pane_visible: default_pane_visible(),
        }
    }
}

const fn default_pane_visible() -> bool {
    true
}

/// Path: `<config_dir>/forge-state.toml`.
pub(crate) fn state_path(config_dir: &Path) -> std::path::PathBuf {
    config_dir.join("forge-state.toml")
}

/// Load the state file. Missing or unparseable → default
/// (empty) state; never errors out of `Workspace::new`.
pub(crate) fn load_or_default(config_dir: &Path) -> PersistedState {
    let path = state_path(config_dir);
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return PersistedState::default();
        }
        Err(e) => {
            tracing::warn!(
                target: "forge_workspace::state",
                path = %path.display(),
                error = %e,
                "failed to read forge-state.toml; using empty state",
            );
            return PersistedState::default();
        }
    };
    match toml::from_str(&raw) {
        Ok(parsed) => parsed,
        Err(e) => {
            tracing::warn!(
                target: "forge_workspace::state",
                path = %path.display(),
                error = %e,
                "failed to parse forge-state.toml; using empty state",
            );
            PersistedState::default()
        }
    }
}

/// Atomic-rename write. Logs `tracing::error!` on failure but
/// does not propagate — callers continue uninterrupted.
pub(crate) fn save(config_dir: &Path, state: &PersistedState) {
    let path = state_path(config_dir);
    let serialised = match toml::to_string_pretty(state) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(
                target: "forge_workspace::state",
                path = %path.display(),
                error = %e,
                "failed to serialise forge-state.toml; skipping write",
            );
            return;
        }
    };

    let temp = match tempfile::NamedTempFile::new_in(config_dir) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(
                target: "forge_workspace::state",
                path = %path.display(),
                error = %e,
                "failed to create forge-state.toml temp file; skipping write",
            );
            return;
        }
    };

    if let Err(e) = std::fs::write(temp.path(), serialised) {
        tracing::error!(
            target: "forge_workspace::state",
            path = %path.display(),
            error = %e,
            "failed to write forge-state.toml temp file; skipping write",
        );
        return;
    }

    if let Err(e) = temp.persist(&path) {
        tracing::error!(
            target: "forge_workspace::state",
            path = %path.display(),
            error = %e.error,
            "failed to atomic-rename forge-state.toml; skipping write",
        );
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn missing_state_loads_default() {
        let dir = tempdir().expect("tempdir");
        let state = load_or_default(dir.path());
        assert!(state.accounts.is_empty());
        assert!(state.selection.round_robin_next.is_none());
        assert!(
            state.ui.projects_pane_visible,
            "ui.projects_pane_visible defaults to true on missing state",
        );
    }

    #[test]
    fn ui_section_round_trips() {
        let dir = tempdir().expect("tempdir");
        let mut state = PersistedState::default();
        state.ui.projects_pane_visible = false;

        save(dir.path(), &state);
        let loaded = load_or_default(dir.path());
        assert!(!loaded.ui.projects_pane_visible);

        let mut state = loaded;
        state.ui.projects_pane_visible = true;
        save(dir.path(), &state);
        let loaded = load_or_default(dir.path());
        assert!(loaded.ui.projects_pane_visible);
    }

    #[test]
    fn legacy_state_without_ui_section_loads_default() {
        // forge-state.toml files written before the `[ui]` section
        // existed must still round-trip with the default visibility.
        let dir = tempdir().expect("tempdir");
        std::fs::write(state_path(dir.path()), "[selection]\nround_robin_next = 1\n")
            .expect("write");
        let state = load_or_default(dir.path());
        assert_eq!(state.selection.round_robin_next, Some(1));
        assert!(
            state.ui.projects_pane_visible,
            "legacy state without [ui] still defaults projects_pane_visible to true",
        );
    }

    #[test]
    fn round_trips_account_last_used_at() {
        let dir = tempdir().expect("tempdir");
        let mut state = PersistedState::default();
        state.accounts.insert(
            "Subspace".to_owned(),
            PersistedAccountState { last_used_at: Some("2026-05-09T10:23:14Z".to_owned()) },
        );
        state.selection.round_robin_next = Some(2);

        save(dir.path(), &state);
        let loaded = load_or_default(dir.path());

        assert_eq!(
            loaded.accounts.get("Subspace").and_then(|a| a.last_used_at.as_deref()),
            Some("2026-05-09T10:23:14Z"),
        );
        assert_eq!(loaded.selection.round_robin_next, Some(2));
    }

    #[test]
    fn malformed_state_loads_default() {
        let dir = tempdir().expect("tempdir");
        std::fs::write(state_path(dir.path()), "not valid = = toml").expect("write");
        let state = load_or_default(dir.path());
        assert!(state.accounts.is_empty());
    }

    #[test]
    fn save_to_unwritable_dir_logs_and_does_not_panic() {
        // Use a path that's almost certainly unwritable on macOS:
        // a file masquerading as a dir is not portable, so use
        // /dev/null as a non-directory parent.
        let bad_dir = std::path::PathBuf::from("/dev/null/notadir");
        let mut state = PersistedState::default();
        state
            .accounts
            .insert("X".to_owned(), PersistedAccountState { last_used_at: Some("now".to_owned()) });
        // Should not panic and not propagate an error.
        save(&bad_dir, &state);
    }
}
