//! Contrast over the TUI palette, at the pair level.
//!
//! `client/src/contrast.test.ts` runs this check for the web page. This is the
//! same instrument at the terminal's scale: every pair `theme.rs` picks both
//! sides of, measured at the WCAG ratio for what it draws as, and a guard that
//! no `Color::Rgb` constant arrives in the palette with nobody saying where it
//! is drawn.
//!
//! **What this covers, and what it does not.** Only pairs whose foreground and
//! ground are both constants in `theme.rs`. Every other constant there is
//! inked on the terminal's own canvas, or lays a ground the terminal inks, and
//! what a terminal paints an ANSI colour as is not knowable from here:
//! [`UNFLOORED`] names each one and says which half the terminal owns. Two
//! things sit outside the table the way the client's file cannot see a colour
//! written inline in a component: a `Color::Rgb` written outside `theme.rs`
//! (the diff overlay's comment chip, the dictate ring's literals), and the
//! code panel's body, whose colours come from the syntect theme in
//! `ui/highlight.rs` rather than from this palette.
//!
//! The numbers are exact about the value forge chooses and approximate about
//! what gets painted: ratatui writes `Color::Rgb` as truecolor, and a
//! 256-colour terminal quantises it.

use ratatui::style::Color;

use super::{
    AVAILABLE, CODE_PANEL_BG, CODE_PANEL_LABEL, DIFF_ADDITION_BG, DIFF_DELETION_BG,
    DIFF_FILE_HEADER_BG, EXPERIMENTAL, GOTIFY, REVIEW_ADDRESSED, REVIEW_RESOLVED, RUST_ORANGE,
    SLACK, USER_MSG_BG,
};

/// The WCAG 2.1 AA floor for text. Everything floored here is read as words;
/// the palette draws no mark whose 3:1 floor would differ.
const TEXT: f64 = 4.5;

/// Every `Color::Rgb` constant `theme.rs` declares, under the name the guard
/// matches against the source and the value the ratios are measured from.
const PALETTE: &[(&str, Color)] = &[
    ("RUST_ORANGE", RUST_ORANGE),
    ("USER_MSG_BG", USER_MSG_BG),
    ("CODE_PANEL_BG", CODE_PANEL_BG),
    ("CODE_PANEL_LABEL", CODE_PANEL_LABEL),
    ("AVAILABLE", AVAILABLE),
    ("REVIEW_RESOLVED", REVIEW_RESOLVED),
    ("REVIEW_ADDRESSED", REVIEW_ADDRESSED),
    ("EXPERIMENTAL", EXPERIMENTAL),
    ("GOTIFY", GOTIFY),
    ("SLACK", SLACK),
    ("DIFF_ADDITION_BG", DIFF_ADDITION_BG),
    ("DIFF_DELETION_BG", DIFF_DELETION_BG),
    ("DIFF_FILE_HEADER_BG", DIFF_FILE_HEADER_BG),
];

/// The pairs whose two sides are both constants in this file: the two token
/// names and the floor.
///
/// Two so far - the fence's language label inside the code panel, and the
/// status badge word on a file header's band. A token drawn on the terminal's
/// canvas as well as on a forge ground appears once, on the ground that is
/// forge's: this is the list of pairs that are measured, not a census of every
/// place a colour lands.
const DRAWN: &[(&str, &str, f64)] =
    &[("CODE_PANEL_LABEL", "CODE_PANEL_BG", TEXT), ("RUST_ORANGE", "DIFF_FILE_HEADER_BG", TEXT)];

/// The `Color::Rgb` tokens `theme.rs` declares that no floor is put on, each
/// with the half of the pair the terminal owns.
///
/// An accent token's ground is the terminal's canvas - `Color::Reset`, or
/// whatever a profile paints behind it - and a forge-drawn ground's ink is one
/// of the terminal's ANSI colours. Neither value is knowable from here, so a
/// floor on either would be a number about this machine's terminal profile
/// rather than about forge. Named rather than left out: a token arriving with
/// no row in [`DRAWN`] and no entry here fails the guard below.
const UNFLOORED: &[(&str, &str)] = &[
    ("USER_MSG_BG", "ground of the Projects pane's row buttons, inked Color::Gray"),
    ("DIFF_ADDITION_BG", "ground of an added diff row, inked Color::Green"),
    ("DIFF_DELETION_BG", "ground of a removed diff row, inked Color::Red"),
    ("AVAILABLE", "the extensions row's mark; also a chip ground inked Color::Black"),
    ("REVIEW_RESOLVED", "the RESOLVED label and the completion glyph, on the canvas"),
    ("REVIEW_ADDRESSED", "the ADDRESSED label and the rail dot, on the canvas"),
    ("EXPERIMENTAL", "the /usage view's GPT rows, on the canvas"),
    ("GOTIFY", "the gotify glyph and its source label, on the canvas"),
    ("SLACK", "the slack glyph and its source label, on the canvas"),
];

/// WCAG 2.1 relative luminance of one channel byte.
fn channel(value: u8) -> f64 {
    let c = f64::from(value) / 255.0;
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

/// WCAG 2.1 relative luminance. `None` for a colour that is not one of forge's
/// own `Rgb` values.
fn luminance(color: Color) -> Option<f64> {
    let Color::Rgb(r, g, b) = color else { return None };
    Some(0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b))
}

/// WCAG 2.1 contrast between two colours: 1 at parity, 21 at worst.
fn contrast(a: Color, b: Color) -> Option<f64> {
    let (x, y) = (luminance(a)?, luminance(b)?);
    let (hi, lo) = if x > y { (x, y) } else { (y, x) };
    Some((hi + 0.05) / (lo + 0.05))
}

/// The palette value under a token name.
fn resolve(palette: &[(&str, Color)], name: &str) -> Option<Color> {
    palette.iter().find(|entry| entry.0 == name).map(|entry| entry.1)
}

/// What a failure should say: the pair and, where it was measurable, its
/// ratio.
///
/// A pair the arithmetic cannot run on is reported rather than measured:
/// `None` compares false against every floor, so a value reaching this
/// unmeasured would read as fine. Both halves are real - an ANSI token added
/// to a row, and a name `theme.rs` has renamed.
fn out_of_band(pairs: &[(&str, &str, f64)], palette: &[(&str, Color)]) -> Vec<String> {
    let mut outside = Vec::new();
    for &(foreground, ground, floor) in pairs {
        let (Some(fg), Some(bg)) = (resolve(palette, foreground), resolve(palette, ground)) else {
            outside.push(format!("{foreground} on {ground}: the palette resolves no such token"));
            continue;
        };
        let Some(measured) = contrast(fg, bg) else {
            outside
                .push(format!("{foreground} on {ground}: not a pair of forge-picked Rgb values"));
            continue;
        };
        if measured < floor {
            outside.push(format!(
                "{foreground} on {ground} measures {measured:.2}:1, below the {floor}:1 floor"
            ));
        }
    }
    outside
}

/// The names of the `Color::Rgb` constants a source text declares.
///
/// A constant whose value is another constant (`COMPLETION = REVIEW_RESOLVED`)
/// is not one of these: it names a token the palette already carries.
fn rgb_constants(source: &str) -> Vec<&str> {
    source
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            if !line.starts_with("pub") && !line.starts_with("const") {
                return None;
            }
            let (name, value) = line.split_once("const ")?.1.split_once(':')?;
            value.trim_start().starts_with("Color = Color::Rgb(").then_some(name)
        })
        .collect()
}

/// The accounting failures in `source`: a `Color::Rgb` constant with no entry
/// in `palette`, and an entry for a constant the source no longer declares.
fn unaccounted(source: &str, palette: &[(&str, Color)]) -> Vec<String> {
    let declared = rgb_constants(source);
    let missing =
        declared.iter().filter(|name| !palette.iter().any(|e| e.0 == **name)).map(|name| {
            format!("{name}: a Color::Rgb constant with no entry saying where it is drawn")
        });
    let stale = palette
        .iter()
        .map(|entry| entry.0)
        .filter(|name| !declared.contains(name))
        .map(|name| format!("{name}: an entry for a constant the palette no longer declares"));
    missing.chain(stale).collect()
}

/// The token names the pairs and the exclusions account for.
fn accounted() -> Vec<&'static str> {
    let mut names: Vec<&str> = DRAWN.iter().flat_map(|&(fg, bg, _)| [fg, bg]).collect();
    names.extend(UNFLOORED.iter().map(|&(name, _)| name));
    names
}

/// **The file's control.** A check that reports nothing reads the same whether
/// the palette is clean or the ratio was never computed, so one pair is
/// measured that nothing could read.
#[test]
fn finds_a_pair_too_close_to_read() {
    assert_eq!(
        out_of_band(&[("DIFF_DELETION_BG", "DIFF_FILE_HEADER_BG", TEXT)], PALETTE),
        ["DIFF_DELETION_BG on DIFF_FILE_HEADER_BG measures 1.00:1, below the 4.5:1 floor"]
    );
}

/// A pair naming a token the palette has lost is a stale pair, not a pair
/// without a floor.
#[test]
fn names_a_pair_whose_token_the_palette_lost() {
    assert_eq!(
        out_of_band(&[("GONE", "CODE_PANEL_BG", TEXT)], PALETTE),
        ["GONE on CODE_PANEL_BG: the palette resolves no such token"]
    );
}

/// An ANSI token reaches the arithmetic as no number at all, and a pair
/// reported as unmeasured is not a pair that passed.
#[test]
fn reports_a_pair_it_cannot_measure() {
    let terminal = [("DIM", Color::DarkGray)];
    assert_eq!(
        out_of_band(&[("DIM", "DIM", TEXT)], &terminal),
        ["DIM on DIM: not a pair of forge-picked Rgb values"]
    );
}

/// The guard's own control: a constant the accounting has never seen is
/// reported by name. Without it, the clean result below is not evidence that
/// the source was read at all.
#[test]
fn reports_a_constant_the_accounting_has_not_seen() {
    let source = "pub const KNOWN: Color = Color::Rgb(1, 2, 3);\n\
                  pub const NEW_INK: Color = Color::Rgb(4, 5, 6);\n";
    let palette = [("KNOWN", CODE_PANEL_BG)];
    assert_eq!(
        unaccounted(source, &palette),
        ["NEW_INK: a Color::Rgb constant with no entry saying where it is drawn"]
    );
}

/// The guard's other direction: an entry for a constant `theme.rs` has dropped
/// is stale, rather than a pair whose token quietly stopped being declared.
#[test]
fn reports_an_entry_the_palette_no_longer_declares() {
    assert_eq!(
        unaccounted("", &[("RUST_ORANGE", RUST_ORANGE)]),
        ["RUST_ORANGE: an entry for a constant the palette no longer declares"]
    );
}

/// The guard that keeps the table from going stale the moment the palette
/// changes: a `Color::Rgb` constant arriving in `theme.rs` fails here until
/// someone says where it is drawn, rather than going unchecked.
#[test]
fn pairs_every_rgb_constant_the_palette_declares() {
    let reported = unaccounted(include_str!("../theme.rs"), PALETTE);
    assert!(reported.is_empty(), "constants the accounting does not cover: {reported:?}");
}

/// The guard above says a token is paired somewhere, which is not the same as
/// the pair that matters still being there. This is the other half: every
/// token is named by a pair or an exclusion, and neither list has kept an
/// entry the palette has dropped.
#[test]
fn names_every_token_where_it_is_drawn() {
    let accounted = accounted();
    let missing: Vec<&str> =
        PALETTE.iter().map(|e| e.0).filter(|name| !accounted.contains(name)).collect();
    assert!(missing.is_empty(), "palette tokens with no row and no exclusion: {missing:?}");

    let orphaned: Vec<&str> =
        accounted.iter().copied().filter(|name| resolve(PALETTE, name).is_none()).collect();
    assert!(orphaned.is_empty(), "rows naming a token the palette does not carry: {orphaned:?}");
}

#[test]
fn draws_every_pair_it_names_inside_its_floor() {
    let reported = out_of_band(DRAWN, PALETTE);
    assert!(reported.is_empty(), "pairs outside their floor: {reported:?}");
}
