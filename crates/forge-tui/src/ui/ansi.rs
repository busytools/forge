//! What the terminal drops from text a command produced before drawing it.
//!
//! Pretty much every real command's output carries ANSI: `cargo build`,
//! `npm install` and anything with a progress bar write colour codes,
//! carriage-return redraws and the odd BEL or backspace. ratatui reads those
//! as terminal control, so they go before the text reaches a span.

/// Drop the escape sequences a terminal obeys, keeping everything else.
/// Bare control bytes are the caller's business: a BEL ends an OSC, so it
/// has to survive this pass to be stripped by whatever follows.
pub fn strip_ansi(text: &str) -> String {
    enum State {
        Normal,
        Escape,
        EscapeIntermediate,
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
                // An escape carrying an intermediate byte is three long, not
                // two: `sgr0` is `\E(B\E[m`, and stopping after the `(` leaves
                // the `B` on the page where a terminal drew nothing.
                '\u{20}'..='\u{2f}' => State::EscapeIntermediate,
                _ => State::Normal,
            },
            State::EscapeIntermediate => State::Normal,
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

/// [`strip_ansi`] plus the control bytes that are not escape sequences.
///
/// Two-stage because they are two different things: the sequences are what
/// the command meant as formatting, while `\r`, `\b`, BEL and `\u{0C}` are
/// what it meant as movement, and a terminal told to redraw has no business
/// doing it over forge's own frame.
pub fn sanitize_for_render(raw: &str) -> String {
    strip_ansi(raw)
        .chars()
        .filter(|c| !matches!(c, '\r' | '\u{08}' | '\u{07}' | '\u{0C}' | '\u{1B}'))
        .collect()
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

    #[test]
    fn strip_ansi_removes_an_escape_that_carries_an_intermediate_byte() {
        // `sgr0` on this machine's terminfo is `\E(B\E[m`, so reading a
        // non-CSI escape as two bytes leaves a bare `B` behind where a reset
        // was emitted - which is what most tools print.
        assert_eq!(strip_ansi("before \u{1b}(B\u{1b}[m after"), "before  after");
        assert_eq!(strip_ansi("\u{1b})0plain"), "plain");
        assert_eq!(strip_ansi("\u{1b}#8"), "");
    }

    #[test]
    fn sanitize_for_render_drops_the_movement_bytes_too() {
        let input = "one\rtwo\u{08}\u{0c}three\u{07}";
        assert_eq!(
            sanitize_for_render(input),
            "onetwothree",
            "a carriage return, a backspace, a form feed and a BEL are movement rather than text",
        );
    }

    /// Both halves of the job on the shape a real command's output arrives
    /// in: the tail reader hands over what `cargo build` wrote, and the
    /// sequences and the movement bytes both have to come off it.
    #[test]
    fn sanitize_for_render_cleans_a_commands_own_tail() {
        let raw = "\u{1b}[32mfresh\u{1b}[0m\rold\u{1b}[K\u{08}\u{07}";
        assert_eq!(
            sanitize_for_render(raw),
            "freshold",
            "a colour, a carriage return, an erase-line and a backspace all go, and the words stay",
        );
    }
}
