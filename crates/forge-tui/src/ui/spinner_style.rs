//! The spinner a terminal draws, and how often it repaints.
//!
//! Both are a client's own: `[ui]` was a server key for a client's
//! presentation and it left with the rest of the server's presentation
//! concerns, so what is here is a default and nothing that reads a config.

use std::ops::RangeInclusive;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Repaint rate when nothing says otherwise.
const DEFAULT_FPS: u32 = 120;

/// Coarsest interval a cadence can produce, matching the 30ms step of the
/// `App::spinner_frame` pulse counter; it binds only for rates 30-33,
/// holding those at the pre-120fps cadence. Nothing would drop frames
/// without it - spinner styles coarsen to fit - but dropping it steps a
/// rate of 30-32 on 31-33ms rather than 30ms (busytools/forge#587).
const COARSEST_REPAINT_INTERVAL: Duration = Duration::from_millis(30);

/// Accepted frame rates. The ceiling is the loop's own structural limit
/// (it tops out near 212fps in practice, and the on-screen fps readout
/// clamps its own average at 240); from 33 down,
/// [`COARSEST_REPAINT_INTERVAL`] takes over.
const FPS_RANGE: RangeInclusive<u32> = 30..=240;

/// How often forge repaints while an animation is running. Stored as the
/// frame interval rather than the frame rate, so a rate that isn't a
/// whole number of milliseconds keeps its microseconds instead of
/// rounding to a different rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepaintCadence {
    interval: Duration,
}

impl Default for RepaintCadence {
    fn default() -> Self {
        Self::from_fps(DEFAULT_FPS)
    }
}

impl RepaintCadence {
    /// Clamp `fps` into the accepted 30-240 range and convert it to a frame
    /// interval. The result never exceeds `COARSEST_REPAINT_INTERVAL`.
    ///
    /// No warn on a clamp: the only production caller is
    /// [`Self::default`], whose rate is in range, so a warning here could
    /// only ever fire from a test.
    pub fn from_fps(fps: u32) -> Self {
        let clamped = fps.clamp(*FPS_RANGE.start(), *FPS_RANGE.end());
        let interval = Duration::from_micros(1_000_000 / u64::from(clamped));
        Self { interval: interval.min(COARSEST_REPAINT_INTERVAL) }
    }

    /// Interval between repaints while animating.
    pub fn frame_interval(self) -> Duration {
        self.interval
    }

    /// Cadence a `requested_ms` animation will actually run at here: the
    /// coarser of design intent and what this repaint rate can paint.
    ///
    /// A style expresses intent, the repaint rate expresses capability.
    /// Asking for a step quicker than frames land would not produce a
    /// quicker spinner, only a stuttering one that skips glyphs, so
    /// intent gives way. Nothing is lost by the clamp - the frames it
    /// removes were never paintable. It is currently dormant: the
    /// quickest style asks 32ms against a gate that never exceeds
    /// `COARSEST_REPAINT_INTERVAL`.
    pub fn effective_cadence_ms(self, requested_ms: u64) -> u128 {
        u128::from(requested_ms).max(self.interval.as_millis())
    }
}

/// A spinner glyph cycle. Each variant carries a `frames()` accessor
/// (the cycle as `&'static [char]`) and a `cadence_ms()` accessor (the
/// per-style frame duration). One source of truth for the active
/// spinner across every animated surface - chat, input, projects pane,
/// inspector, tool-call icons, and the launchpad.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SpinnerStyle {
    /// `⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏` - the fast default braille spinner. Familiar;
    /// reads as continuous with the rest of the chrome.
    #[default]
    Braille,
    /// `◐◓◑◒` - phase-of-moon rotation. Calm, smooth.
    PhaseOfMoon,
    /// `· ✦ ✧ ✦` - ember sparkles. Works on any unicode terminal (no
    /// truecolor required); reads as "sparks flying off hot metal".
    Ember,
    /// `▁▂▃▄▅▆▇█▇▆▅▄▃▂` - vertical bar rising then falling, a smooth
    /// VU-meter pulse.
    BarsV,
    /// `✶✸✹✺✹✷` - rotating six-point star; twinkles.
    Star,
    /// `✦✧✩✪` - sparkle cycle; a lighter twinkle than the star.
    Sparkle,
}

impl SpinnerStyle {
    /// Frame cycle for this style. Returned slice has fixed-known
    /// length per variant - callers index modulo `len()` per render
    /// tick.
    pub fn frames(self) -> &'static [char] {
        match self {
            Self::Braille => &[
                '\u{280B}', '\u{2819}', '\u{2839}', '\u{2838}', '\u{283C}', '\u{2834}', '\u{2826}',
                '\u{2827}', '\u{2807}', '\u{280F}',
            ],
            Self::PhaseOfMoon => &['\u{25D0}', '\u{25D3}', '\u{25D1}', '\u{25D2}'],
            Self::Ember => &['\u{00B7}', '\u{2726}', '\u{2727}', '\u{2726}'],
            Self::BarsV => &[
                '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}',
                '\u{2588}', '\u{2587}', '\u{2586}', '\u{2585}', '\u{2584}', '\u{2583}', '\u{2582}',
            ],
            Self::Star => &['\u{2736}', '\u{2738}', '\u{2739}', '\u{273A}', '\u{2739}', '\u{2737}'],
            Self::Sparkle => &['\u{2726}', '\u{2727}', '\u{2729}', '\u{272A}'],
        }
    }

    /// Lower-case key used in TOML serde. Matches the `serde
    /// rename_all = "snake_case"` mapping above. Useful for
    /// rendering the current value back to the user (e.g. in help
    /// output or config dump).
    pub fn key(self) -> &'static str {
        match self {
            Self::Braille => "braille",
            Self::PhaseOfMoon => "phase_of_moon",
            Self::Ember => "ember",
            Self::BarsV => "bars_v",
            Self::Star => "star",
            Self::Sparkle => "sparkle",
        }
    }

    /// Per-style frame cadence in milliseconds - the style's design
    /// intent, not necessarily what it runs at. A slow `[ui] fps` can
    /// coarsen it; [`RepaintCadence::effective_cadence_ms`] resolves
    /// the two, and that is what the frame index must divide by.
    /// Braille is the fast default; the rest are tuned per glyph set.
    pub fn cadence_ms(self) -> u64 {
        match self {
            Self::Braille => 32,
            Self::PhaseOfMoon => 90,
            Self::BarsV => 70,
            Self::Star => 130,
            Self::Ember | Self::Sparkle => 160,
        }
    }

    /// Every style in picker display order. Single source of truth for
    /// "all spinner styles" - the `/spinner` picker and the name parser
    /// both iterate this.
    pub const ALL_STYLES: [SpinnerStyle; 6] =
        [Self::Braille, Self::PhaseOfMoon, Self::Ember, Self::BarsV, Self::Star, Self::Sparkle];

    /// Parse a lower-case key (the inverse of [`Self::key`]) into its
    /// style. `None` for any unrecognised key.
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL_STYLES.into_iter().find(|style| style.key() == key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_spinner_is_braille() {
        let style = SpinnerStyle::default();
        assert_eq!(style, SpinnerStyle::Braille);
        assert_eq!(style.frames().len(), 10);
    }

    #[test]
    fn each_variant_has_non_empty_frames() {
        for style in SpinnerStyle::ALL_STYLES {
            assert!(
                !style.frames().is_empty(),
                "{} should have a non-empty frame cycle",
                style.key()
            );
        }
    }

    #[test]
    fn cadence_ms_is_per_style() {
        // Each style has its own cadence - drift means a surface no
        // longer ticks at the design-spec frequency.
        assert_eq!(SpinnerStyle::Braille.cadence_ms(), 32);
        assert_eq!(SpinnerStyle::PhaseOfMoon.cadence_ms(), 90);
        assert_eq!(SpinnerStyle::Ember.cadence_ms(), 160);
        assert_eq!(SpinnerStyle::BarsV.cadence_ms(), 70);
        assert_eq!(SpinnerStyle::Star.cadence_ms(), 130);
        assert_eq!(SpinnerStyle::Sparkle.cadence_ms(), 160);
    }

    #[test]
    fn from_key_round_trips_every_style() {
        for style in SpinnerStyle::ALL_STYLES {
            assert_eq!(SpinnerStyle::from_key(style.key()), Some(style));
        }
    }

    #[test]
    fn from_key_rejects_unknown() {
        assert_eq!(SpinnerStyle::from_key("nope"), None);
        assert_eq!(SpinnerStyle::from_key(""), None);
    }

    #[test]
    fn all_styles_lists_every_variant() {
        assert_eq!(SpinnerStyle::ALL_STYLES.len(), 6);
        // Per-variant identity: a variant added to the enum without
        // joining ALL_STYLES would be unparseable by key and unpickable.
        for style in [
            SpinnerStyle::Braille,
            SpinnerStyle::PhaseOfMoon,
            SpinnerStyle::Ember,
            SpinnerStyle::BarsV,
            SpinnerStyle::Star,
            SpinnerStyle::Sparkle,
        ] {
            assert!(SpinnerStyle::ALL_STYLES.contains(&style), "{style:?} missing from ALL_STYLES");
        }
    }

    /// The default has to go through the same microsecond arithmetic as
    /// an explicit value, or 120 quietly becomes the 8ms/125fps that
    /// whole-millisecond rounding would give.
    #[test]
    fn the_default_interval_is_not_rounded_to_whole_milliseconds() {
        let interval = RepaintCadence::default().frame_interval();
        assert_eq!(interval, Duration::from_micros(8333));
        assert_ne!(interval, Duration::from_millis(8), "8ms would be 125fps, not 120");
    }

    /// No style may ask for frames that cannot be painted. This used to
    /// be enforced by forbidding any style quicker than the coarsest
    /// gate; now the gate coarsens the style instead, so the property is
    /// stated over the resolved cadence: never quicker than repaints
    /// allow, never quicker than the style asked for, and exactly what
    /// the style asked for whenever repaints can keep up.
    #[test]
    fn no_style_asks_for_frames_the_repaint_rate_cannot_paint() {
        for fps in [0, 1, 30, 33, 45, 60, 90, 120, 240, u32::MAX] {
            let cadence = RepaintCadence::from_fps(fps);
            let interval_ms = cadence.frame_interval().as_millis();
            for style in SpinnerStyle::ALL_STYLES {
                let intent = u128::from(style.cadence_ms());
                let effective = cadence.effective_cadence_ms(style.cadence_ms());
                assert!(
                    effective >= interval_ms,
                    "fps={fps} {}: a {effective}ms step inside a {interval_ms}ms repaint",
                    style.key(),
                );
                assert!(
                    effective >= intent,
                    "fps={fps} {}: {effective}ms is quicker than the {intent}ms intent",
                    style.key(),
                );
                if intent >= interval_ms {
                    assert_eq!(
                        effective,
                        intent,
                        "fps={fps} {}: repaints allow {intent}ms, so it must run at it",
                        style.key(),
                    );
                }
            }
        }
    }

    /// The clamp on its own terms. It takes a synthetic cadence rather
    /// than a style because no shipped style is quick enough to reach
    /// it: the quickest asks 32ms and the gate stops at 30ms, so
    /// `ALL_STYLES` holds nothing that could stand in for one.
    #[test]
    fn a_cadence_quicker_than_the_repaint_floor_coarsens_to_it() {
        let floor = RepaintCadence::from_fps(*FPS_RANGE.start());
        assert_eq!(
            floor.frame_interval(),
            COARSEST_REPAINT_INTERVAL,
            "the slowest accepted fps must sit on the floor, or this tests nothing",
        );
        assert_eq!(
            floor.effective_cadence_ms(24),
            30,
            "a 24ms step is not paintable at a 30ms interval, so it coarsens to 30ms",
        );
        assert_eq!(
            floor.effective_cadence_ms(32),
            32,
            "a 32ms step outruns the floor and keeps its own intent",
        );
    }

    #[test]
    fn revised_set_drops_pulse_forgedot_adds_new() {
        assert_eq!(SpinnerStyle::BarsV.frames().len(), 14);
        assert_eq!(SpinnerStyle::Star.key(), "star");
        assert_eq!(SpinnerStyle::from_key("sparkle"), Some(SpinnerStyle::Sparkle));
        assert_eq!(SpinnerStyle::from_key("pulse"), None);
        assert_eq!(SpinnerStyle::from_key("forge_dot"), None);
    }
}
