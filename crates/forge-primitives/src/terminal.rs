//! The terminal's wire shapes: what a client draws, what a hand-off parks,
//! and what the agent's tools answer.
//!
//! **The bytes are not here.** Terminal output crosses as its own binary
//! frame kinds ([`TERMINAL_OUTPUT_TAG`] and [`TERMINAL_INPUT_TAG`]); these
//! types are the JSON around it - the strip's row, the attach snapshot, the
//! parked ask and the state a tool reads.

use serde::{Deserialize, Serialize};

use crate::SessionSlot;

/// The binary kind a terminal's output crosses as: `id u16`, `seq u32`, raw
/// bytes.
pub const TERMINAL_OUTPUT_TAG: u8 = 4;

/// The binary kind a client's terminal input crosses as: `id u16`, raw bytes.
pub const TERMINAL_INPUT_TAG: u8 = 5;

/// The most columns and rows a resize may name.
pub const MAX_COLS: u16 = 500;
pub const MAX_ROWS: u16 = 300;
pub const MIN_COLS: u16 = 2;
pub const MIN_ROWS: u16 = 2;

/// Where a state answer came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalSource {
    /// The shell's OSC 133 marks, or its own exit once it has died.
    Exact,
    /// The screen's shape, because the shell emitted no marks.
    Heuristic,
}

/// What kind of input the terminal is waiting for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalInputKind {
    Password,
    Confirm,
    Prompt,
}

/// One input the terminal is waiting for, with the evidence behind it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalInputExpected {
    pub kind: TerminalInputKind,
    pub evidence: String,
}

/// A command's exit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalExit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

/// What the terminal is doing, as the agent reads it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalState {
    pub running: bool,
    pub input_expected: Option<TerminalInputExpected>,
    pub exit: Option<TerminalExit>,
    pub source: TerminalSource,
}

/// One terminal as the strip draws it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSummary {
    pub id: u16,
    pub seat: SessionSlot,
    pub shell: String,
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
    /// Whether the shell process is alive.
    pub alive: bool,
    pub state: TerminalState,
    /// The title the shell last set, when it set one.
    pub title: String,
    /// Whether a hand-off is parked on it.
    pub held: bool,
}

/// A terminal hand-off: a session asking the person at a client to type into
/// the seat's terminal before it can carry on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalHandOff {
    pub id: uuid::Uuid,
    /// What the session needs done, in its own words.
    pub reason: String,
    /// The terminal the person is asked to look at.
    pub terminal: u16,
    /// Whether the program is reading a secret (ECHO off), so the card can
    /// say the typing is hidden.
    pub secret: bool,
}

/// One answer to a parked terminal hand-off: the ending, the door that sent
/// it and the connection that carried it.
pub type TerminalHandOffAnswer = crate::browser::HandOffAnswer;

#[cfg(test)]
mod tests {
    use super::*;

    /// The two binary tags are the ones the frame table names, and they do
    /// not collide with the dictation and browser kinds.
    #[test]
    fn the_binary_tags_are_the_next_two() {
        assert_eq!(TERMINAL_OUTPUT_TAG, 4);
        assert_eq!(TERMINAL_INPUT_TAG, 5);
        assert_ne!(TERMINAL_OUTPUT_TAG, TERMINAL_INPUT_TAG);
    }

    /// A hand-off crosses with its reason, its terminal and whether the
    /// typing is hidden.
    #[test]
    fn a_hand_off_crosses_with_its_reason_and_secret() {
        let handoff = TerminalHandOff {
            id: uuid::Uuid::from_u128(7),
            reason: "sudo password".to_owned(),
            terminal: 1,
            secret: true,
        };
        let encoded = serde_json::to_value(&handoff).expect("a hand-off encodes");
        assert_eq!(
            encoded,
            serde_json::json!({
                "id": "00000000-0000-0000-0000-000000000007",
                "reason": "sudo password",
                "terminal": 1,
                "secret": true,
            }),
        );
        assert_eq!(serde_json::from_value::<TerminalHandOff>(encoded).expect("decodes"), handoff);
    }

    /// A state crosses with its source, so a reader knows whether the answer
    /// is exact or a guess.
    #[test]
    fn a_state_crosses_with_its_source() {
        let state = TerminalState {
            running: false,
            input_expected: Some(TerminalInputExpected {
                kind: TerminalInputKind::Password,
                evidence: "ECHO off".to_owned(),
            }),
            exit: Some(TerminalExit { code: Some(7), signal: None }),
            source: TerminalSource::Exact,
        };
        let encoded = serde_json::to_value(&state).expect("a state encodes");
        assert_eq!(encoded["source"], "exact");
        assert_eq!(encoded["input_expected"]["kind"], "password");
        assert_eq!(encoded["exit"]["code"], 7);
        assert_eq!(serde_json::from_value::<TerminalState>(encoded).expect("decodes"), state);
    }

    /// A summary carries the seat, the shell and the cwd, so the strip can
    /// name the terminal without a second read.
    #[test]
    fn a_summary_carries_its_seat_and_shell() {
        let summary = TerminalSummary {
            id: 1,
            seat: SessionSlot::worker("O", "p", "w"),
            shell: "/bin/zsh".to_owned(),
            cwd: "/tmp".to_owned(),
            cols: 80,
            rows: 24,
            alive: true,
            state: TerminalState {
                running: false,
                input_expected: None,
                exit: None,
                source: TerminalSource::Heuristic,
            },
            title: String::new(),
            held: false,
        };
        let encoded = serde_json::to_value(&summary).expect("a summary encodes");
        assert_eq!(encoded["shell"], "/bin/zsh");
        assert_eq!(encoded["seat"]["label"], "w");
        assert_eq!(serde_json::from_value::<TerminalSummary>(encoded).expect("decodes"), summary);
    }
}
