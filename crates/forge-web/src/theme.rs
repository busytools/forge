//! The palettes forge ships, by the name `[web] theme` takes.
//!
//! Every surface reads these twelve tokens and nothing else, so a theme is
//! one place to change and no component branches for it.

/// The palette drawn when no name is set.
pub const DEFAULT_THEME: &str = "dark";

const BG: &str = "#04050a";
const S1: &str = "#0d111a";
const S2: &str = "#141926";
const S3: &str = "#1b2130";
const LINE: &str = "#222a3c";
const TEXT: &str = "#eaeef6";
const MUTED: &str = "#8f98a8";
const DIM: &str = "#5d6675";
const ACCENT: &str = "#f47600";
const OK: &str = "#82c76b";
const WARN: &str = "#c9a13b";
const BAD: &str = "#e0683e";

/// Every token the dark palette resolves, in the order they are emitted.
const DARK: &[(&str, &str)] = &[
    ("--bg", BG),
    ("--s1", S1),
    ("--s2", S2),
    ("--s3", S3),
    ("--line", LINE),
    ("--text", TEXT),
    ("--muted", MUTED),
    ("--dim", DIM),
    ("--accent", ACCENT),
    ("--ok", OK),
    ("--warn", WARN),
    ("--bad", BAD),
];

/// The page's `:root` declarations for `name`.
pub fn root_variables(name: Option<&str>) -> String {
    let mut out = String::new();
    for (key, value) in tokens(name) {
        out.push_str(key);
        out.push(':');
        out.push_str(value);
        out.push(';');
    }
    out
}

/// The palette's accent, for the places with no cascade to inherit
/// `currentColor` from - a favicon is read as a standalone document.
pub fn accent(_name: Option<&str>) -> &'static str {
    // Dark is the only palette shipped, so every name draws it. A second
    // palette joins here, and `THEME_NAMES` with it.
    ACCENT
}

fn tokens(_theme: Option<&str>) -> &'static [(&'static str, &'static str)] {
    // Dark is the only palette shipped, so every name draws it - including
    // one outside `THEME_NAMES`, which the loader refuses at boot. A second
    // palette joins here.
    DARK
}

#[cfg(test)]
mod tests {
    use forge_primitives::web::THEME_NAMES;

    use super::{DEFAULT_THEME, accent, root_variables};

    /// Every token a surface reads is emitted, so a component that reads
    /// one the palette forgot renders unstyled rather than loudly.
    #[test]
    fn the_root_block_carries_every_token() {
        let variables = root_variables(None);

        for token in [
            "--bg", "--s1", "--s2", "--s3", "--line", "--text", "--muted", "--dim", "--accent",
            "--ok", "--warn", "--bad",
        ] {
            assert!(
                variables.contains(&format!("{token}:")),
                "the palette must resolve {token}, got: {variables}",
            );
        }
    }

    /// The favicon's colour is the same accent the page declares, so a
    /// palette change moves both.
    #[test]
    fn the_accent_is_the_one_the_root_block_declares() {
        assert!(
            root_variables(None).contains(&format!("--accent:{};", accent(None))),
            "the standalone accent must be the palette's own",
        );
    }

    /// An unset name and the built-in's own name draw the same palette,
    /// and the built-in is one the shipped list offers.
    #[test]
    fn the_built_in_palette_is_an_unset_name() {
        assert_eq!(root_variables(None), root_variables(Some(DEFAULT_THEME)));
        assert!(
            THEME_NAMES.contains(&DEFAULT_THEME),
            "the built-in has to be a name forge.toml accepts, or nothing could name it",
        );
    }
}
