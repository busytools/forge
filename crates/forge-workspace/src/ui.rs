//! UI configuration knobs - the `[ui]` section in `forge.toml`.
//!
//! Currently carries the active spinner style.
//! Distinct from per-session UI state (input editor, viewport, etc.)
//! which lives on `UiSession` in forge-tui - this is workspace-level
//! configuration that survives across sessions and processes.

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

/// Lenient deserialize for an optional persisted spinner key (the
/// forge-state.toml sidecar override): an unknown/removed key maps to
/// `None` so the boot resolve order falls through to the config default
/// rather than failing the whole state load.
pub fn deserialize_lenient_opt<'de, D>(deserializer: D) -> Result<Option<SpinnerStyle>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(deserializer)?;
    Ok(opt.and_then(|key| SpinnerStyle::from_key(&key)))
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

    /// Per-style frame cadence in milliseconds. Every spinner surface
    /// derives the current frame index as `elapsed_ms / cadence_ms`
    /// (modulo the frame count), so each style animates at its own
    /// speed. Braille is the fast default (30ms, one frame per redraw
    /// tick); the rest are tuned per glyph set.
    pub fn cadence_ms(self) -> u64 {
        match self {
            Self::Braille => 30,
            Self::PhaseOfMoon => 90,
            Self::Ember => 160,
            Self::BarsV => 70,
            Self::Star => 130,
            Self::Sparkle => 160,
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
        let typo: UiSettings =
            toml::from_str("spinner = \"corkscrew\"\n").expect("typo parses");
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
        assert_eq!(SpinnerStyle::Braille.cadence_ms(), 30);
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
    fn revised_set_drops_pulse_forgedot_adds_new() {
        assert_eq!(SpinnerStyle::BarsV.frames().len(), 14);
        assert_eq!(SpinnerStyle::Star.key(), "star");
        assert_eq!(SpinnerStyle::from_key("sparkle"), Some(SpinnerStyle::Sparkle));
        assert_eq!(SpinnerStyle::from_key("pulse"), None);
        assert_eq!(SpinnerStyle::from_key("forge_dot"), None);
    }
}
