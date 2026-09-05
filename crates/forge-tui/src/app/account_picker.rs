//! `/account` picker overlay: transient state + key handling.
//!
//! A centered overlay (rendered by [`crate::ui::account_picker`])
//! listing the active session's project accounts plus their live
//! rate-limit state (snapshotted via
//! [`forge_workspace::Workspace::project_accounts_snapshot`]). Arrow
//! keys move the highlight; `enter` switches the session to the
//! highlighted account (a no-op when it is already the current one);
//! `esc` closes. The switch re-spawns the session under the picked
//! account and resumes the same conversation - see
//! `forge_workspace::spawn::handle_switch_account`.

use crossterm::event::{KeyCode, KeyEvent};
use forge_workspace::AccountRow;

use super::App;

/// State for the open `/account` picker. `None` on `App` when closed.
#[derive(Debug, Clone)]
pub struct AccountPickerState {
    /// Project accounts + their state, in allow-list order.
    pub rows: Vec<AccountRow>,
    /// Highlighted row index.
    pub highlight: usize,
}

impl AccountPickerState {
    /// The highlighted account row, if any.
    pub fn selected(&self) -> Option<&AccountRow> {
        self.rows.get(self.highlight)
    }

    fn move_up(&mut self) {
        self.highlight = self.highlight.saturating_sub(1);
    }

    fn move_down(&mut self) {
        if self.highlight + 1 < self.rows.len() {
            self.highlight += 1;
        }
    }
}

/// Open the picker over `rows`, highlighting the current account (or
/// the first row when none is marked). A no-op when `rows` is empty -
/// the executor surfaces its own note in that case.
pub(crate) fn open(app: &mut App, rows: Vec<AccountRow>) {
    if rows.is_empty() {
        return;
    }
    let highlight = rows.iter().position(|r| r.is_current).unwrap_or(0);
    app.account_picker = Some(AccountPickerState { rows, highlight });
    app.needs_redraw = true;
}

pub(crate) fn close(app: &mut App) {
    app.account_picker = None;
    app.needs_redraw = true;
}

/// Handle a key while the picker is open. Always consumes the key
/// (returns `true`; the overlay is modal). Up/Down move the highlight
/// (clamped); `enter` switches to the highlighted account; `esc`
/// closes.
pub(crate) fn handle_key(app: &mut App, key: KeyEvent) -> bool {
    if app.account_picker.is_none() {
        return false;
    }
    match key.code {
        KeyCode::Up => {
            if let Some(state) = app.account_picker.as_mut() {
                state.move_up();
            }
            app.needs_redraw = true;
        }
        KeyCode::Down => {
            if let Some(state) = app.account_picker.as_mut() {
                state.move_down();
            }
            app.needs_redraw = true;
        }
        KeyCode::Enter => commit(app),
        KeyCode::Esc => close(app),
        _ => {}
    }
    true
}

/// Switch the active session to the highlighted account. Picking the
/// current account is a no-op that just closes. Otherwise dispatch
/// `Command::SwitchAccount`, carrying the resume launch settings so
/// the switch preserves the session's model / mode / effort.
fn commit(app: &mut App) {
    let Some(state) = app.account_picker.as_ref() else {
        return;
    };
    let Some(selected) = state.selected() else {
        close(app);
        return;
    };
    if selected.is_current {
        close(app);
        return;
    }
    let account = selected.display_name.clone();
    close(app);

    // Re-check at commit time: a delivered peer / cron / gotify prompt
    // may have started a turn while the picker was open. Block only a
    // known in-flight turn (`None` / `Some(Idle)` allow, matching the
    // open-gate); the workspace backstop is authoritative. Bailing here
    // avoids the round-trip and surfaces the same notice.
    if matches!(
        app.runtime_session_state(),
        Some(
            crate::agent::model::RuntimeSessionState::Running
                | crate::agent::model::RuntimeSessionState::RequiresAction
        )
    ) {
        super::slash::push_system_message(
            app,
            "Finish or cancel the current turn before switching accounts.",
        );
        return;
    }

    let launch_settings = crate::app::connect::session_launch_settings_for_resume(app);
    if let Err(err) = app.dispatch_command(|key| forge_workspace::Command::SwitchAccount {
        key,
        account_display_name: account,
        launch_settings,
    }) {
        tracing::warn!(
            target: "forge_tui::account_picker",
            error = %err,
            "failed to dispatch account switch",
        );
        super::slash::push_system_message(app, "Couldn't switch accounts - please try again.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use std::path::PathBuf;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn row(name: &str, is_current: bool) -> AccountRow {
        AccountRow {
            display_name: name.to_owned(),
            config_dir: PathBuf::from(format!("/cfg/{name}")),
            is_current,
            unusable: None,
            budget: forge_workspace::AccountBudget::Subscription {
                five_hour_util: Some(10.0),
                seven_day_util: Some(5.0),
                resets_at: None,
            },
            experimental: false,
        }
    }

    #[test]
    fn open_highlights_the_current_account() {
        let mut app = App::test_default();
        open(&mut app, vec![row("A", false), row("B", true), row("C", false)]);
        let state = app.account_picker.expect("picker open");
        assert_eq!(state.highlight, 1, "highlight lands on the current account");
        assert_eq!(state.selected().map(|r| r.display_name.as_str()), Some("B"));
    }

    #[test]
    fn open_with_no_current_highlights_first() {
        let mut app = App::test_default();
        open(&mut app, vec![row("A", false), row("B", false)]);
        assert_eq!(app.account_picker.expect("open").highlight, 0);
    }

    #[test]
    fn open_with_empty_rows_is_noop() {
        let mut app = App::test_default();
        open(&mut app, Vec::new());
        assert!(app.account_picker.is_none(), "empty rows do not open a picker");
    }

    #[test]
    fn navigation_is_clamped_at_both_ends() {
        let mut app = App::test_default();
        open(&mut app, vec![row("A", true), row("B", false), row("C", false)]);
        // Up from the first row stays put (clamped, no wrap).
        handle_key(&mut app, key(KeyCode::Up));
        assert_eq!(app.account_picker.as_ref().expect("open").highlight, 0);
        // Down walks to the last row and clamps there.
        handle_key(&mut app, key(KeyCode::Down));
        handle_key(&mut app, key(KeyCode::Down));
        handle_key(&mut app, key(KeyCode::Down));
        assert_eq!(app.account_picker.expect("open").highlight, 2, "clamps at the last row");
    }

    #[test]
    fn selected_maps_across_the_experimental_boundary() {
        let mut app = App::test_default();
        // Rows arrive pre-sorted [regular, experimental] from the
        // snapshot; the dim EXPERIMENTAL header the render inserts does
        // not shift the highlight index, so nav still spans both groups.
        let exp = AccountRow {
            display_name: "Exp".to_owned(),
            config_dir: PathBuf::from("/cfg/Exp"),
            is_current: false,
            unusable: None,
            budget: forge_workspace::AccountBudget::Subscription {
                five_hour_util: Some(10.0),
                seven_day_util: Some(5.0),
                resets_at: None,
            },
            experimental: true,
        };
        open(&mut app, vec![row("A", false), exp]);
        handle_key(&mut app, key(KeyCode::Down));
        let state = app.account_picker.as_ref().expect("open");
        assert_eq!(state.highlight, 1);
        assert_eq!(state.selected().map(|r| r.display_name.as_str()), Some("Exp"));
        assert!(
            state.selected().expect("selected").experimental,
            "highlight lands on the experimental row across the boundary",
        );
    }

    #[test]
    fn esc_closes_the_picker() {
        let mut app = App::test_default();
        open(&mut app, vec![row("A", true), row("B", false)]);
        assert!(handle_key(&mut app, key(KeyCode::Esc)));
        assert!(app.account_picker.is_none(), "esc closes the picker");
    }

    #[test]
    fn enter_on_current_account_just_closes() {
        let mut app = App::test_default();
        // Highlight starts on the current account (B).
        open(&mut app, vec![row("A", false), row("B", true)]);
        assert!(handle_key(&mut app, key(KeyCode::Enter)));
        assert!(
            app.account_picker.is_none(),
            "picking the current account closes without a switch"
        );
    }

    fn last_message_text(app: &App) -> String {
        app.messages()
            .last()
            .map(|m| {
                m.blocks
                    .iter()
                    .filter_map(|b| match b {
                        crate::app::MessageBlock::Text(t) => Some(t.markdown.full_text()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn commit_while_running_bails_with_notice_and_no_switch() {
        let mut app = App::test_default();
        open(&mut app, vec![row("A", true), row("B", false)]);
        // Move the highlight off the current account so Enter would
        // otherwise dispatch a switch.
        handle_key(&mut app, key(KeyCode::Down));
        // A turn started (delivered prompt) while the picker was open.
        app.set_runtime_session_state(Some(crate::agent::model::RuntimeSessionState::Running));

        assert!(handle_key(&mut app, key(KeyCode::Enter)));
        assert!(app.account_picker.is_none(), "commit closes the picker");
        let text = last_message_text(&app);
        assert!(text.contains("Finish or cancel"), "commit surfaces the idle notice; got: {text}");
    }

    #[test]
    fn running_update_auto_closes_open_picker() {
        let mut app = App::test_default();
        open(&mut app, vec![row("A", true), row("B", false)]);
        assert!(app.account_picker.is_some());
        crate::app::events::handle_runtime_session_state_update(
            &mut app,
            crate::agent::model::RuntimeSessionState::Running,
        );
        assert!(app.account_picker.is_none(), "a Running update closes the open picker");
    }
}
