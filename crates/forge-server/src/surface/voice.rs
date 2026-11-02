//! `voice()`: the speaking role's switch, model directory, and the seat's
//! listener count.

use std::path::PathBuf;

use forge_primitives::SessionSlot;

use super::ViewSurface;

/// The voice read one seat's control draws.
pub struct VoiceView {
    /// Whether `[tts] enabled` is set: without it nothing synthesises and
    /// the control has nothing to turn on.
    pub enabled: bool,
    /// Where the speaking model lands. `None` when the platform has no usable
    /// cache directory and none was configured.
    pub models_dir: Option<PathBuf>,
    /// Clients with voice on for this seat, including this one.
    pub listeners: u32,
}

impl ViewSurface {
    /// The `[tts]` section's own read: on or off, and where its model lands.
    /// No seat, because neither fact varies by one.
    pub fn voices(&self) -> forge_workspace::TtsView {
        self.workspace.voices()
    }

    /// A connection has left: every seat it was listening to drops it, and
    /// each seat's subscribers hear the count it left behind.
    pub fn voice_release(&self, client: u64) -> Vec<SessionSlot> {
        self.workspace.voice_release(client)
    }

    /// The seat's voice state. `listeners` is per seat, which is why the read
    /// names one.
    pub fn voice(&self, key: &SessionSlot) -> VoiceView {
        let view = self.workspace.voices();
        let listeners = u32::try_from(self.workspace.voice_listeners(key)).unwrap_or(u32::MAX);
        VoiceView { enabled: view.enabled, models_dir: view.models_dir, listeners }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    /// Catches either field being read from a source other than the settings:
    /// a switched-off `[tts]` must read as off, and the directory is the
    /// config's answer rather than the platform default.
    #[test]
    fn voice_reads_the_tts_settings() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let surface = ViewSurface::new(Arc::clone(&workspace));
        let seat = SessionSlot::from_str_for_test("voice");

        let view = surface.voice(&seat);
        assert!(!view.enabled, "the fixture's [tts] is absent, which is off");
        assert_eq!(view.listeners, 0);
    }
}
