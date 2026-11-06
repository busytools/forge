//! The named keys an agent may press, and the bytes each one sends.
//!
//! **A key is bytes, not a command.** The shell and every program in it see
//! an ordinary terminal; Enter is its own key, Ctrl-C is 0x03, and the arrows
//! are the sequences an `xterm-256color` terminal sends. Nothing here names an
//! agent.

use serde::{Deserialize, Serialize};

/// One named key, pressed in order with the text a call carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Key {
    Enter,
    Tab,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Backspace,
    CtrlC,
    CtrlD,
    CtrlZ,
    CtrlL,
}

impl Key {
    /// The bytes this key sends to the PTY.
    pub const fn bytes(self) -> &'static [u8] {
        match self {
            Self::Enter => b"\r",
            Self::Tab => b"\t",
            Self::Escape => b"\x1b",
            Self::Up => b"\x1b[A",
            Self::Down => b"\x1b[B",
            Self::Right => b"\x1b[C",
            Self::Left => b"\x1b[D",
            Self::Home => b"\x1b[H",
            Self::End => b"\x1b[F",
            Self::PageUp => b"\x1b[5~",
            Self::PageDown => b"\x1b[6~",
            Self::Backspace => b"\x7f",
            Self::CtrlC => b"\x03",
            Self::CtrlD => b"\x04",
            Self::CtrlZ => b"\x1a",
            Self::CtrlL => b"\x0c",
        }
    }

    /// Parse the wire spelling a tool call carries.
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "enter" => Self::Enter,
            "tab" => Self::Tab,
            "escape" => Self::Escape,
            "up" => Self::Up,
            "down" => Self::Down,
            "left" => Self::Left,
            "right" => Self::Right,
            "home" => Self::Home,
            "end" => Self::End,
            "page_up" => Self::PageUp,
            "page_down" => Self::PageDown,
            "backspace" => Self::Backspace,
            "ctrl_c" => Self::CtrlC,
            "ctrl_d" => Self::CtrlD,
            "ctrl_z" => Self::CtrlZ,
            "ctrl_l" => Self::CtrlL,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Enter is a carriage return, not a newline.** A terminal's Enter key
    /// sends `\r`; a `\n` is Ctrl-J and a program that reads raw keys sees a
    /// different key, and zsh does not accept a line on one.
    #[test]
    fn enter_is_a_carriage_return() {
        assert_eq!(Key::Enter.bytes(), b"\r");
        assert_ne!(Key::Enter.bytes(), b"\n");
    }

    /// The control keys are the terminal's own bytes, so a program sees the
    /// key a person pressed rather than an escape sequence.
    #[test]
    fn the_control_keys_are_their_own_bytes() {
        assert_eq!(Key::CtrlC.bytes(), b"\x03");
        assert_eq!(Key::CtrlD.bytes(), b"\x04");
        assert_eq!(Key::CtrlZ.bytes(), b"\x1a");
        assert_eq!(Key::CtrlL.bytes(), b"\x0c");
    }

    /// Every wire spelling parses back to the key it names, so a tool call's
    /// `keys` array cannot silently drop one.
    #[test]
    fn every_wire_spelling_parses() {
        for (name, key) in [
            ("enter", Key::Enter),
            ("tab", Key::Tab),
            ("escape", Key::Escape),
            ("up", Key::Up),
            ("down", Key::Down),
            ("left", Key::Left),
            ("right", Key::Right),
            ("home", Key::Home),
            ("end", Key::End),
            ("page_up", Key::PageUp),
            ("page_down", Key::PageDown),
            ("backspace", Key::Backspace),
            ("ctrl_c", Key::CtrlC),
            ("ctrl_d", Key::CtrlD),
            ("ctrl_z", Key::CtrlZ),
            ("ctrl_l", Key::CtrlL),
        ] {
            assert_eq!(Key::parse(name), Some(key), "{name}");
        }
        assert_eq!(Key::parse("ctrl_q"), None, "an unknown key is refused");
    }
}
