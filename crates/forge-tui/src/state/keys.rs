#![allow(
    dead_code,
    missing_docs,
    clippy::pedantic,
    reason = "lifted upstream from claude-code-rust (subset)"
)]

//! Keyboard helpers extracted from upstream `app/keys.rs` (1,171 LoC).
//! The full keys.rs handles every shortcut on the running TUI; this
//! file lifts only the three small helpers that `permissions.rs` and
//! `questions.rs` reach for. The full file lifts when the rest of the
//! TUI key routing migrates.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn is_ctrl_shortcut(modifiers: KeyModifiers) -> bool {
    modifiers.contains(KeyModifiers::CONTROL) && !modifiers.contains(KeyModifiers::ALT)
}

fn ctrl_char(expected: char) -> Option<char> {
    let upper = expected.to_ascii_uppercase();
    if !upper.is_ascii_alphabetic() {
        return None;
    }
    Some(char::from((upper as u8) & 0x1f))
}

#[must_use]
pub fn is_ctrl_char_shortcut(key: KeyEvent, expected: char) -> bool {
    match key.code {
        KeyCode::Char(c) if c.eq_ignore_ascii_case(&expected) => is_ctrl_shortcut(key.modifiers),
        KeyCode::Char(c) if Some(c) == ctrl_char(expected) => {
            !key.modifiers.contains(KeyModifiers::ALT)
        }
        _ => false,
    }
}
