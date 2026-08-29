//! UI configuration knobs - the `[ui]` section in `forge.toml`.
//!
//! Carries the active spinner style and the repaint cadence.
//! Distinct from per-session UI state (input editor, viewport, etc.)
//! which lives on `UiSession` in forge-tui - this is workspace-level
//! configuration that survives across sessions and processes.

use std::ops::RangeInclusive;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// All `[ui]` section knobs. Every field has a default so an
/// absent `[ui]` section in `forge.toml` is equivalent to all
/// defaults.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct UiSettings {
    /// Active spinner style for every animated surface (launchpad,
    /// chat thinking/working, input box, projects pane, inspector).
    /// Default is `Braille`. The legacy `launchpad_spinner` key is
    /// accepted as an alias so an existing forge.toml keeps working.
    /// An unknown/removed key falls back to the default rather than
    /// failing the load (see `deserialize_lenient`).
    #[serde(default, alias = "launchpad_spinner", deserialize_with = "deserialize_lenient")]
    pub spinner: SpinnerStyle,
    /// Target repaint rate while something on screen is animating.
    /// Absent, out-of-range or non-integer values resolve to the
    /// default rather than failing the load (see `deserialize_fps`).
    #[serde(default, deserialize_with = "deserialize_fps")]
    pub fps: RepaintCadence,
}

/// Repaint rate when `[ui] fps` is absent.
const DEFAULT_FPS: u32 = 120;

/// Coarsest interval any `[ui] fps` can produce, matching the 30ms step
/// of the `App::spinner_frame` pulse counter; it binds only for `fps`
/// 30-33, holding those at the pre-120fps cadence. Nothing would drop
/// frames without it - spinner styles coarsen to fit - but dropping it
/// steps `fps` 30-32 on 31-33ms rather than 30ms (busytools/forge#587).
const COARSEST_REPAINT_INTERVAL: Duration = Duration::from_millis(30);

/// Accepted `[ui] fps` values. The ceiling is the loop's own structural
/// limit (it tops out near 212fps in practice, and the on-screen fps
/// readout clamps its own average at 240); from 33 down,
/// [`COARSEST_REPAINT_INTERVAL`] takes over.
const FPS_RANGE: RangeInclusive<u32> = 30..=240;

/// How often forge repaints while an animation is running, from the
/// `[ui] fps` key. Stored as the frame interval rather than the frame
/// rate, so a rate that isn't a whole number of milliseconds keeps its
/// microseconds instead of rounding to a different rate.
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
    /// Clamp `fps` into the accepted 30-240 range and convert it to a
    /// frame interval, warning when the value had to move. The result
    /// never exceeds `COARSEST_REPAINT_INTERVAL`.
    pub fn from_fps(fps: u32) -> Self {
        let clamped = fps.clamp(*FPS_RANGE.start(), *FPS_RANGE.end());
        if clamped != fps {
            tracing::warn!(
                target: "forge_workspace::ui",
                requested = fps,
                applied = clamped,
                "[ui] fps is outside the supported range; clamping",
            );
        }
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

/// Lenient deserialize for `[ui] fps`. A non-integer value (a float, a
/// string, a table) resolves to the default rather than failing the
/// whole config load - a hand-edited typo must never stop forge
/// booting. Out-of-range integers clamp instead, in
/// [`RepaintCadence::from_fps`].
pub fn deserialize_fps<'de, D>(deserializer: D) -> Result<RepaintCadence, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Fps(u32),
        NotAnInteger(serde::de::IgnoredAny),
    }

    match Raw::deserialize(deserializer)? {
        Raw::Fps(fps) => Ok(RepaintCadence::from_fps(fps)),
        Raw::NotAnInteger(_) => {
            tracing::warn!(
                target: "forge_workspace::ui",
                "[ui] fps is not a whole number; using the default cadence",
            );
            Ok(RepaintCadence::default())
        }
    }
}

/// Lenient deserialize for a persisted spinner key (the `[ui] spinner`
/// config field): an unknown/removed key (a dropped variant, a typo)
/// resolves to the default style instead of failing the whole load - a
/// stale value must never break boot.
pub fn deserialize_lenient<'de, D>(deserializer: D) -> Result<SpinnerStyle, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let key = String::deserialize(deserializer)?;
    Ok(SpinnerStyle::from_key(&key).unwrap_or_default())
}

/// Resolve the effective spinner: a persisted `/spinner` override wins
/// over the forge.toml `[ui] spinner` default. The single precedence
/// point, so a boot-time edit can't silently drop the user's pick.
pub(crate) fn resolve_spinner(
    override_: Option<SpinnerStyle>,
    default_: SpinnerStyle,
) -> SpinnerStyle {
    override_.unwrap_or(default_)
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
    fn resolve_spinner_override_wins_over_default() {
        assert_eq!(
            resolve_spinner(Some(SpinnerStyle::Ember), SpinnerStyle::Star),
            SpinnerStyle::Ember,
            "a persisted override beats the forge.toml default",
        );
    }

    #[test]
    fn resolve_spinner_falls_back_to_default_when_no_override() {
        assert_eq!(
            resolve_spinner(None, SpinnerStyle::Star),
            SpinnerStyle::Star,
            "no override falls through to the forge.toml default",
        );
    }

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
    fn key_round_trips_through_serde() {
        for style in SpinnerStyle::ALL_STYLES {
            let toml = format!("spinner = \"{}\"\n", style.key());
            let parsed: UiSettings = toml::from_str(&toml).expect("parse round trip");
            assert_eq!(parsed.spinner, style);
        }
    }

    #[test]
    fn absent_ui_section_yields_defaults() {
        let parsed: UiSettings = toml::from_str("").expect("empty parses");
        assert_eq!(parsed, UiSettings::default());
        assert_eq!(parsed.spinner, SpinnerStyle::Braille);
    }

    #[test]
    fn unknown_spinner_key_falls_back_to_default() {
        // A removed variant or a typo must NOT fail the config load - it
        // resolves to the default (Braille) so a stale forge.toml value
        // never breaks boot.
        let removed: UiSettings =
            toml::from_str("spinner = \"pulse\"\n").expect("removed key parses");
        assert_eq!(removed.spinner, SpinnerStyle::Braille);
        let typo: UiSettings = toml::from_str("spinner = \"corkscrew\"\n").expect("typo parses");
        assert_eq!(typo.spinner, SpinnerStyle::Braille);
    }

    #[test]
    fn spinner_field_parses_new_key() {
        let parsed: UiSettings = toml::from_str("spinner = \"ember\"\n").expect("parse");
        assert_eq!(parsed.spinner, SpinnerStyle::Ember);
    }

    #[test]
    fn legacy_launchpad_spinner_key_still_parses_via_alias() {
        let parsed: UiSettings = toml::from_str("launchpad_spinner = \"ember\"\n").expect("parse");
        assert_eq!(parsed.spinner, SpinnerStyle::Ember);
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
    }

    #[test]
    fn absent_fps_key_defaults_to_120() {
        let parsed: UiSettings = toml::from_str("spinner = \"ember\"\n").expect("parse");
        assert_eq!(parsed.fps, RepaintCadence::default());
        assert_eq!(parsed.fps, RepaintCadence::from_fps(120));
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

    #[test]
    fn fps_converts_to_a_frame_interval() {
        let parsed: UiSettings = toml::from_str("fps = 120\n").expect("parse");
        // 8333us, not a truncated 8ms - the gate divides in micros.
        assert_eq!(parsed.fps.frame_interval(), Duration::from_micros(8333));
        assert_eq!(RepaintCadence::from_fps(60).frame_interval(), Duration::from_micros(16_666));
        assert_eq!(RepaintCadence::from_fps(240).frame_interval(), Duration::from_micros(4166));
    }

    #[test]
    fn out_of_range_fps_clamps_instead_of_failing() {
        let high: UiSettings = toml::from_str("fps = 100000\n").expect("high parses");
        assert_eq!(high.fps, RepaintCadence::from_fps(240));
        // Clamps to the 30fps floor, then the coarsest-interval cap
        // rounds that 33.3ms back to 30ms.
        let low: UiSettings = toml::from_str("fps = 0\n").expect("zero parses");
        assert_eq!(low.fps.frame_interval(), Duration::from_millis(30));
        assert_eq!(low.fps, RepaintCadence::from_fps(30));
        let negative: UiSettings = toml::from_str("fps = -5\n").expect("negative parses");
        assert_eq!(negative.fps, RepaintCadence::default());
    }

    #[test]
    fn non_integer_fps_falls_back_to_the_default() {
        for bad in ["fps = 12.5\n", "fps = \"fast\"\n", "fps = true\n", "fps = [120]\n"] {
            let parsed: UiSettings = toml::from_str(bad).unwrap_or_else(|e| {
                panic!("{bad:?} must not fail the load: {e}");
            });
            assert_eq!(parsed.fps, RepaintCadence::default(), "{bad:?}");
        }
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
