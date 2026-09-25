//! Dictation's preflight state as a view reads it.

use std::path::PathBuf;

use forge_workspace::DictateSnapshot;

use super::ViewSurface;

/// Dictation's preflight state: what the preflight screen draws.
///
/// The device catalog is deliberately absent. Enumerating devices is a
/// blocking cpal walk that also trips a microphone check, so it is
/// reached on demand from a spawned task rather than read per frame
/// along with this.
pub struct DictateView {
    /// Per-model fetch and load progress. Empty `models` means
    /// `[dictate] enabled` is false.
    pub snapshot: DictateSnapshot,
    /// Where the dictation models land. `None` when the platform has no
    /// usable cache directory and none was configured.
    pub models_dir: Option<PathBuf>,
}

impl ViewSurface {
    pub fn dictate(&self) -> DictateView {
        DictateView {
            snapshot: self.workspace.dictate_snapshot(),
            models_dir: self.workspace.dictate_models_dir(),
        }
    }
}
