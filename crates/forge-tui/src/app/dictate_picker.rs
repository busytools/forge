//! `/dictate` overlay: transient state + key handling.
//!
//! A centered overlay (rendered by [`crate::ui::dictate_picker`])
//! following the `/account` picker idiom: arrows move the highlight,
//! enter sets the highlighted row, esc closes. Enter never closes -
//! the dialog is a set of choices made in one visit, and reset
//! deliberately stays open so fine-tuning can continue. The rows are
//! derived from the session's live overrides on every read, so the
//! markers and the reset row's dimness can never drift from what the
//! workspace echoed.

use crossterm::event::{KeyCode, KeyEvent};
use forge_workspace::{Context, DictateOverrideUpdate, DictateOverrides, Structure, Styling};

use super::App;

/// State for the open `/dictate` overlay. `None` on `App` when closed.
#[derive(Debug, Clone)]
pub struct DictatePickerState {
    /// Highlighted row index into [`rows`]' output.
    pub highlight: usize,
}

/// One selectable row. `selectable` is false only for the reset row
/// while nothing is overridden: the mock draws it DIM and unreachable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PickerRow {
    pub group: &'static str,
    pub label: &'static str,
    pub update: DictateOverrideUpdate,
    /// This session has overridden this axis to exactly this value.
    pub marker: bool,
    pub selectable: bool,
}

/// The rows for `overrides`: the eleven axis rows, then the reset row.
pub(crate) fn rows(overrides: DictateOverrides) -> Vec<PickerRow> {
    let mut rows = Vec::new();
    let voice = [
        (Styling::Casual, "casual"),
        (Styling::SemiCasual, "semi-casual"),
        (Styling::SemiFormal, "semi-formal"),
        (Styling::Formal, "formal"),
    ];
    for (value, label) in voice {
        rows.push(PickerRow {
            group: "VOICE",
            label,
            update: DictateOverrideUpdate::Styling(value),
            marker: overrides.styling == Some(value),
            selectable: true,
        });
    }
    let structure = [(Structure::Prose, "prose"), (Structure::Lists, "may bullet a list")];
    for (value, label) in structure {
        rows.push(PickerRow {
            group: "STRUCTURE",
            label,
            update: DictateOverrideUpdate::Structure(value),
            marker: overrides.structure == Some(value),
            selectable: true,
        });
    }
    let context = [(Context::General, "plain text"), (Context::Email, "email layout")];
    for (value, label) in context {
        rows.push(PickerRow {
            group: "DESTINATION",
            label,
            update: DictateOverrideUpdate::Context(value),
            marker: overrides.context == Some(value),
            selectable: true,
        });
    }
    rows.push(PickerRow {
        group: "",
        label: "Reset all to defaults",
        update: DictateOverrideUpdate::Reset,
        marker: false,
        selectable: !overrides.is_empty(),
    });
    rows
}

/// The session's live overrides, or the default set when no session is
/// active (the overlay is opened per session, so this is defensive).
fn live_overrides(app: &App) -> DictateOverrides {
    app.active_session().map(|s| s.dictate_overrides).unwrap_or_default()
}

pub(crate) fn open(app: &mut App) {
    app.dictate_picker = Some(DictatePickerState { highlight: 0 });
    app.needs_redraw = true;
}

pub(crate) fn close(app: &mut App) {
    app.dictate_picker = None;
    app.needs_redraw = true;
}

/// Handle a key while the overlay is open. Always consumes the key
/// (returns `true`; the overlay is modal). Up/Down move the highlight
/// over the selectable rows (the inert reset row is skipped); enter
/// sets the highlighted row and stays open; esc closes.
pub(crate) fn handle_key(app: &mut App, key: KeyEvent) -> bool {
    if app.dictate_picker.is_none() {
        return false;
    }
    let rows = rows(live_overrides(app));
    match key.code {
        KeyCode::Up => {
            if let Some(state) = app.dictate_picker.as_mut() {
                state.highlight = previous_selectable(&rows, state.highlight);
            }
            app.needs_redraw = true;
        }
        KeyCode::Down => {
            if let Some(state) = app.dictate_picker.as_mut() {
                state.highlight = next_selectable(&rows, state.highlight);
            }
            app.needs_redraw = true;
        }
        KeyCode::Enter => commit(app, &rows),
        KeyCode::Esc => close(app),
        _ => {}
    }
    true
}

fn next_selectable(rows: &[PickerRow], from: usize) -> usize {
    let mut idx = from;
    while idx + 1 < rows.len() {
        idx += 1;
        if rows[idx].selectable {
            return idx;
        }
    }
    from
}

fn previous_selectable(rows: &[PickerRow], from: usize) -> usize {
    let mut idx = from;
    while idx > 0 {
        idx -= 1;
        if rows[idx].selectable {
            return idx;
        }
    }
    from
}

/// Apply the highlighted row and keep the dialog open: the dialog is a
/// set of choices made in one visit. The markers update when the
/// workspace echo lands. A reset also restarts the highlight at the
/// first row, because fine-tuning begins again from the top.
fn commit(app: &mut App, rows: &[PickerRow]) {
    let Some(state) = app.dictate_picker.as_ref() else {
        return;
    };
    let Some(row) = rows.get(state.highlight) else {
        return;
    };
    if !row.selectable {
        return;
    }
    let result = app.dispatch_command(|key| forge_workspace::Command::SetDictateOverride {
        key,
        update: row.update,
    });
    if let Err(error) = result {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "dictate_override_dispatch_failed",
            message = "could not apply the highlighted /dictate row",
            outcome = "failure",
            error_message = %error,
        );
    }
    if row.update == DictateOverrideUpdate::Reset
        && let Some(state) = app.dictate_picker.as_mut()
    {
        state.highlight = 0;
    }
    app.needs_redraw = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;

    fn overridden() -> DictateOverrides {
        DictateOverrides {
            structure: Some(Structure::Lists),
            context: Some(Context::Email),
            ..Default::default()
        }
    }

    fn group_of<'a>(rows: &'a [PickerRow], group: &str) -> Vec<&'a PickerRow> {
        rows.iter().filter(|r| r.group == group).collect()
    }

    #[test]
    fn rows_carry_the_crates_vocabulary_under_the_dialogs_groups() {
        let rows = rows(DictateOverrides::default());

        let voice: Vec<&str> = group_of(&rows, "VOICE").iter().map(|r| r.label).collect();
        assert_eq!(voice, vec!["casual", "semi-casual", "semi-formal", "formal"]);

        let structure: Vec<&str> = group_of(&rows, "STRUCTURE").iter().map(|r| r.label).collect();
        assert_eq!(structure, vec!["prose", "may bullet a list"]);

        let destination: Vec<&str> =
            group_of(&rows, "DESTINATION").iter().map(|r| r.label).collect();
        assert_eq!(destination, vec!["plain text", "email layout"]);

        assert_eq!(rows.len(), 9, "eight axis rows plus reset");
    }

    #[test]
    fn the_marker_marks_this_sessions_override_not_the_current_value() {
        let all = rows(overridden());
        let marked: Vec<&str> = all.iter().filter(|r| r.marker).map(|r| r.label).collect();
        assert_eq!(marked, vec!["may bullet a list", "email layout"]);

        // Nothing overridden: no markers anywhere.
        assert!(rows(DictateOverrides::default()).iter().all(|r| !r.marker));
    }

    #[test]
    fn reset_row_is_selectable_only_when_something_is_set() {
        let bare = rows(DictateOverrides::default());
        let reset = bare.last().expect("reset row is always drawn");
        assert_eq!(reset.label, "Reset all to defaults");
        assert!(!reset.selectable, "nothing to clear");

        let set = rows(overridden());
        assert!(set.last().expect("reset row").selectable);
    }

    #[test]
    fn submit_opens_the_overlay_and_args_are_refused() {
        let mut app = App::test_default();
        assert!(crate::app::slash::try_handle_submit(&mut app, "/dictate"));
        assert!(app.dictate_picker.is_some(), "no-arg opens the overlay");

        let mut app = App::test_default();
        assert!(crate::app::slash::try_handle_submit(&mut app, "/dictate extra"));
        assert!(app.dictate_picker.is_none(), "arguments are not part of the command");
        let last = app.messages().last().expect("a usage notice");
        assert!(matches!(last.role, crate::app::MessageRole::System(_)));
    }

    #[test]
    fn enter_sets_the_highlighted_axis_and_stays_open() {
        let mut app = App::test_default();
        let workspace = app.workspace.clone().expect("test workspace");
        workspace.enable_test_dispatch_intercept();
        crate::app::slash::try_handle_submit(&mut app, "/dictate");
        assert!(app.dictate_picker.is_some());

        // Highlight `semi-formal` (third row) and commit.
        let key = |code| KeyEvent::new(code, crossterm::event::KeyModifiers::NONE);
        handle_key(&mut app, key(KeyCode::Down));
        handle_key(&mut app, key(KeyCode::Down));
        handle_key(&mut app, key(KeyCode::Enter));

        assert!(app.dictate_picker.is_some(), "enter does not close the dialog");
        let dispatched = workspace.drain_test_dispatch_buffer();
        assert_eq!(dispatched.len(), 1, "one set per enter: {dispatched:?}");
        match &dispatched[0] {
            forge_workspace::Command::SetDictateOverride { key, update } => {
                assert_eq!(key, &app.active_session_key.clone().expect("active session"));
                assert_eq!(*update, DictateOverrideUpdate::Styling(Styling::SemiFormal),);
            }
            other => panic!("a set dispatch, got {other:?}"),
        }
    }

    #[test]
    fn arrows_move_over_selectable_rows_only() {
        let mut app = App::test_default();
        crate::app::slash::try_handle_submit(&mut app, "/dictate");

        let key = |code| KeyEvent::new(code, crossterm::event::KeyModifiers::NONE);
        // Down to the end: the inert reset row is skipped, so the
        // highlight stops on `email layout` (index 7).
        for _ in 0..20 {
            handle_key(&mut app, key(KeyCode::Down));
        }
        let state = app.dictate_picker.as_ref().expect("open");
        assert_eq!(state.highlight, 7, "the dim reset row is unreachable");

        // Up never leaves the top.
        for _ in 0..20 {
            handle_key(&mut app, key(KeyCode::Up));
        }
        assert_eq!(app.dictate_picker.as_ref().expect("open").highlight, 0);
    }

    #[test]
    fn reset_commits_a_full_clear_then_restarts_the_highlight() {
        let mut app = App::test_default();
        let workspace = app.workspace.clone().expect("test workspace");
        workspace.enable_test_dispatch_intercept();
        let key = app.active_session_key.clone().expect("active session");
        app.sessions.get_mut(&key).expect("bucket").dictate_overrides = overridden();
        crate::app::slash::try_handle_submit(&mut app, "/dictate");

        let key = |code| KeyEvent::new(code, crossterm::event::KeyModifiers::NONE);
        // Down onto the now-selectable reset row (index 8).
        for _ in 0..8 {
            handle_key(&mut app, key(KeyCode::Down));
        }
        handle_key(&mut app, key(KeyCode::Enter));

        assert!(app.dictate_picker.is_some(), "reset does not close the dialog");
        let dispatched = workspace.drain_test_dispatch_buffer();
        match dispatched.last() {
            Some(forge_workspace::Command::SetDictateOverride { key, update }) => {
                assert_eq!(key, &app.active_session_key.clone().expect("active session"));
                assert_eq!(*update, DictateOverrideUpdate::Reset);
            }
            other => panic!("a reset dispatch, got {other:?}"),
        }
        assert_eq!(
            app.dictate_picker.as_ref().expect("open").highlight,
            0,
            "fine-tuning starts over from the first row"
        );
    }

    #[test]
    fn esc_closes() {
        let mut app = App::test_default();
        crate::app::slash::try_handle_submit(&mut app, "/dictate");
        assert!(handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Esc, crossterm::event::KeyModifiers::NONE)
        ));
        assert!(app.dictate_picker.is_none());
    }
}
