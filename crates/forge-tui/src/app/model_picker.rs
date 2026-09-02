//! `/model` picker overlay: transient state + key handling.
//!
//! A centered overlay (rendered by [`crate::ui::model_picker`]) listing
//! the session's available models - the curated OpenRouter catalog on an
//! `openrouter` account, the CLI-advertised regular models elsewhere.
//! `enter` switches the session to the highlighted model; `esc` closes
//! without switching. Rows are snapshotted at open; a session reporting
//! no models never opens the picker (the `/model` submit falls back to
//! the current-model info line).

use crossterm::event::{KeyCode, KeyEvent};

use super::App;
use crate::agent::model;

/// State for the open `/model` picker. `None` on `App` when closed.
#[derive(Debug, Clone)]
pub struct ModelPickerState {
    /// Rows snapshotted at open from the session's available models.
    pub rows: Vec<model::AvailableModel>,
    /// Index into [`ModelPickerState::rows`] of the highlighted row.
    pub highlight: usize,
}

/// The pickable rows for the active session: the CLI-advertised models
/// minus the pseudo `default` row the CLI lists but `/model` cannot
/// switch to directly (the same filter the argument autocomplete uses).
fn rows(app: &App) -> Vec<model::AvailableModel> {
    app.available_models()
        .iter()
        .filter(|row| {
            !row.id.eq_ignore_ascii_case("default")
                && !row.display_name.eq_ignore_ascii_case("default")
        })
        .cloned()
        .collect()
}

/// Open the picker over the session's available models. Returns `false`
/// (opening nothing) when the session reports no rows to pick from.
pub(crate) fn open(app: &mut App) -> bool {
    let rows = rows(app);
    if rows.is_empty() {
        return false;
    }
    let current = app.current_model();
    let highlight = rows
        .iter()
        .position(|row| {
            current.is_some_and(|model| {
                model.requested_id.as_deref() == Some(row.id.as_str())
                    || model.resolved_id == row.id
            })
        })
        .unwrap_or(0);
    app.model_picker = Some(ModelPickerState { rows, highlight });
    app.needs_redraw = true;
    true
}

/// Handle a key while the picker is open. Always consumes the key
/// (returns `true`). Up/Down move the highlight; `enter` switches the
/// session to the highlighted model and closes; `esc` closes without
/// switching.
pub(crate) fn handle_key(app: &mut App, key: KeyEvent) -> bool {
    enum Action {
        Move(usize),
        Commit(String),
        Close,
    }
    let Some(state) = app.model_picker.as_ref() else {
        return false;
    };
    let count = state.rows.len();
    let action = match key.code {
        KeyCode::Up => Action::Move((state.highlight + count - 1) % count),
        KeyCode::Down => Action::Move((state.highlight + 1) % count),
        KeyCode::Enter => Action::Commit(state.rows[state.highlight].id.clone()),
        KeyCode::Esc => Action::Close,
        _ => return true,
    };
    match action {
        Action::Move(highlight) => {
            if let Some(state) = app.model_picker.as_mut() {
                state.highlight = highlight;
            }
            app.needs_redraw = true;
        }
        Action::Commit(id) => {
            close(app);
            crate::app::slash::switch_model(app, &id);
        }
        Action::Close => close(app),
    }
    true
}

fn close(app: &mut App) {
    app.model_picker = None;
    app.needs_redraw = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// The ten curated OpenRouter rows, shaped the way
    /// `curated_available_models` produces them.
    fn curated_rows() -> Vec<model::AvailableModel> {
        [
            ("z-ai/glm-5.3", "Z.ai: GLM 5.3 (Opus-class)"),
            ("deepseek/deepseek-v4-pro-0813", "DeepSeek: DeepSeek V4 Pro (Opus-class)"),
            ("moonshotai/kimi-k3", "MoonshotAI: Kimi K3 (Opus-class)"),
            ("z-ai/glm-5.3-flash", "Z.ai: GLM 5.3 Flash (Opus-class)"),
            ("deepseek/deepseek-v4-flash", "DeepSeek: DeepSeek V4 Flash (Strong)"),
            ("minimax/minimax-m3", "MiniMax: MiniMax M3 (Strong)"),
            ("z-ai/glm-5.2", "Z.ai: GLM 5.2 (Strong)"),
            ("google/gemini-2.5-flash", "Google: Gemini 2.5 Flash (Closed reference)"),
            ("x-ai/grok-4.3", "xAI: Grok 4.3 (Closed reference)"),
            ("deepseek/deepseek-v4-pro", "DeepSeek: DeepSeek V4 Pro (Closed reference)"),
        ]
        .into_iter()
        .map(|(id, name)| model::AvailableModel::new(id, name).description("bench - price"))
        .collect()
    }

    fn app_with_rows(rows: Vec<model::AvailableModel>) -> App {
        let mut app = App::test_default();
        app.try_active_bucket_mut()
            .expect("test_default seeds an active bucket")
            .available_models = rows;
        app
    }

    // -- open --------------------------------------------------------

    #[test]
    fn model_submit_opens_the_picker_on_a_session_with_models() {
        let mut app = app_with_rows(curated_rows());

        let handled = crate::app::slash::try_handle_submit(&mut app, "/model");

        assert!(handled, "/model is handled locally");
        let picker = app.model_picker.expect("the picker opens after submitting /model");
        assert_eq!(picker.rows.len(), 10, "the picker lists every available model");
        assert_eq!(picker.rows[0].id, "z-ai/glm-5.3");
    }

    #[test]
    fn model_submit_hides_the_sdk_default_row() {
        let mut rows = vec![
            model::AvailableModel::new("default", "Default (recommended)"),
            model::AvailableModel::new("opus[1m]", "Opus (1M context)"),
            model::AvailableModel::new("sonnet", "Sonnet"),
            model::AvailableModel::new("haiku", "Haiku"),
        ];
        rows[0].description = Some("Use the default model".to_owned());
        let mut app = app_with_rows(rows);

        assert!(crate::app::slash::try_handle_submit(&mut app, "/model"));

        let picker = app.model_picker.expect("the picker opens");
        assert_eq!(picker.rows.len(), 3, "the pseudo default row is not pickable");
        assert!(picker.rows.iter().all(|row| row.id != "default"));
    }

    #[test]
    fn model_submit_falls_back_to_the_info_line_when_no_models() {
        let mut app = App::test_default();

        let handled = crate::app::slash::try_handle_submit(&mut app, "/model");

        assert!(handled);
        assert!(app.model_picker.is_none(), "no picker without models");
        let last = app.messages().last().expect("the info line still shows");
        let text: String = last
            .blocks
            .iter()
            .filter_map(|b| match b {
                crate::app::MessageBlock::Text(t) => Some(t.markdown.full_text()),
                _ => None,
            })
            .collect();
        assert!(text.contains("Model:"), "the info line names the model, got: {text}");
    }

    #[test]
    fn open_seeds_the_highlight_to_the_current_model() {
        let mut app = app_with_rows(curated_rows());
        app.set_current_model(Some(model::CurrentModel::new(
            "z-ai/glm-5.3-flash",
            "GLM 5.3 Flash",
            "GLM 5.3 Flash",
        )));

        assert!(crate::app::slash::try_handle_submit(&mut app, "/model"));

        let picker = app.model_picker.expect("picker open");
        assert_eq!(
            picker.rows[picker.highlight].id, "z-ai/glm-5.3-flash",
            "the highlight lands on the running model",
        );
    }

    // -- navigation --------------------------------------------------

    #[test]
    fn up_down_move_the_highlight_and_wrap() {
        let mut app = app_with_rows(curated_rows());
        assert!(open(&mut app), "the test app has rows");

        handle_key(&mut app, key(KeyCode::Down));
        assert_eq!(app.model_picker.as_ref().expect("open").highlight, 1);
        handle_key(&mut app, key(KeyCode::Up));
        handle_key(&mut app, key(KeyCode::Up));
        let count = app.model_picker.as_ref().expect("open").rows.len();
        assert_eq!(
            app.model_picker.as_ref().expect("open").highlight,
            count - 1,
            "up from the first row wraps to the last",
        );
    }

    // -- commit + cancel ---------------------------------------------

    #[test]
    fn enter_switches_to_the_highlighted_model_and_closes() {
        let mut app = app_with_rows(curated_rows());
        if let Some(ws) = app.workspace.as_ref() {
            ws.enable_test_dispatch_intercept();
        }
        open(&mut app);
        handle_key(&mut app, key(KeyCode::Down));

        assert!(handle_key(&mut app, key(KeyCode::Enter)));

        assert!(app.model_picker.is_none(), "enter closes the picker");
        let dispatched =
            app.workspace.as_ref().map(|ws| ws.drain_test_dispatch_buffer()).unwrap_or_default();
        assert!(
            matches!(&dispatched[..],
                [forge_workspace::Command::SetModel { model, .. }]
                    if model == "deepseek/deepseek-v4-pro-0813"),
            "enter dispatches SetModel for the highlighted row, got: {dispatched:?}",
        );
    }

    #[test]
    fn esc_closes_without_committing() {
        let mut app = app_with_rows(curated_rows());
        if let Some(ws) = app.workspace.as_ref() {
            ws.enable_test_dispatch_intercept();
        }
        open(&mut app);

        assert!(handle_key(&mut app, key(KeyCode::Esc)));

        assert!(app.model_picker.is_none(), "esc closes the picker");
        let dispatched =
            app.workspace.as_ref().map(|ws| ws.drain_test_dispatch_buffer()).unwrap_or_default();
        assert!(dispatched.is_empty(), "esc must not switch models, got: {dispatched:?}");
    }
}
