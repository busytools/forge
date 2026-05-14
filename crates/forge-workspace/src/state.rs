//! `forge-state.toml` — persisted UI state (side-pane visibility).
//! Lives at `<config_dir>/forge-state.toml`, same dir as forge.toml.
//! Atomic-rename writes via `tempfile`.
//!
//! Historical note: this file used to also persist account
//! `last_used_at` clocks (for LRU) and `round_robin_next` (for RR
//! picking). Both are gone — the single account selection policy is
//! now usage-based off a live 30s-refresh cache that the workspace
//! holds in memory, so there's nothing useful to persist about
//! account choice across forge launches. Old keys in existing
//! `forge-state.toml` files are silently ignored on load.
use std::path::Path;

use serde::{Deserialize, Serialize};

/// On-disk schema for `forge-state.toml`. Missing file → empty
/// state (defaults). Parse failure → empty state + warn log; the
/// UI defaults to "panes visible" rather than blocking startup.
#[derive(Debug, Default, Deserialize, Serialize)]
pub(crate) struct PersistedState {
    #[serde(default)]
    pub ui: PersistedUiState,
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
        assert!(
            state.ui.projects_pane_visible,
            "ui.projects_pane_visible defaults to true on missing state",
        );
        assert!(state.ui.inspector_pane_visible);
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
    fn legacy_state_with_retired_sections_loads_default() {
        // forge-state.toml files written before the [selection] +
        // [[accounts]] tables were retired must still round-trip
        // — the retired sections are silently ignored.
        let dir = tempdir().expect("tempdir");
        std::fs::write(
            state_path(dir.path()),
            "[selection]\nround_robin_next = 1\n\n[accounts.\"Subspace\"]\nlast_used_at = \"2026-05-09T10:23:14Z\"\n",
        )
        .expect("write");
        let state = load_or_default(dir.path());
        assert!(state.ui.projects_pane_visible);
    }

    #[test]
    fn malformed_state_loads_default() {
        let dir = tempdir().expect("tempdir");
        std::fs::write(state_path(dir.path()), "not valid = = toml").expect("write");
        let state = load_or_default(dir.path());
        assert!(state.ui.projects_pane_visible);
    }

    #[test]
    fn save_to_unwritable_dir_logs_and_does_not_panic() {
        // Use a path that's almost certainly unwritable on macOS:
        // a file masquerading as a dir is not portable, so use
        // /dev/null as a non-directory parent.
        let bad_dir = std::path::PathBuf::from("/dev/null/notadir");
        let state = PersistedState::default();
        // Should not panic and not propagate an error.
        save(&bad_dir, &state);
    }
}
