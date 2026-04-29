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
