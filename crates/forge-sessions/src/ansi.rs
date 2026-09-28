//! Stripping escape sequences from text a command produced.
//!
//! Both views draw output that came off a pty - a Monitor's watched
//! command, a Bash call's result - and both have to drop the sequences
//! before drawing it: ratatui would read them as terminal control, and a
//! page would print them as literal text.

/// Drop CSI and OSC escape sequences, keeping everything else. Bare
/// control bytes are the caller's business: a BEL ends an OSC, so it has
/// to survive this pass to be stripped by whatever follows.
pub fn strip_ansi(text: &str) -> String {
    enum State {
        Normal,
        Escape,
        Csi,
        Osc,
        OscEscape,
    }

    let mut out = String::with_capacity(text.len());
    let mut state = State::Normal;

    for ch in text.chars() {
        state = match state {
            State::Normal => {
                if ch == '\u{1b}' {
                    State::Escape
                } else {
                    out.push(ch);
                    State::Normal
                }
            }
            State::Escape => match ch {
                '[' => State::Csi,
                ']' => State::Osc,
                _ => State::Normal,
            },
            State::Csi => {
                if ('\u{40}'..='\u{7e}').contains(&ch) {
                    State::Normal
                } else {
                    State::Csi
                }
            }
            State::Osc => match ch {
                '\u{07}' => State::Normal,
                '\u{1b}' => State::OscEscape,
                _ => State::Osc,
            },
            State::OscEscape => {
                if ch == '\\' {
                    State::Normal
                } else {
                    State::Osc
                }
            }
        };
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_removes_csi_sequences() {
        let input = "\u{1b}[31mred\u{1b}[0m plain";
        assert_eq!(strip_ansi(input), "red plain");
    }

    #[test]
    fn strip_ansi_removes_osc_sequences() {
        let input = "prefix\u{1b}]0;title\u{07}suffix";
        assert_eq!(strip_ansi(input), "prefixsuffix");
    }
}
