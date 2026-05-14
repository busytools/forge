//! UI configuration knobs — the `[ui]` section in `forge.toml`.
//!
//! Currently carries the launchpad spinner style. Will grow as the
//! launchpad UI lands. Distinct from the per-session UI state
//! (input editor, viewport, etc.) which lives on `UiSession` in
//! forge-tui — this is purely workspace-level configuration that
//! survives across sessions and processes.

use serde::Deserialize;

/// All `[ui]` section knobs. Every field has a default so an
/// absent `[ui]` section in `forge.toml` is equivalent to all
/// defaults.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct UiSettings {
    /// Spinner style used by the launchpad's "loading projects"
    /// indicator and per-project loading glyph. Distinct from the
    /// in-chat braille spinner so the visual language separates
    /// launchpad context from in-conversation context. Default is
    /// `Braille` — same glyph as everywhere else, ensuring no
    /// surprise for users who don't touch the config.
    #[serde(default)]
    pub launchpad_spinner: SpinnerStyle,
}

/// Spinner glyph cycle used by the launchpad. Each variant carries
/// a `frames()` accessor returning the cycle as `&'static [char]`.
///
/// Glyph choice is intentionally varied — different launchpad
/// personalities should pick visually distinct alternatives so the
/// terminal mode "feels different" from the in-chat braille spinner.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SpinnerStyle {
    /// `⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏` — the default braille spinner used everywhere
    /// else in forge. Familiar; the launchpad reads as continuous
    /// with the rest of the chrome.
    #[default]
    Braille,
    /// `◐◓◑◒` — phase-of-moon rotation. Calm, smooth, distinct
    /// from braille so the launchpad reads as its own surface.
    PhaseOfMoon,
    /// `▘▝▗▖` — quadrant blocks. Crisp, geometric, slightly playful.
    Quadrant,
    /// `◜◝◞◟` — quarter arcs. Minimal; reads as hand-drawn.
    QuarterArc,
    /// `○◔◑◕●◕◑◔○` — pulse fill. Breathing in and out, "alive."
    Pulse,
    /// `⠁⠂⠄⠂` — minimal braille bouncing dot. Quiet, single-cell.
    BouncingDot,
    /// `●` — solid bullet. Intended for a forge-orange intensity
    /// tween at the render layer; the frames are single-glyph so
    /// the animation is driven by colour modulation rather than
    /// glyph changes.
    ForgeDot,
    /// `|/-\` — classic ASCII spinner. Renders even in dumb
    /// terminals without unicode.
    ClassicAscii,
    /// `⠁⠃⠇⡇⡏⡟⡿⠿` — braille fill wave. Directional.
    DotsWave,
}

impl SpinnerStyle {
    /// Frame cycle for this style. Returned slice has fixed-known
    /// length per variant (`braille` = 10, `phase_of_moon` = 4,
    /// etc.) — callers index modulo `len()` per render tick.
    #[must_use]
    pub fn frames(self) -> &'static [char] {
        match self {
            Self::Braille => &[
                '\u{280B}', '\u{2819}', '\u{2839}', '\u{2838}', '\u{283C}', '\u{2834}', '\u{2826}',
                '\u{2827}', '\u{2807}', '\u{280F}',
            ],
            Self::PhaseOfMoon => &['\u{25D0}', '\u{25D3}', '\u{25D1}', '\u{25D2}'],
            Self::Quadrant => &['\u{2598}', '\u{259D}', '\u{2597}', '\u{2596}'],
            Self::QuarterArc => &['\u{25DC}', '\u{25DD}', '\u{25DE}', '\u{25DF}'],
            Self::Pulse => &[
                '\u{25CB}', '\u{25D4}', '\u{25D1}', '\u{25D5}', '\u{25CF}', '\u{25D5}', '\u{25D1}',
                '\u{25D4}',
            ],
            Self::BouncingDot => &['\u{2801}', '\u{2802}', '\u{2804}', '\u{2802}'],
            Self::ForgeDot => &['\u{25CF}'],
            Self::ClassicAscii => &['|', '/', '-', '\\'],
            Self::DotsWave => &[
                '\u{2801}', '\u{2803}', '\u{2807}', '\u{2847}', '\u{284F}', '\u{285F}', '\u{287F}',
                '\u{28FF}',
            ],
        }
    }

    /// Lower-case key used in TOML serde. Matches the `serde
    /// rename_all = "snake_case"` mapping above. Useful for
    /// rendering the current value back to the user (e.g. in help
    /// output or config dump).
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Braille => "braille",
            Self::PhaseOfMoon => "phase_of_moon",
            Self::Quadrant => "quadrant",
            Self::QuarterArc => "quarter_arc",
            Self::Pulse => "pulse",
            Self::BouncingDot => "bouncing_dot",
            Self::ForgeDot => "forge_dot",
            Self::ClassicAscii => "classic_ascii",
            Self::DotsWave => "dots_wave",
        }
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
        for style in [
            SpinnerStyle::Braille,
            SpinnerStyle::PhaseOfMoon,
            SpinnerStyle::Quadrant,
            SpinnerStyle::QuarterArc,
            SpinnerStyle::Pulse,
            SpinnerStyle::BouncingDot,
            SpinnerStyle::ForgeDot,
            SpinnerStyle::ClassicAscii,
            SpinnerStyle::DotsWave,
        ] {
            assert!(
                !style.frames().is_empty(),
                "{} should have a non-empty frame cycle",
                style.key()
            );
        }
    }

    #[test]
    fn key_round_trips_through_serde() {
        // Each variant's TOML key (via `serde rename_all =
        // "snake_case"`) must match its `key()` accessor exactly.
        // If they ever drift, parsing a freshly-written value won't
        // produce the same enum back.
        for style in [
            SpinnerStyle::Braille,
            SpinnerStyle::PhaseOfMoon,
            SpinnerStyle::Quadrant,
            SpinnerStyle::QuarterArc,
            SpinnerStyle::Pulse,
            SpinnerStyle::BouncingDot,
            SpinnerStyle::ForgeDot,
            SpinnerStyle::ClassicAscii,
            SpinnerStyle::DotsWave,
        ] {
            let toml = format!("launchpad_spinner = \"{}\"\n", style.key());
            let parsed: UiSettings = toml::from_str(&toml).expect("parse round trip");
            assert_eq!(parsed.launchpad_spinner, style);
        }
    }

    #[test]
    fn absent_ui_section_yields_defaults() {
        let parsed: UiSettings = toml::from_str("").expect("empty parses");
        assert_eq!(parsed, UiSettings::default());
        assert_eq!(parsed.launchpad_spinner, SpinnerStyle::Braille);
    }

    #[test]
    fn unknown_spinner_key_errors() {
        // Catches typos in the config rather than silently falling
        // back to the default.
        let result: Result<UiSettings, _> = toml::from_str("launchpad_spinner = \"corkscrew\"\n");
        assert!(result.is_err(), "unknown spinner key should error");
    }
}
