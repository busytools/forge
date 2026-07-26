//! Shared spinner frame selection.
//!
//! One time-based helper drives every animated spinner surface from
//! the active `SpinnerStyle`'s own cadence.

use forge_workspace::SpinnerStyle;

use crate::app::App;

/// Reduced-motion cadence floor (ms). The fast styles (braille 80,
/// pulse 100, phase_of_moon 120) clamp up to this so a reduced-motion
/// preference visibly slows them; ember (180) and forge_dot (1400)
/// already sit past the floor.
pub(crate) const REDUCED_FLOOR_MS: u64 = 160;

/// Frame index for `style` at `elapsed_ms` since the spinner epoch.
/// `elapsed / cadence`, wrapped to the frame count; under reduced
/// motion the cadence is floored so the fast styles slow down.
pub fn spinner_frame_index(style: SpinnerStyle, elapsed_ms: u128, reduced_motion: bool) -> usize {
    let cadence =
        if reduced_motion { style.cadence_ms().max(REDUCED_FLOOR_MS) } else { style.cadence_ms() };
    let cadence = u128::from(cadence).max(1);
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
        let idx = spinner_frame_index(style, elapsed, reduced);
        style.frames()[idx]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spinner_index_advances_by_cadence() {
        let s = SpinnerStyle::Braille; // 10 frames, 30ms
        assert_eq!(spinner_frame_index(s, 0, false), 0);
        assert_eq!(spinner_frame_index(s, 30, false), 1);
        assert_eq!(spinner_frame_index(s, 300, false), 0); // wraps at len*cadence (10*30)
    }

    #[test]
    fn spinner_index_reduced_motion_is_slower() {
        let s = SpinnerStyle::Braille; // 30ms normal
        // At 30ms elapsed: normal shows frame 1; reduced (floored to
        // 160ms) is still on frame 0.
        assert_eq!(spinner_frame_index(s, 30, true), 0);
        assert_eq!(spinner_frame_index(s, 30, false), 1);
    }
}
