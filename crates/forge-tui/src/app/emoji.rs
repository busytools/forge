//! Slack-style `:shortcode:` emoji typeahead, shared by every text
//! input via [`App::focused_input`].
//!
//! Trigger rule mirrors [`super::mention`]: the `:` only counts at the
//! start of a line or after whitespace, and the query has to look like a
//! shortcode. Without that, `http://`, `10:30` and `note:` would all pop
//! a picker.

use super::{App, FocusTarget, dialog::DialogState};
pub use forge_sessions::emoji::{Emoji, exact, is_shortcode_char, matches};

/// Max candidates shown in the dropdown. The list is dense (one glyph +
/// one short name per row), so a shorter window than the file picker's
/// keeps it from swallowing the pane.
pub const MAX_VISIBLE: usize = 10;

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

impl EmojiState {
    pub fn has_selectable_candidates(&self) -> bool {
        !self.candidates.is_empty()
    }

    pub fn selected(&self) -> Option<&'static Emoji> {
        self.candidates.get(self.dialog.selected).copied()
    }
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

/// The ranked matches for `query`, capped at what a view holds.
fn candidates_for(query: &str) -> Vec<&'static Emoji> {
    let mut candidates = matches(query);
    candidates.truncate(crate::app::MAX_CANDIDATES);
    candidates
}

/// Open the picker if the cursor sits in a `:shortcode` token.
pub fn activate(app: &mut App) {
    let Some((trigger_row, trigger_col, query)) = detect_at_focused_cursor(app) else {
        return;
    };
    let candidates = candidates_for(&query);
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
    let candidates = candidates_for(&query);
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

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn detect(text: &str) -> Option<(usize, usize, String)> {
        let lines = vec![text.to_owned()];
        detect_emoji_at_cursor(&lines, 0, text.chars().count())
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

    /// A bare `:` opens the token but ranks nothing: the dropdown
    /// stays shut until the query is long enough to mean something.
    #[test]
    fn a_bare_colon_detects_but_holds_no_candidates() {
        assert_eq!(detect(":"), Some((0, 0, String::new())));
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
        assert_eq!(app.input().expect("active session").text(), "ship it \u{1F680}");
        assert!(app.pending_submit().is_none(), "Enter on the picker must not arm a prompt submit");
    }

    #[test]
    fn chat_typing_continues_after_confirming() {
        let mut app = App::test_default();
        type_into_chat(&mut app, ":tada");
        press(&mut app, crossterm::event::KeyCode::Enter);
        type_into_chat(&mut app, " ship");

        assert_eq!(app.input().expect("active session").text(), "\u{1F389} ship");
    }

    #[test]
    fn chat_esc_dismisses_the_picker_and_keeps_the_draft() {
        let mut app = App::test_default();
        type_into_chat(&mut app, "hi :roc");

        press(&mut app, crossterm::event::KeyCode::Esc);

        assert!(app.emoji.is_none());
        assert_eq!(
            app.input().expect("active session").text(),
            "hi :roc",
            "the typed token survives"
        );
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
        assert_eq!(app.input().expect("active session").text(), "");
    }

    #[test]
    fn chat_url_does_not_open_the_picker() {
        let mut app = App::test_default();
        type_into_chat(&mut app, "see https://example.dev");

        assert!(app.emoji.is_none());
        assert_eq!(app.input().expect("active session").text(), "see https://example.dev");
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
        assert_eq!(app.input().expect("active session").text(), second.glyph);
    }
}
