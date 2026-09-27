//! The palettes and typefaces forge ships, by the names `[web] theme` and
//! `[web] font` take.
//!
//! Every surface reads these twelve tokens and nothing else, so a theme is
//! one place to change and no component branches for it. The stacks are
//! here for the same reason, and the sheet declares none of its own: the
//! injected block is the only place a typeface is chosen.

/// The palette drawn when no name is set.
pub const DEFAULT_THEME: &str = "dark";

/// The built-in pair, shipped beside the view: Inter is a variable font
/// over `wght 100-900`, so the scale interpolates rather than snapping to
/// a static Bold, and Fira Code carries the two weights the mockups load,
/// its ligatures riding `calt`.
const BUILT_IN_FONT: &str = concat!(
    r#"--ui:"Inter",system-ui,-apple-system,"Segoe UI",sans-serif;"#,
    r#"--mono:"Fira Code",ui-monospace,Menlo,monospace;"#,
);

/// The stacks the OS already has, which is what `system` asks for.
const SYSTEM_FONT: &str = concat!(
    r#"--ui:system-ui,-apple-system,"Segoe UI",sans-serif;"#,
    r#"--mono:ui-monospace,Menlo,monospace;"#,
);

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

/// The page's own font declarations for `name`, in the shape
/// [`root_variables`] emits, so the server can put both in one block.
/// `None` for a name outside the shipped set: the loader refuses one at
/// boot, and a renderer that drew the built-in pair instead would make an
/// ignored name read as the key working.
pub fn font_variables(name: Option<&str>) -> Option<&'static str> {
    match name {
        None => Some(BUILT_IN_FONT),
        Some("system") => Some(SYSTEM_FONT),
        Some(_) => None,
    }
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
    use forge_primitives::web::{FONT_NAMES, THEME_NAMES};

    use super::{DEFAULT_THEME, accent, font_variables, root_variables};

    /// Every token the stylesheet reads is one the palette resolves: a
    /// component reading a token no palette carries renders unstyled, and
    /// the stylesheet is where the reads are.
    ///
    /// The reads come out of the sheet rather than a list written here,
    /// which could only ever fail on a rename inside the palette - the
    /// inverse of what the test is for.
    #[test]
    fn every_token_the_stylesheet_reads_resolves() {
        let sheet = include_str!("web.css");
        // The ones the sheet declares for itself: layout and the type
        // scale, neither of them palette. The font stack is injected beside
        // the palette rather than declared in the sheet, so it resolves
        // through `font_variables`.
        let local = [
            "--r",
            "--cols",
            "--pad",
            "--fs-prose",
            "--fs-base",
            "--fs-group",
            "--fs-data",
            "--fs-label",
            "--fs-title",
            "--ins",
            "--rail-l",
            "--rail-r",
        ];
        let resolved = root_variables(None) + font_variables(None).expect("the built-in pair");

        let mut reads: Vec<&str> = sheet
            .split("var(")
            .skip(1)
            .filter_map(|rest| rest.split([')', ',']).next())
            .map(str::trim)
            .filter(|token| !local.contains(token))
            .collect();
        reads.sort_unstable();
        reads.dedup();

        assert!(!reads.is_empty(), "the sheet reads the palette somewhere");
        for token in reads {
            assert!(
                resolved.contains(&format!("{token}:")),
                "the palette must resolve {token}, which the stylesheet reads",
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

    /// Every name `forge.toml` accepts draws a stack of its own, so a name
    /// added to the shipped list without one cannot quietly render as
    /// nothing or as the built-in pair. The built-in pair has no name of
    /// its own: an unset `font` is it, and the one name shipped is the
    /// opt-out.
    #[test]
    fn every_shipped_font_name_has_its_own_stack() {
        let built_in = font_variables(None);
        for name in FONT_NAMES {
            let stack = font_variables(Some(name));
            assert!(stack.is_some(), "{name} is a name forge.toml accepts but draws no stack");
            assert_ne!(
                stack, built_in,
                "{name} is a name forge.toml accepts but draws the built-in stack",
            );
        }
    }

    /// A name outside the shipped set draws no stack, rather than the
    /// built-in one. The loader refuses such a config at boot, so this is
    /// the renderer's backstop: it must not dress a name nobody ships as
    /// the built-in pair, which is a silent fallback wearing the answer.
    #[test]
    fn a_font_name_outside_the_shipped_set_draws_no_stack() {
        assert!(!FONT_NAMES.contains(&"comic"), "the case is a name forge does not ship");
        assert_eq!(font_variables(Some("comic")), None);
    }

    /// The stack the built-in pair draws with and the faces the sheet
    /// declares name the same families: a family on one side only is
    /// either prose falling back to the OS face with nothing reporting it,
    /// or a file shipped and never asked for.
    #[test]
    fn the_built_in_stack_and_the_sheets_faces_agree() {
        let sheet = include_str!("web.css");
        let declared: Vec<&str> = sheet
            .split("@font-face")
            .skip(1)
            .filter_map(|block| block.split_once("font-family:"))
            .filter_map(|(_, rest)| rest.split(';').next())
            .map(|value| value.trim().trim_matches('"'))
            .collect();
        assert_eq!(declared.len(), 3, "the sheet declares every vendored face: {declared:?}");

        let built_in = font_variables(None).expect("the built-in pair declares a stack");
        let ui =
            built_in.split_once("--ui:").expect("the built-in stack declares the prose face").1;
        let first = ui.split(',').next().expect("a family").trim().trim_matches('"');
        assert!(
            declared.contains(&first),
            "prose is drawn in {first}, which is a face the sheet must declare: {declared:?}",
        );
        for face in &declared {
            assert!(
                built_in.contains(&format!("\"{face}\"")),
                "{face} is declared by the sheet but the built-in stack never asks for it",
            );
        }
    }
}
