//! Terminal tab/window title updates.
//!
//! Writes the OSC 0 escape (`ESC ] 0 ; <title> BEL`) so the host
//! terminal updates its tab and window titles with the current
//! session's model + cwd. Falls back silently when stdout is
//! redirected or the terminal doesn't support OSC.

use std::io::{Write, stdout};

use crate::state::app::App;

/// Build the title string for the current `app` state.
///
/// Format: `"{model_short} \u{2014} {cwd_basename}"` when both are
/// available; `"forge-tui"` when neither is set yet.
#[must_use]
pub fn build_title(app: &App) -> String {
    let model_short = app
        .current_model
        .as_ref()
        .map(|m| m.display_name_short.clone());
    let cwd_basename = if app.cwd_raw.is_empty() {
        None
    } else {
        std::path::Path::new(&app.cwd_raw)
            .file_name()
            .and_then(|s| s.to_str())
            .map(str::to_owned)
    };

    match (model_short, cwd_basename) {
        (Some(model), Some(cwd)) => format!("{model} \u{2014} {cwd}"),
        (Some(model), None) => format!("forge-tui \u{2014} {model}"),
        (None, Some(cwd)) => format!("forge-tui \u{2014} {cwd}"),
        (None, None) => "forge-tui".to_owned(),
    }
}

/// Emit the OSC 0 (set window + icon name) escape so the terminal
/// updates the tab title. Best-effort; ignores stdout write errors
/// (the alt-screen guard is the source of truth for terminal state).
pub fn update(app: &App) {
    let title = build_title(app);
    let mut out = stdout().lock();
    let _ = write!(out, "\x1b]0;{title}\x07");
    let _ = out.flush();
}
