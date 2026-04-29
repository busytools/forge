#![allow(
    dead_code,
    missing_docs,
    clippy::pedantic,
    reason = "lifted upstream from claude-code-rust (types subset)"
)]

//! Slash command state types lifted from upstream `app/slash/mod.rs`.
//! The full slash module (parser + dispatch + per-command executors)
//! is ~2,900 LoC and pulls in config / connect / events. We lift only
//! the type surface that the autocomplete UI reaches for; full slash
//! lift comes after config lifts (cuts list).

use crate::state::dialog::DialogState;

pub const MAX_VISIBLE: usize = 8;

#[derive(Debug, Clone)]
pub struct SlashCandidate {
    pub insert_value: String,
    pub primary: String,
    pub secondary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashContext {
    CommandName,
    Argument {
        command: String,
        arg_index: usize,
        token_range: (usize, usize),
    },
}

#[derive(Debug, Clone)]
pub struct SlashState {
    /// Character position where `/` token starts.
    pub trigger_row: usize,
    pub trigger_col: usize,
    /// Current typed query for the active slash context.
    pub query: String,
    /// Command-name or argument context.
    pub context: SlashContext,
    /// Filtered list of supported candidates.
    pub candidates: Vec<SlashCandidate>,
    /// Shared autocomplete dialog navigation state.
    pub dialog: DialogState,
}

#[must_use]
pub fn is_cancel_command(text: &str) -> bool {
    let trimmed = text.trim();
    if !trimmed.starts_with('/') {
        return false;
    }
    let mut parts = trimmed.split_whitespace();
    parts.next().is_some_and(|name| name == "/cancel")
}

// ---------------------------------------------------------------------------
// Trigger detection + lifecycle (forge-shape; mirrors mention/subagent)
// ---------------------------------------------------------------------------

use crate::state::app::App;

/// Detect a `/<query>` token at the start of the first input line.
/// Returns `(start_col_inclusive, end_col_exclusive, query)` when the
/// cursor is positioned inside the token; `None` otherwise.
#[must_use]
pub fn detect_slash_at_cursor(
    lines: &[String],
    cursor_row: usize,
    cursor_col: usize,
) -> Option<(usize, usize, String)> {
    if cursor_row != 0 {
        return None;
    }
    let line = lines.first()?;
    if !line.starts_with('/') {
        return None;
    }
    // Token spans from column 0 to the first whitespace (or end of line).
    let end = line
        .char_indices()
        .find(|(_, c)| c.is_whitespace())
        .map_or(line.chars().count(), |(byte_idx, _)| {
            line[..byte_idx].chars().count()
        });
    if cursor_col > end {
        return None;
    }
    let query = line.chars().skip(1).take(end.saturating_sub(1)).collect();
    Some((0, end, query))
}

pub fn activate(app: &mut App) {
    let Some((_, _, query)) = detect_slash_at_cursor(
        app.input.lines(),
        app.input.cursor_row(),
        app.input.cursor_col(),
    ) else {
        return;
    };
    let candidates = filter_candidates(&app.available_commands, &query);
    let mut dialog = DialogState::default();
    dialog.clamp(candidates.len(), candidates.len().min(MAX_VISIBLE));
    app.slash = Some(SlashState {
        trigger_row: 0,
        trigger_col: 0,
        query,
        context: SlashContext::CommandName,
        candidates,
        dialog,
    });
}

pub fn update_query(app: &mut App) {
    let Some((_, _, query)) = detect_slash_at_cursor(
        app.input.lines(),
        app.input.cursor_row(),
        app.input.cursor_col(),
    ) else {
        return;
    };
    let candidates = filter_candidates(&app.available_commands, &query);
    if let Some(slash) = app.slash.as_mut() {
        slash.query = query;
        slash.candidates = candidates;
        slash
            .dialog
            .clamp(slash.candidates.len(), slash.candidates.len().min(MAX_VISIBLE));
    }
}

pub fn sync_with_cursor(app: &mut App) {
    let in_slash = detect_slash_at_cursor(
        app.input.lines(),
        app.input.cursor_row(),
        app.input.cursor_col(),
    )
    .is_some();
    match (in_slash, app.slash.is_some()) {
        (true, true) => update_query(app),
        (true, false) => activate(app),
        (false, true) => deactivate(app),
        (false, false) => {}
    }
}

pub fn confirm_selection(app: &mut App) {
    let Some(slash) = app.slash.take() else { return };
    let Some(candidate) = slash.candidates.get(slash.dialog.selected) else {
        return;
    };
    let replacement = candidate.insert_value.clone();
    let mut text = format!("{replacement} ");
    // Replace the leading `/<query>` with the new prefix. Anything the
    // user already typed after the first whitespace is preserved.
    if let Some(line) = app.input.lines().first()
        && let Some(rest) = line.split_once(char::is_whitespace).map(|(_, rest)| rest)
    {
        text.push_str(rest);
    }
    app.input.set_text(&text);
}

pub fn move_up(app: &mut App) {
    if let Some(slash) = app.slash.as_mut()
        && !slash.candidates.is_empty()
    {
        slash
            .dialog
            .move_up(slash.candidates.len(), slash.candidates.len().min(MAX_VISIBLE));
    }
}

pub fn move_down(app: &mut App) {
    if let Some(slash) = app.slash.as_mut()
        && !slash.candidates.is_empty()
    {
        slash
            .dialog
            .move_down(slash.candidates.len(), slash.candidates.len().min(MAX_VISIBLE));
    }
}

pub fn deactivate(app: &mut App) {
    app.slash = None;
}

fn filter_candidates(
    commands: &[crate::state::model::AvailableCommand],
    query: &str,
) -> Vec<SlashCandidate> {
    let q = query.to_ascii_lowercase();
    commands
        .iter()
        .filter(|cmd| q.is_empty() || cmd.name.to_ascii_lowercase().contains(&q))
        .map(|cmd| SlashCandidate {
            insert_value: format!("/{}", cmd.name),
            primary: cmd.name.clone(),
            secondary: if cmd.description.is_empty() {
                None
            } else {
                Some(cmd.description.clone())
            },
        })
        .collect()
}
