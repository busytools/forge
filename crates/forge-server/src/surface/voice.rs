//! `voices()`: the speaking role's switch and model directory, and the
//! release a dropped connection owes every seat it was listening to.

use forge_primitives::SessionSlot;

use super::ViewSurface;

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
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    /// A switched-off `[tts]` reads as off, and the directory is the
    /// config's answer rather than the platform default.
    #[test]
    fn voices_reads_the_tts_settings() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let surface = ViewSurface::new(Arc::clone(&workspace));

        let view = surface.voices();
        assert!(!view.enabled, "the fixture's [tts] is absent, which is off");
        assert!(
            view.models_dir.as_ref().is_some_and(|dir| dir.ends_with("forge-tts")),
            "an unset models_dir is the platform cache, got: {:?}",
            view.models_dir,
        );
    }
}
