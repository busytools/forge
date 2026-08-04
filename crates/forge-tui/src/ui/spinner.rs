//! Shared spinner frame selection.
//!
//! One time-based helper drives every animated spinner surface from
//! the active `SpinnerStyle`'s own cadence.

use forge_workspace::{RepaintCadence, SpinnerStyle};

use crate::app::App;

/// Reduced-motion cadence floor (ms). The quicker styles (braille 24,
/// bars_v 70, phase_of_moon 90, star 130) clamp up to this so a
/// reduced-motion preference visibly slows them; ember and sparkle
/// already sit on the floor.
pub(crate) const REDUCED_FLOOR_MS: u64 = 160;

/// Frame index for `style` at `elapsed_ms` since the spinner epoch.
/// `elapsed / cadence`, wrapped to the frame count. The cadence resolves
/// through two clamps: reduced motion floors the style's intent, then
/// `repaint` coarsens whatever is left to something paintable.
pub fn spinner_frame_index(
    style: SpinnerStyle,
    elapsed_ms: u128,
    reduced_motion: bool,
    repaint: RepaintCadence,
) -> usize {
    let requested =
        if reduced_motion { style.cadence_ms().max(REDUCED_FLOOR_MS) } else { style.cadence_ms() };
    let cadence = repaint.effective_cadence_ms(requested).max(1);
    ((elapsed_ms / cadence) as usize) % style.frames().len()
}

impl App {
    /// Current glyph for the active spinner style, derived from the
    /// app's spinner epoch and the reduced-motion preference. Every
    /// main-UI surface calls this so they animate in lock-step at the
    /// active style's own cadence.
    pub fn active_spinner_glyph(&self) -> char {
        self.spinner_glyph_for(self.spinner_style)
    }

    /// Current glyph for an arbitrary `style` (not necessarily the
    /// active one), using the same epoch + reduced-motion inputs as
    /// [`Self::active_spinner_glyph`]. The `/spinner` picker animates
    /// each row at its own style's cadence through this.
    pub fn spinner_glyph_for(&self, style: SpinnerStyle) -> char {
        let elapsed = self.spinner_epoch.elapsed().as_millis();
        let reduced = self.config.prefers_reduced_motion_effective();
        let idx = spinner_frame_index(style, elapsed, reduced, self.repaint_cadence);
        style.frames()[idx]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spinner_index_advances_by_cadence() {
        // The default 120fps paints quicker than any style asks, so
        // every style runs at its own intent there.
        let quick = RepaintCadence::default();
        let s = SpinnerStyle::Braille; // 10 frames, 24ms
        assert_eq!(spinner_frame_index(s, 0, false, quick), 0);
        assert_eq!(spinner_frame_index(s, 24, false, quick), 1);
        assert_eq!(spinner_frame_index(s, 240, false, quick), 0); // wraps at len*cadence
    }

    #[test]
    fn spinner_index_reduced_motion_is_slower() {
        let quick = RepaintCadence::default();
        let s = SpinnerStyle::Braille;
        // At 24ms elapsed: normal shows frame 1; reduced (floored to
        // 160ms) is still on frame 0.
        assert_eq!(spinner_frame_index(s, 24, true, quick), 0);
        assert_eq!(spinner_frame_index(s, 24, false, quick), 1);
    }

    /// A slow `[ui] fps` coarsens the glyph rather than making it skip:
    /// at the 30ms floor braille steps on 30ms, so the frame it would
    /// have shown at 24ms is never asked for.
    #[test]
    fn a_slow_repaint_rate_coarsens_the_glyph_instead_of_dropping_frames() {
        let s = SpinnerStyle::Braille;
        let slow = RepaintCadence::from_fps(30);
        assert_eq!(spinner_frame_index(s, 24, false, slow), 0, "24ms is not paintable at fps=30");
        assert_eq!(spinner_frame_index(s, 30, false, slow), 1);
        // Every step is reachable - no glyph is skipped by the clamp.
        let seen: Vec<usize> =
            (0..10).map(|step| spinner_frame_index(s, step * 30, false, slow)).collect();
        assert_eq!(seen, (0..10).collect::<Vec<usize>>());
    }
}
