//! Slack-style `:shortcode:` emoji typeahead, shared by every text
//! input via [`App::focused_input`].
//!
//! Trigger rule mirrors [`super::mention`]: the `:` only counts at the
//! start of a line or after whitespace, and the query has to look like a
//! shortcode. Without that, `http://`, `10:30` and `note:` would all pop
//! a picker.

use super::{App, FocusTarget, dialog::DialogState};

/// Max candidates shown in the dropdown. The list is dense (one glyph +
/// one short name per row), so a shorter window than the file picker's
/// keeps it from swallowing the pane.
pub const MAX_VISIBLE: usize = 10;

/// Characters after the `:` before the picker opens. One is too eager -
/// `:D` and a bare `: ` would both pop a dropdown mid-sentence.
pub const MIN_QUERY_CHARS: usize = 2;

pub struct EmojiState {
    /// Character position (row, col) of the `:` that opened the picker.
    pub trigger_row: usize,
    pub trigger_col: usize,
    /// Query text after the `:` (e.g. "roc" from ":roc").
    pub query: String,
    /// Ranked matches for `query`.
    pub candidates: Vec<&'static Emoji>,
    /// Shared autocomplete dialog navigation state.
    pub dialog: DialogState,
}

/// One shortcode / glyph pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Emoji {
    /// GitHub / Slack shortcode without the surrounding colons.
    pub name: &'static str,
    pub glyph: &'static str,
}

impl EmojiState {
    pub fn has_selectable_candidates(&self) -> bool {
        !self.candidates.is_empty()
    }

    pub fn selected(&self) -> Option<&'static Emoji> {
        self.candidates.get(self.dialog.selected).copied()
    }
}

/// Whether `c` can appear in a shortcode. GitHub's own set is
/// `[a-z0-9_+-]`; anything else means the token isn't a shortcode and the
/// picker must stay shut.
fn is_shortcode_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '+' | '-')
}

/// Detect a `:shortcode` token at the cursor. Scans back to the `:`,
/// which must sit at column 0 or directly after whitespace. Returns
/// `(trigger_row, trigger_col, query)` with `trigger_col` at the `:`.
pub fn detect_emoji_at_cursor(
    lines: &[String],
    cursor_row: usize,
    cursor_col: usize,
) -> Option<(usize, usize, String)> {
    let line = lines.get(cursor_row)?;
    let chars: Vec<char> = line.chars().collect();
    // Defensive clamp - matches `mention::detect_mention_at_cursor`; the
    // slice below would panic if cursor_col ever exceeded chars.len().
    let cursor_col = cursor_col.min(chars.len());

    let mut i = cursor_col;
    while i > 0 {
        i -= 1;
        let ch = *chars.get(i)?;
        if ch == ':' {
            if i > 0 && !chars.get(i - 1).is_some_and(|c| c.is_whitespace()) {
                return None;
            }
            let query: String = chars[i + 1..cursor_col].iter().collect();
            if query.chars().all(is_shortcode_char) {
                return Some((cursor_row, i, query));
            }
            return None;
        }
        if !is_shortcode_char(ch) {
            return None;
        }
    }
    None
}

/// Rank matches for `query`: exact first, then shortcodes that start
/// with it, then the rest of the substring matches. Ties break
/// alphabetically, which the table's own ordering already provides.
pub fn matches(query: &str) -> Vec<&'static Emoji> {
    if query.chars().count() < MIN_QUERY_CHARS {
        return Vec::new();
    }
    let mut scored: Vec<(u8, &'static Emoji)> = TABLE
        .iter()
        .filter_map(|emoji| {
            let rank = if emoji.name == query {
                0
            } else if emoji.name.starts_with(query) {
                1
            } else if emoji.name.contains(query) {
                2
            } else {
                return None;
            };
            Some((rank, emoji))
        })
        .collect();
    scored.sort_by_key(|(rank, emoji)| (*rank, emoji.name));
    scored.truncate(crate::app::MAX_CANDIDATES);
    scored.into_iter().map(|(_, emoji)| emoji).collect()
}

/// Look up an exact shortcode. Backs the closing-colon shorthand so
/// typing `:tada:` straight through lands the glyph.
pub fn exact(name: &str) -> Option<&'static Emoji> {
    TABLE.iter().find(|emoji| emoji.name == name)
}

/// Open the picker if the cursor sits in a `:shortcode` token.
pub fn activate(app: &mut App) {
    let Some((trigger_row, trigger_col, query)) = detect_at_focused_cursor(app) else {
        return;
    };
    let candidates = matches(&query);
    app.emoji = Some(EmojiState {
        trigger_row,
        trigger_col,
        query,
        candidates,
        dialog: DialogState::default(),
    });
    sync_focus(app);
}

/// Re-filter while the picker is open, closing it once the token breaks.
pub fn update_query(app: &mut App) {
    let Some((trigger_row, trigger_col, query)) = detect_at_focused_cursor(app) else {
        deactivate(app);
        return;
    };
    let candidates = matches(&query);
    if let Some(emoji) = app.emoji.as_mut() {
        emoji.trigger_row = trigger_row;
        emoji.trigger_col = trigger_col;
        emoji.query = query;
        emoji.candidates = candidates;
        emoji.dialog.clamp(emoji.candidates.len(), MAX_VISIBLE);
    }
    sync_focus(app);
}

/// Keep picker state in step with the cursor, the way
/// [`super::mention::sync_with_cursor`] does.
pub fn sync_with_cursor(app: &mut App) {
    let in_token = detect_at_focused_cursor(app).is_some();
    match (in_token, app.emoji.is_some()) {
        (true, true) => update_query(app),
        (true, false) => activate(app),
        (false, true) => deactivate(app),
        (false, false) => {}
    }
}

fn detect_at_focused_cursor(app: &App) -> Option<(usize, usize, String)> {
    let input = app.focused_input()?;
    detect_emoji_at_cursor(input.lines(), input.cursor_row(), input.cursor_col())
}

fn sync_focus(app: &mut App) {
    if app.emoji.as_ref().is_some_and(EmojiState::has_selectable_candidates) {
        app.claim_focus_target(FocusTarget::Emoji);
    } else {
        app.release_focus_target(FocusTarget::Emoji);
    }
}

/// Replace the whole `:query` token with the selected glyph, leaving the
/// cursor after it so typing continues.
pub fn confirm_selection(app: &mut App) {
    let Some(state) = app.emoji.take() else {
        return;
    };
    app.release_focus_target(FocusTarget::Emoji);
    let Some(emoji) = state.candidates.get(state.dialog.selected).copied() else {
        return;
    };
    replace_token(app, state.trigger_row, state.trigger_col, emoji.glyph);
}

/// Swap the token starting at `trigger_col` for `glyph`. The token runs
/// to the end of the shortcode characters plus one optional closing `:`,
/// so both `:roc` + Enter and a typed-through `:rocket:` collapse to the
/// glyph with nothing left over.
fn replace_token(app: &mut App, trigger_row: usize, trigger_col: usize, glyph: &str) {
    let Some(input) = app.focused_input() else {
        return;
    };
    let mut lines = input.lines().to_vec();
    let Some(line) = lines.get(trigger_row) else {
        return;
    };
    let chars: Vec<char> = line.chars().collect();
    if chars.get(trigger_col) != Some(&':') {
        return;
    }

    let mut end = trigger_col + 1;
    while end < chars.len() && is_shortcode_char(chars[end]) {
        end += 1;
    }
    if chars.get(end) == Some(&':') {
        end += 1;
    }

    let before: String = chars[..trigger_col].iter().collect();
    let after: String = chars[end..].iter().collect();
    lines[trigger_row] = format!("{before}{glyph}{after}");
    let new_cursor_col = trigger_col + glyph.chars().count();
    if let Some(input) = app.focused_input_mut() {
        input.replace_lines_and_cursor(lines, trigger_row, new_cursor_col);
    }
}

/// Handle the closing `:` of a typed-through shortcode: with an exact
/// match, insert the glyph instead of the colon. Returns `true` when the
/// colon was consumed.
pub fn try_close_shortcode(app: &mut App) -> bool {
    let Some(state) = app.emoji.as_ref() else {
        return false;
    };
    let Some(emoji) = exact(&state.query) else {
        return false;
    };
    let (trigger_row, trigger_col) = (state.trigger_row, state.trigger_col);
    app.emoji = None;
    app.release_focus_target(FocusTarget::Emoji);
    replace_token(app, trigger_row, trigger_col, emoji.glyph);
    true
}

pub fn deactivate(app: &mut App) {
    app.emoji = None;
    app.release_focus_target(FocusTarget::Emoji);
}

pub fn move_up(app: &mut App) {
    if let Some(emoji) = app.emoji.as_mut() {
        emoji.dialog.move_up(emoji.candidates.len(), MAX_VISIBLE);
    }
}

pub fn move_down(app: &mut App) {
    if let Some(emoji) = app.emoji.as_mut() {
        emoji.dialog.move_down(emoji.candidates.len(), MAX_VISIBLE);
    }
}

/// Shortcodes people already know, from the GitHub / Slack naming that
/// both surfaces share. Deliberately a curated set rather than the full
/// Unicode emoji list: the long tail is never scrolled to in a
/// typeahead, and a crate carrying every sequence plus its metadata is
/// several hundred KB of generated tables for a cosmetic feature.
///
/// Sorted by `name` so ranking ties break alphabetically for free and
/// additions land as a readable one-line diff.
#[rustfmt::skip]
static TABLE: &[Emoji] = &[
    Emoji { name: "+1", glyph: "\u{1F44D}" },
    Emoji { name: "-1", glyph: "\u{1F44E}" },
    Emoji { name: "100", glyph: "\u{1F4AF}" },
    Emoji { name: "alarm_clock", glyph: "\u{23F0}" },
    Emoji { name: "alien", glyph: "\u{1F47D}" },
    Emoji { name: "anchor", glyph: "\u{2693}" },
    Emoji { name: "angry", glyph: "\u{1F620}" },
    Emoji { name: "art", glyph: "\u{1F3A8}" },
    Emoji { name: "astonished", glyph: "\u{1F632}" },
    Emoji { name: "avocado", glyph: "\u{1F951}" },
    Emoji { name: "balloon", glyph: "\u{1F388}" },
    Emoji { name: "banana", glyph: "\u{1F34C}" },
    Emoji { name: "bar_chart", glyph: "\u{1F4CA}" },
    Emoji { name: "battery", glyph: "\u{1F50B}" },
    Emoji { name: "beer", glyph: "\u{1F37A}" },
    Emoji { name: "bell", glyph: "\u{1F514}" },
    Emoji { name: "birthday", glyph: "\u{1F382}" },
    Emoji { name: "blush", glyph: "\u{1F60A}" },
    Emoji { name: "bomb", glyph: "\u{1F4A3}" },
    Emoji { name: "book", glyph: "\u{1F4D5}" },
    Emoji { name: "bookmark", glyph: "\u{1F516}" },
    Emoji { name: "books", glyph: "\u{1F4DA}" },
    Emoji { name: "boom", glyph: "\u{1F4A5}" },
    Emoji { name: "brain", glyph: "\u{1F9E0}" },
    Emoji { name: "broom", glyph: "\u{1F9F9}" },
    Emoji { name: "bug", glyph: "\u{1F41B}" },
    Emoji { name: "building_construction", glyph: "\u{1F3D7}" },
    Emoji { name: "bulb", glyph: "\u{1F4A1}" },
    Emoji { name: "cake", glyph: "\u{1F370}" },
    Emoji { name: "calendar", glyph: "\u{1F4C5}" },
    Emoji { name: "camera", glyph: "\u{1F4F7}" },
    Emoji { name: "candle", glyph: "\u{1F56F}" },
    Emoji { name: "cat", glyph: "\u{1F431}" },
    Emoji { name: "chart_with_downwards_trend", glyph: "\u{1F4C9}" },
    Emoji { name: "chart_with_upwards_trend", glyph: "\u{1F4C8}" },
    Emoji { name: "check", glyph: "\u{2714}" },
    Emoji { name: "cherry_blossom", glyph: "\u{1F338}" },
    Emoji { name: "clap", glyph: "\u{1F44F}" },
    Emoji { name: "clipboard", glyph: "\u{1F4CB}" },
    Emoji { name: "clock", glyph: "\u{1F550}" },
    Emoji { name: "cloud", glyph: "\u{2601}" },
    Emoji { name: "coffee", glyph: "\u{2615}" },
    Emoji { name: "cold_sweat", glyph: "\u{1F630}" },
    Emoji { name: "computer", glyph: "\u{1F4BB}" },
    Emoji { name: "confetti_ball", glyph: "\u{1F38F}" },
    Emoji { name: "confused", glyph: "\u{1F615}" },
    Emoji { name: "construction", glyph: "\u{1F6A7}" },
    Emoji { name: "cookie", glyph: "\u{1F36A}" },
    Emoji { name: "cow", glyph: "\u{1F42E}" },
    Emoji { name: "crab", glyph: "\u{1F980}" },
    Emoji { name: "crossed_fingers", glyph: "\u{1F91E}" },
    Emoji { name: "cry", glyph: "\u{1F622}" },
    Emoji { name: "dancer", glyph: "\u{1F483}" },
    Emoji { name: "dart", glyph: "\u{1F3AF}" },
    Emoji { name: "dizzy", glyph: "\u{1F4AB}" },
    Emoji { name: "dog", glyph: "\u{1F436}" },
    Emoji { name: "door", glyph: "\u{1F6AA}" },
    Emoji { name: "dragon", glyph: "\u{1F409}" },
    Emoji { name: "drum", glyph: "\u{1F941}" },
    Emoji { name: "ear", glyph: "\u{1F442}" },
    Emoji { name: "earth_americas", glyph: "\u{1F30E}" },
    Emoji { name: "eggplant", glyph: "\u{1F346}" },
    Emoji { name: "envelope", glyph: "\u{2709}" },
    Emoji { name: "exploding_head", glyph: "\u{1F92F}" },
    Emoji { name: "eyes", glyph: "\u{1F440}" },
    Emoji { name: "facepalm", glyph: "\u{1F926}" },
    Emoji { name: "fire", glyph: "\u{1F525}" },
    Emoji { name: "fireworks", glyph: "\u{1F386}" },
    Emoji { name: "fish", glyph: "\u{1F41F}" },
    Emoji { name: "floppy_disk", glyph: "\u{1F4BE}" },
    Emoji { name: "flushed", glyph: "\u{1F633}" },
    Emoji { name: "fox", glyph: "\u{1F98A}" },
    Emoji { name: "frowning", glyph: "\u{1F641}" },
    Emoji { name: "gear", glyph: "\u{2699}" },
    Emoji { name: "gem", glyph: "\u{1F48E}" },
    Emoji { name: "ghost", glyph: "\u{1F47B}" },
    Emoji { name: "gift", glyph: "\u{1F381}" },
    Emoji { name: "globe_with_meridians", glyph: "\u{1F310}" },
    Emoji { name: "grimacing", glyph: "\u{1F62C}" },
    Emoji { name: "grin", glyph: "\u{1F601}" },
    Emoji { name: "hammer", glyph: "\u{1F528}" },
    Emoji { name: "hammer_and_wrench", glyph: "\u{1F6E0}" },
    Emoji { name: "handshake", glyph: "\u{1F91D}" },
    Emoji { name: "hankey", glyph: "\u{1F4A9}" },
    Emoji { name: "heart", glyph: "\u{2764}" },
    Emoji { name: "heart_eyes", glyph: "\u{1F60D}" },
    Emoji { name: "hearts", glyph: "\u{1F495}" },
    Emoji { name: "hourglass", glyph: "\u{231B}" },
    Emoji { name: "house", glyph: "\u{1F3E0}" },
    Emoji { name: "hugs", glyph: "\u{1F917}" },
    Emoji { name: "ice_cube", glyph: "\u{1F9CA}" },
    Emoji { name: "inbox_tray", glyph: "\u{1F4E5}" },
    Emoji { name: "information_source", glyph: "\u{2139}" },
    Emoji { name: "joy", glyph: "\u{1F602}" },
    Emoji { name: "key", glyph: "\u{1F511}" },
    Emoji { name: "keyboard", glyph: "\u{2328}" },
    Emoji { name: "label", glyph: "\u{1F3F7}" },
    Emoji { name: "laptop", glyph: "\u{1F4BB}" },
    Emoji { name: "leaves", glyph: "\u{1F343}" },
    Emoji { name: "lemon", glyph: "\u{1F34B}" },
    Emoji { name: "link", glyph: "\u{1F517}" },
    Emoji { name: "lock", glyph: "\u{1F512}" },
    Emoji { name: "loudspeaker", glyph: "\u{1F4E2}" },
    Emoji { name: "mag", glyph: "\u{1F50D}" },
    Emoji { name: "mailbox", glyph: "\u{1F4EB}" },
    Emoji { name: "medal", glyph: "\u{1F3C5}" },
    Emoji { name: "megaphone", glyph: "\u{1F4E3}" },
    Emoji { name: "memo", glyph: "\u{1F4DD}" },
    Emoji { name: "microscope", glyph: "\u{1F52C}" },
    Emoji { name: "money_with_wings", glyph: "\u{1F4B8}" },
    Emoji { name: "monkey", glyph: "\u{1F412}" },
    Emoji { name: "moon", glyph: "\u{1F319}" },
    Emoji { name: "mouse", glyph: "\u{1F42D}" },
    Emoji { name: "muscle", glyph: "\u{1F4AA}" },
    Emoji { name: "mushroom", glyph: "\u{1F344}" },
    Emoji { name: "musical_note", glyph: "\u{1F3B5}" },
    Emoji { name: "nail_care", glyph: "\u{1F485}" },
    Emoji { name: "neutral_face", glyph: "\u{1F610}" },
    Emoji { name: "no_entry", glyph: "\u{26D4}" },
    Emoji { name: "notebook", glyph: "\u{1F4D3}" },
    Emoji { name: "octopus", glyph: "\u{1F419}" },
    Emoji { name: "ok_hand", glyph: "\u{1F44C}" },
    Emoji { name: "open_file_folder", glyph: "\u{1F4C2}" },
    Emoji { name: "outbox_tray", glyph: "\u{1F4E4}" },
    Emoji { name: "owl", glyph: "\u{1F989}" },
    Emoji { name: "package", glyph: "\u{1F4E6}" },
    Emoji { name: "page_facing_up", glyph: "\u{1F4C4}" },
    Emoji { name: "paperclip", glyph: "\u{1F4CE}" },
    Emoji { name: "parrot", glyph: "\u{1F99C}" },
    Emoji { name: "party_popper", glyph: "\u{1F389}" },
    Emoji { name: "peach", glyph: "\u{1F351}" },
    Emoji { name: "pencil", glyph: "\u{270F}" },
    Emoji { name: "penguin", glyph: "\u{1F427}" },
    Emoji { name: "phone", glyph: "\u{1F4DE}" },
    Emoji { name: "pig", glyph: "\u{1F437}" },
    Emoji { name: "pill", glyph: "\u{1F48A}" },
    Emoji { name: "pizza", glyph: "\u{1F355}" },
    Emoji { name: "point_down", glyph: "\u{1F447}" },
    Emoji { name: "point_left", glyph: "\u{1F448}" },
    Emoji { name: "point_right", glyph: "\u{1F449}" },
    Emoji { name: "point_up", glyph: "\u{1F446}" },
    Emoji { name: "poop", glyph: "\u{1F4A9}" },
    Emoji { name: "pray", glyph: "\u{1F64F}" },
    Emoji { name: "punch", glyph: "\u{1F44A}" },
    Emoji { name: "pushpin", glyph: "\u{1F4CC}" },
    Emoji { name: "question", glyph: "\u{2753}" },
    Emoji { name: "rabbit", glyph: "\u{1F430}" },
    Emoji { name: "rainbow", glyph: "\u{1F308}" },
    Emoji { name: "raised_hands", glyph: "\u{1F64C}" },
    Emoji { name: "recycle", glyph: "\u{267B}" },
    Emoji { name: "robot", glyph: "\u{1F916}" },
    Emoji { name: "rocket", glyph: "\u{1F680}" },
    Emoji { name: "rofl", glyph: "\u{1F923}" },
    Emoji { name: "rose", glyph: "\u{1F339}" },
    Emoji { name: "sailboat", glyph: "\u{26F5}" },
    Emoji { name: "salt", glyph: "\u{1F9C2}" },
    Emoji { name: "satellite", glyph: "\u{1F4E1}" },
    Emoji { name: "scissors", glyph: "\u{2702}" },
    Emoji { name: "scroll", glyph: "\u{1F4DC}" },
    Emoji { name: "seedling", glyph: "\u{1F331}" },
    Emoji { name: "shark", glyph: "\u{1F988}" },
    Emoji { name: "shield", glyph: "\u{1F6E1}" },
    Emoji { name: "shipit", glyph: "\u{1F6A2}" },
    Emoji { name: "shrug", glyph: "\u{1F937}" },
    Emoji { name: "skull", glyph: "\u{1F480}" },
    Emoji { name: "sleeping", glyph: "\u{1F634}" },
    Emoji { name: "slightly_smiling_face", glyph: "\u{1F642}" },
    Emoji { name: "smile", glyph: "\u{1F604}" },
    Emoji { name: "smiley", glyph: "\u{1F603}" },
    Emoji { name: "smirk", glyph: "\u{1F60F}" },
    Emoji { name: "snail", glyph: "\u{1F40C}" },
    Emoji { name: "snake", glyph: "\u{1F40D}" },
    Emoji { name: "snowflake", glyph: "\u{2744}" },
    Emoji { name: "sob", glyph: "\u{1F62D}" },
    Emoji { name: "sos", glyph: "\u{1F198}" },
    Emoji { name: "sparkles", glyph: "\u{2728}" },
    Emoji { name: "speech_balloon", glyph: "\u{1F4AC}" },
    Emoji { name: "star", glyph: "\u{2B50}" },
    Emoji { name: "stopwatch", glyph: "\u{23F1}" },
    Emoji { name: "sunglasses", glyph: "\u{1F60E}" },
    Emoji { name: "sunny", glyph: "\u{2600}" },
    Emoji { name: "sweat_smile", glyph: "\u{1F605}" },
    Emoji { name: "tada", glyph: "\u{1F389}" },
    Emoji { name: "telescope", glyph: "\u{1F52D}" },
    Emoji { name: "test_tube", glyph: "\u{1F9EA}" },
    Emoji { name: "thinking", glyph: "\u{1F914}" },
    Emoji { name: "thread", glyph: "\u{1F9F5}" },
    Emoji { name: "thumbsdown", glyph: "\u{1F44E}" },
    Emoji { name: "thumbsup", glyph: "\u{1F44D}" },
    Emoji { name: "toolbox", glyph: "\u{1F9F0}" },
    Emoji { name: "trophy", glyph: "\u{1F3C6}" },
    Emoji { name: "turtle", glyph: "\u{1F422}" },
    Emoji { name: "unicorn", glyph: "\u{1F984}" },
    Emoji { name: "unlock", glyph: "\u{1F513}" },
    Emoji { name: "upside_down_face", glyph: "\u{1F643}" },
    Emoji { name: "volcano", glyph: "\u{1F30B}" },
    Emoji { name: "warning", glyph: "\u{26A0}" },
    Emoji { name: "wastebasket", glyph: "\u{1F5D1}" },
    Emoji { name: "wave", glyph: "\u{1F44B}" },
    Emoji { name: "whale", glyph: "\u{1F433}" },
    Emoji { name: "wheelchair", glyph: "\u{267F}" },
    Emoji { name: "wink", glyph: "\u{1F609}" },
    Emoji { name: "wolf", glyph: "\u{1F43A}" },
    Emoji { name: "wrench", glyph: "\u{1F527}" },
    Emoji { name: "x", glyph: "\u{274C}" },
    Emoji { name: "yarn", glyph: "\u{1F9F6}" },
    Emoji { name: "zany_face", glyph: "\u{1F92A}" },
    Emoji { name: "zap", glyph: "\u{26A1}" },
    Emoji { name: "zzz", glyph: "\u{1F4A4}" },
];

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn detect(text: &str) -> Option<(usize, usize, String)> {
        let lines = vec![text.to_owned()];
        detect_emoji_at_cursor(&lines, 0, text.chars().count())
    }

    #[test]
    fn table_is_sorted_and_has_no_duplicate_shortcodes() {
        let names: Vec<&str> = TABLE.iter().map(|e| e.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "keep TABLE sorted by name");
        let mut deduped = sorted.clone();
        deduped.dedup();
        assert_eq!(sorted.len(), deduped.len(), "duplicate shortcode in TABLE");
    }

    #[test]
    fn every_shortcode_uses_the_documented_charset() {
        for emoji in TABLE {
            assert!(
                emoji.name.chars().all(is_shortcode_char),
                "{} is unreachable - detection rejects its characters",
                emoji.name
            );
            assert!(!emoji.glyph.is_empty(), "{} has no glyph", emoji.name);
        }
    }

    #[test]
    fn triggers_at_start_of_input_and_after_whitespace() {
        assert_eq!(detect(":roc"), Some((0, 0, "roc".to_owned())));
        assert_eq!(detect("ship :roc"), Some((0, 5, "roc".to_owned())));
        assert_eq!(detect("a\t:roc"), Some((0, 2, "roc".to_owned())));
    }

    /// The rule that keeps every URL from opening a picker.
    #[test]
    fn does_not_trigger_mid_word() {
        assert_eq!(detect("http://example.com"), None);
        assert_eq!(detect("https://x"), None);
        assert_eq!(detect("note:todo"), None);
        assert_eq!(detect("10:30"), None);
        assert_eq!(detect("Foo::bar"), None);
    }

    #[test]
    fn does_not_trigger_on_non_shortcode_queries() {
        assert_eq!(detect(":Rocket"), None, "uppercase is not a shortcode char");
        assert_eq!(detect(":ro ck"), None, "whitespace breaks the token");
        assert_eq!(detect(":ro.ck"), None);
    }

    #[test]
    fn empty_and_short_queries_yield_no_candidates() {
        assert_eq!(detect(":"), Some((0, 0, String::new())));
        assert!(matches("").is_empty());
        assert!(matches("r").is_empty(), "one char is below MIN_QUERY_CHARS");
        assert!(!matches("ro").is_empty());
    }

    #[test]
    fn ranking_puts_exact_then_prefix_ahead_of_substring() {
        let ranked = matches("check");
        assert_eq!(ranked.first().map(|e| e.name), Some("check"), "exact match leads");

        let ranked = matches("rocket");
        assert_eq!(ranked.first().map(|e| e.name), Some("rocket"));

        let ranked = matches("art");
        let names: Vec<&str> = ranked.iter().map(|e| e.name).collect();
        let art_pos = names.iter().position(|n| *n == "art").expect("art matches");
        let heart_pos = names.iter().position(|n| *n == "heart").expect("heart contains art");
        assert!(art_pos < heart_pos, "prefix match ranks ahead of substring: {names:?}");
    }

    #[test]
    fn exact_resolves_known_shortcodes_only() {
        assert_eq!(exact("tada").map(|e| e.glyph), Some("\u{1F389}"));
        assert_eq!(exact("definitely_not_an_emoji"), None);
    }

    /// Type into the chat draft at human speed - see the note on the
    /// /diff overlay's `type_text` helper for why the timing reset matters.
    fn type_into_chat(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.paste_burst.on_non_char_key(std::time::Instant::now());
            crate::app::events::handle_terminal_event(
                app,
                crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Char(ch),
                    crossterm::event::KeyModifiers::NONE,
                )),
            );
        }
    }

    fn press(app: &mut App, code: crossterm::event::KeyCode) {
        crate::app::events::handle_terminal_event(
            app,
            crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
                code,
                crossterm::event::KeyModifiers::NONE,
            )),
        );
    }

    #[test]
    fn chat_enter_confirms_the_emoji_instead_of_submitting() {
        let mut app = App::test_default();
        type_into_chat(&mut app, "ship it :rocket");
        assert!(app.emoji.is_some(), "picker is open over the chat draft");

        press(&mut app, crossterm::event::KeyCode::Enter);

        assert!(app.emoji.is_none());
        assert_eq!(app.input().text(), "ship it \u{1F680}");
        assert!(app.pending_submit().is_none(), "Enter on the picker must not arm a prompt submit");
    }

    #[test]
    fn chat_typing_continues_after_confirming() {
        let mut app = App::test_default();
        type_into_chat(&mut app, ":tada");
        press(&mut app, crossterm::event::KeyCode::Enter);
        type_into_chat(&mut app, " ship");

        assert_eq!(app.input().text(), "\u{1F389} ship");
    }

    #[test]
    fn chat_esc_dismisses_the_picker_and_keeps_the_draft() {
        let mut app = App::test_default();
        type_into_chat(&mut app, "hi :roc");

        press(&mut app, crossterm::event::KeyCode::Esc);

        assert!(app.emoji.is_none());
        assert_eq!(app.input().text(), "hi :roc", "the typed token survives");
    }

    #[test]
    fn chat_backspacing_out_of_the_token_closes_the_picker() {
        let mut app = App::test_default();
        type_into_chat(&mut app, ":roc");
        assert!(app.emoji.is_some());

        for _ in 0..4 {
            press(&mut app, crossterm::event::KeyCode::Backspace);
        }

        assert!(app.emoji.is_none(), "the `:` is gone, so is the picker");
        assert_eq!(app.input().text(), "");
    }

    #[test]
    fn chat_url_does_not_open_the_picker() {
        let mut app = App::test_default();
        type_into_chat(&mut app, "see https://example.dev");

        assert!(app.emoji.is_none());
        assert_eq!(app.input().text(), "see https://example.dev");
    }

    #[test]
    fn arrow_keys_move_the_selection() {
        let mut app = App::test_default();
        type_into_chat(&mut app, ":sm");
        let first = app.emoji.as_ref().and_then(EmojiState::selected).expect("a match");

        press(&mut app, crossterm::event::KeyCode::Down);
        let second = app.emoji.as_ref().and_then(EmojiState::selected).expect("a match");

        assert_ne!(first.name, second.name, "Down moves to the next row");
        press(&mut app, crossterm::event::KeyCode::Enter);
        assert_eq!(app.input().text(), second.glyph);
    }
}
