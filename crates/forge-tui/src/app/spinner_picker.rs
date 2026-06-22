//! `/spinner` picker overlay: transient state + key handling.
//!
//! A centered overlay (rendered by [`crate::ui::spinner_picker`])
//! listing every [`SpinnerStyle`] animating live. Arrow keys preview
//! the highlighted style across the whole UI by mutating
//! `App::spinner_style`; `enter` commits the choice (persisted to the
//! forge-state.toml sidecar via `Workspace::persist_spinner`); `esc`
//! restores the style that was active when the overlay opened.

use crossterm::event::{KeyCode, KeyEvent};
use forge_workspace::SpinnerStyle;

use super::App;

/// State for the open `/spinner` picker. `None` on `App` when closed.
#[derive(Debug, Clone, Copy)]
pub struct SpinnerPickerState {
    /// Index into [`SpinnerStyle::ALL_STYLES`] of the highlighted row.
    pub highlight: usize,
    /// Style active when the picker opened. Restored on `esc` (cancel)
    /// so live-preview navigation doesn't stick when the user backs out.
    pub prior_style: SpinnerStyle,
}

/// Open the picker: highlight the active style and snapshot it as the
/// cancel-restore target. The overlay reads `App::spinner_style` for
/// the live preview, so no further seeding is needed.
pub(crate) fn open(app: &mut App) {
    let highlight =
        SpinnerStyle::ALL_STYLES.iter().position(|s| *s == app.spinner_style).unwrap_or(0);
    app.spinner_picker = Some(SpinnerPickerState { highlight, prior_style: app.spinner_style });
    app.needs_redraw = true;
}

fn close(app: &mut App) {
    app.spinner_picker = None;
    app.needs_redraw = true;
}

/// Handle a key while the picker is open. Always consumes the key
/// (returns `true`). Up/Down move the highlight AND live-preview that
/// style; `enter` commits + persists the highlight; `esc` restores the
/// pre-open style. Any other key is swallowed (the overlay is modal).
pub(crate) fn handle_key(app: &mut App, key: KeyEvent) -> bool {
    let Some(state) = app.spinner_picker else {
        return false;
    };
    let count = SpinnerStyle::ALL_STYLES.len();
    match key.code {
        KeyCode::Up => preview(app, (state.highlight + count - 1) % count),
        KeyCode::Down => preview(app, (state.highlight + 1) % count),
        KeyCode::Enter => {
            let style = SpinnerStyle::ALL_STYLES[state.highlight];
            app.spinner_style = style;
            if let Some(ws) = app.workspace.as_ref() {
                ws.persist_spinner(style);
            }
            close(app);
        }
        KeyCode::Esc => {
            app.spinner_style = state.prior_style;
            close(app);
        }
        _ => {}
    }
    true
}

/// Move the highlight and live-preview that style by setting it active
/// immediately - the whole UI animates with it. No persist until `enter`.
fn preview(app: &mut App, highlight: usize) {
    app.spinner_style = SpinnerStyle::ALL_STYLES[highlight];
    if let Some(state) = app.spinner_picker.as_mut() {
        state.highlight = highlight;
    }
    app.needs_redraw = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn open_seeds_highlight_and_prior_from_active() {
        let mut app = App::test_default();
        app.spinner_style = SpinnerStyle::Pulse;
        open(&mut app);
        let state = app.spinner_picker.expect("picker open");
        let expected = SpinnerStyle::ALL_STYLES.iter().position(|s| *s == SpinnerStyle::Pulse);
        assert_eq!(Some(state.highlight), expected, "highlight is the active style's index");
        assert_eq!(state.prior_style, SpinnerStyle::Pulse);
    }

    #[test]
    fn navigate_live_previews_the_highlighted_style() {
        let mut app = App::test_default();
        app.spinner_style = SpinnerStyle::ALL_STYLES[0];
        open(&mut app);
        assert!(handle_key(&mut app, key(KeyCode::Down)));
        assert_eq!(
            app.spinner_style,
            SpinnerStyle::ALL_STYLES[1],
            "down previews the next style live",
        );
        assert_eq!(app.spinner_picker.expect("still open").highlight, 1);
    }

    #[test]
    fn enter_commits_highlighted_style_and_closes() {
        let mut app = App::test_default();
        app.spinner_style = SpinnerStyle::ALL_STYLES[0];
        open(&mut app);
        handle_key(&mut app, key(KeyCode::Down));
        assert!(handle_key(&mut app, key(KeyCode::Enter)));
        assert_eq!(app.spinner_style, SpinnerStyle::ALL_STYLES[1]);
        assert!(app.spinner_picker.is_none(), "enter closes the picker");
    }

    #[test]
    fn esc_restores_prior_style_and_closes() {
        let mut app = App::test_default();
        app.spinner_style = SpinnerStyle::ALL_STYLES[0];
        open(&mut app);
        handle_key(&mut app, key(KeyCode::Down));
        assert_ne!(app.spinner_style, SpinnerStyle::ALL_STYLES[0], "navigation changed the live style");
        assert!(handle_key(&mut app, key(KeyCode::Esc)));
        assert_eq!(
            app.spinner_style,
            SpinnerStyle::ALL_STYLES[0],
            "esc restores the pre-open style",
        );
        assert!(app.spinner_picker.is_none(), "esc closes the picker");
    }

    #[test]
    fn up_from_first_wraps_to_last() {
        let mut app = App::test_default();
        app.spinner_style = SpinnerStyle::ALL_STYLES[0];
        open(&mut app);
        handle_key(&mut app, key(KeyCode::Up));
        let last = SpinnerStyle::ALL_STYLES.len() - 1;
        assert_eq!(app.spinner_picker.expect("open").highlight, last, "up from first wraps to last");
        assert_eq!(app.spinner_style, SpinnerStyle::ALL_STYLES[last]);
    }
}
