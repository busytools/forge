//! UI configuration knobs - the `[ui]` section in `forge.toml`.
//!
//! Currently carries the active spinner style.
//! Distinct from per-session UI state (input editor, viewport, etc.)
//! which lives on `UiSession` in forge-tui - this is workspace-level
//! configuration that survives across sessions and processes.

use serde::Deserialize;

/// All `[ui]` section knobs. Every field has a default so an
/// absent `[ui]` section in `forge.toml` is equivalent to all
/// defaults.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct UiSettings {
    /// Active spinner style for every animated surface (launchpad,
    /// chat thinking/working, input box, projects pane, inspector).
    /// Default is `Braille`. The legacy `launchpad_spinner` key is
    /// accepted as an alias so an existing forge.toml keeps working.
    #[serde(default, alias = "launchpad_spinner")]
    pub spinner: SpinnerStyle,
}

/// Spinner glyph cycle used by the launchpad. Each variant carries
/// a `frames()` accessor returning the cycle as `&'static [char]`
/// and a `cadence_ms()` accessor returning the per-style frame
/// duration.
///
/// Glyph choice is intentionally varied - different launchpad
/// personalities should pick visually distinct alternatives so the
/// terminal mode "feels different" from the in-chat braille spinner.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SpinnerStyle {
    /// `⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏` - the default braille spinner used everywhere
    /// else in forge. Familiar; the launchpad reads as continuous
    /// with the rest of the chrome.
    #[default]
    Braille,
    /// `◐◓◑◒` - phase-of-moon rotation. Calm, smooth, distinct
    /// from braille so the launchpad reads as its own surface.
    PhaseOfMoon,
    /// `○◔◑◕●◕◑◔` - pulse fill. Breathing in and out, "alive."
    Pulse,
    /// `●` - solid bullet. Intended for a forge-orange intensity
    /// tween at the render layer; the frame is single-glyph so the
    /// animation is driven by colour modulation rather than glyph
    /// changes. See `forge_dot_alpha_step` in the launchpad
    /// renderer for the opacity ramp.
    ForgeDot,
    /// `· ✦ ✧ ✦` - ember sparkles. Branded but works on any unicode
    /// terminal (no truecolor required). 180ms cadence reads as
    /// "sparks flying off hot metal" rather than anxious blinking.
    Ember,
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
            Self::Pulse => &[
                '\u{25CB}', '\u{25D4}', '\u{25D1}', '\u{25D5}', '\u{25CF}', '\u{25D5}', '\u{25D1}',
                '\u{25D4}',
            ],
            Self::ForgeDot => &['\u{25CF}'],
            Self::Ember => &['\u{00B7}', '\u{2726}', '\u{2727}', '\u{2726}'],
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
            Self::Pulse => "pulse",
            Self::ForgeDot => "forge_dot",
            Self::Ember => "ember",
        }
    }

    /// Per-style frame cadence in milliseconds. The launchpad render
    /// driver derives the current frame index from
    /// `elapsed_since_open.as_millis() / cadence_ms`.
    ///
    /// `forge_dot` is the special one - its frame table is single-
    /// glyph and the cadence here drives a full opacity tween cycle
    /// (so it's intentionally much slower than the others, ~1.4s
    /// per cycle).
    pub fn cadence_ms(self) -> u64 {
        match self {
            Self::Braille => 80,
            Self::PhaseOfMoon => 120,
            Self::Pulse => 100,
            Self::ForgeDot => 1_400,
            Self::Ember => 180,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_STYLES: [SpinnerStyle; 5] = [
        SpinnerStyle::Braille,
        SpinnerStyle::PhaseOfMoon,
        SpinnerStyle::Pulse,
        SpinnerStyle::ForgeDot,
        SpinnerStyle::Ember,
    ];

    #[test]
    fn default_spinner_is_braille() {
        let style = SpinnerStyle::default();
        assert_eq!(style, SpinnerStyle::Braille);
        assert_eq!(style.frames().len(), 10);
    }

    #[test]
    fn each_variant_has_non_empty_frames() {
        for style in ALL_STYLES {
            assert!(
                !style.frames().is_empty(),
                "{} should have a non-empty frame cycle",
                style.key()
            );
        }
    }

    #[test]
    fn key_round_trips_through_serde() {
        for style in ALL_STYLES {
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
    fn unknown_spinner_key_errors() {
        let result: Result<UiSettings, _> = toml::from_str("launchpad_spinner = \"corkscrew\"\n");
        assert!(result.is_err(), "unknown spinner key should error");
    }

    #[test]
    fn spinner_field_parses_new_key() {
        let parsed: UiSettings = toml::from_str("spinner = \"ember\"\n").expect("parse");
        assert_eq!(parsed.spinner, SpinnerStyle::Ember);
    }

    #[test]
    fn legacy_launchpad_spinner_key_still_parses_via_alias() {
        let parsed: UiSettings = toml::from_str("launchpad_spinner = \"pulse\"\n").expect("parse");
        assert_eq!(parsed.spinner, SpinnerStyle::Pulse);
    }

    #[test]
    fn cadence_ms_is_per_style() {
        // Each style has its own cadence - drift means the launchpad
        // animation no longer ticks at the design-spec frequency.
        assert_eq!(SpinnerStyle::Braille.cadence_ms(), 80);
        assert_eq!(SpinnerStyle::PhaseOfMoon.cadence_ms(), 120);
        assert_eq!(SpinnerStyle::Pulse.cadence_ms(), 100);
        assert_eq!(SpinnerStyle::ForgeDot.cadence_ms(), 1_400);
        assert_eq!(SpinnerStyle::Ember.cadence_ms(), 180);
    }
}
