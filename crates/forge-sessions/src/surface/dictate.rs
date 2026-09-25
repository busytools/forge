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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    /// Catches either field being read from a source other than the call
    /// it replaces - the models directory comes off the config rather
    /// than the platform default, so bypassing it leaves the first-run
    /// note naming the wrong path.
    #[test]
    fn dictate_agrees_with_the_calls_it_replaces() {
        let (workspace, _dir) = crate::surface::testing::workspace();

        let view = ViewSurface::new(Arc::clone(&workspace)).dictate();

        assert_eq!(view.snapshot, workspace.dictate_snapshot(), "the snapshot is the call's own",);
        assert_eq!(view.snapshot.models.len(), 2, "dictation is on, so the snapshot is not empty");
        assert_eq!(
            view.models_dir,
            workspace.dictate_models_dir(),
            "the models directory is the config's answer",
        );
        assert_eq!(
            view.models_dir,
            Some(PathBuf::from("/tmp/forge-dictate-models")),
            "the configured directory is the one reported, not the platform default",
        );
    }
}
