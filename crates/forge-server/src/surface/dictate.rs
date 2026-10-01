//! `dictate()`: devices, models and preflight state.

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
    /// Whether `[dictate] enabled` is set. **Carried rather than inferred
    /// from an empty `models` list**: the list is empty for a switched-off
    /// section AND for a `DictateSnapshot::default()`, so a reader asserting
    /// the cause from the value reports a healthy configuration as switched
    /// off - or a switched-off one as healthy - with nothing to say which.
    pub enabled: bool,
    /// Per-model fetch and load progress.
    pub snapshot: DictateSnapshot,
    /// Where the dictation models land. `None` when the platform has no
    /// usable cache directory and none was configured.
    pub models_dir: Option<PathBuf>,
    /// The input a pick has moved this process to, over the configured pin.
    ///
    /// `SetDictateDevice` crossed the wire and nothing read it back, so a
    /// client could move the device and had no way to see where it had moved
    /// it to. `None` means the pin stands.
    pub device: Option<forge_workspace::DictateDeviceChoice>,
}

impl ViewSurface {
    pub fn dictate(&self) -> DictateView {
        DictateView {
            enabled: self.workspace.dictate_enabled(),
            snapshot: self.workspace.dictate_snapshot(),
            models_dir: self.workspace.dictate_models_dir(),
            device: self.workspace.dictate_device_pick(),
        }
    }

    /// The `[dictate] bind` key, as `forge.toml` declares it. The terminal
    /// reads the same value for its own key handler; a client draws the
    /// keyboard the record carries rather than hardcoding one.
    pub fn dictate_bind(&self) -> forge_workspace::DictateBind {
        self.workspace.dictate_bind()
    }

    /// How a press maps onto a take, as `forge.toml` declares it.
    pub fn dictate_mode(&self) -> forge_workspace::DictateMode {
        self.workspace.dictate_mode()
    }

    /// Every input forge can record from, and the configured pin.
    ///
    /// A blocking walk that opens the microphone stack, so a caller runs it
    /// off its own thread: the socket answers an on-demand request with it,
    /// and the terminal reaches it from a spawned task.
    pub fn dictate_device_catalog(&self) -> Result<forge_workspace::DictateDeviceCatalog, String> {
        self.workspace.dictate_device_catalog()
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

        assert_eq!(view.snapshot, workspace.dictate_snapshot(), "the snapshot is the call's own");
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
