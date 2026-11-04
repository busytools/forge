//! The attach repaint: the visible screen as a VT stream a client can draw.
//!
//! `alacritty_terminal` has no formatted dump, so forge writes one. A client
//! that attaches late, reconnects or falls behind is sent this instead of the
//! bytes it missed: the grid's characters, their colours and attributes, and
//! the cursor, as the escape sequences xterm.js already parses.
//!
//! **The repaint is the screen, not the history.** A reattaching client gets
//! what is visible; the scrollback is the server's, and `terminal__read` is
//! how an agent reads it.

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color, NamedColor};

/// The visible screen as a VT stream, ending with the cursor where the
/// program left it.
pub fn repaint<T: EventListener>(term: &Term<T>) -> Vec<u8> {
    let grid = term.grid();
    let rows = grid.screen_lines();
    let cols = grid.columns();
    let mut out = Vec::with_capacity(rows * cols * 2);
    out.extend_from_slice(b"\x1b[0m\x1b[H\x1b[2J");
    let mut current: Option<Style> = None;
    for row in 0..rows {
        // Position each row explicitly: a full-width row would otherwise
        // wrap on its own and shift every row after it.
        out.extend_from_slice(format!("\x1b[{};1H", row + 1).as_bytes());
        let line = Line(i32::try_from(row).unwrap_or(i32::MAX));
        for col in 0..cols {
            let cell = &grid[line][Column(col)];
            if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }
            let style = Style { fg: cell.fg, bg: cell.bg, flags: cell.flags };
            if current != Some(style) {
                out.extend_from_slice(b"\x1b[0m");
                out.extend_from_slice(style.sgr().as_bytes());
                current = Some(style);
            }
            let mut buf = [0u8; 4];
            out.extend_from_slice(cell.c.encode_utf8(&mut buf).as_bytes());
        }
    }
    out.extend_from_slice(b"\x1b[0m");
    let point = grid.cursor.point;
    let last_row = i32::try_from(rows.saturating_sub(1)).unwrap_or(i32::MAX);
    let row = point.line.0.clamp(0, last_row) + 1;
    let col = point.column.0 + 1;
    out.extend_from_slice(format!("\x1b[{row};{col}H").as_bytes());
    out
}

/// One cell's drawing attributes, compared to decide when to emit an SGR.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Style {
    fg: Color,
    bg: Color,
    flags: Flags,
}

impl Style {
    fn sgr(self) -> String {
        let mut parts = Vec::new();
        if self.flags.contains(Flags::BOLD) {
            parts.push("1".to_owned());
        }
        if self.flags.contains(Flags::DIM) {
            parts.push("2".to_owned());
        }
        if self.flags.contains(Flags::ITALIC) {
            parts.push("3".to_owned());
        }
        if self.flags.intersects(Flags::ALL_UNDERLINES) {
            parts.push("4".to_owned());
        }
        if self.flags.contains(Flags::INVERSE) {
            parts.push("7".to_owned());
        }
        if self.flags.contains(Flags::HIDDEN) {
            parts.push("8".to_owned());
        }
        if self.flags.contains(Flags::STRIKEOUT) {
            parts.push("9".to_owned());
        }
        parts.push(fg_sgr(self.fg));
        parts.push(bg_sgr(self.bg));
        format!("\x1b[{}m", parts.join(";"))
    }
}

fn fg_sgr(color: Color) -> String {
    match color {
        Color::Named(named) => match named {
            NamedColor::Foreground
            | NamedColor::Background
            | NamedColor::Cursor
            | NamedColor::BrightForeground
            | NamedColor::DimForeground => "39".to_owned(),
            other => {
                let index = named_index(other);
                if index < 8 { format!("{}", 30 + index) } else { format!("{}", 90 + index - 8) }
            }
        },
        Color::Indexed(index) => format!("38;5;{index}"),
        Color::Spec(rgb) => format!("38;2;{};{};{}", rgb.r, rgb.g, rgb.b),
    }
}

fn bg_sgr(color: Color) -> String {
    match color {
        Color::Named(named) => match named {
            NamedColor::Background
            | NamedColor::Foreground
            | NamedColor::Cursor
            | NamedColor::BrightForeground
            | NamedColor::DimForeground => "49".to_owned(),
            other => {
                let index = named_index(other);
                if index < 8 { format!("{}", 40 + index) } else { format!("{}", 100 + index - 8) }
            }
        },
        Color::Indexed(index) => format!("48;5;{index}"),
        Color::Spec(rgb) => format!("48;2;{};{};{}", rgb.r, rgb.g, rgb.b),
    }
}

/// The 0..15 index of a named colour, folding the dim variants onto their
/// base colour.
fn named_index(named: NamedColor) -> u8 {
    match named {
        NamedColor::Black | NamedColor::DimBlack => 0,
        NamedColor::Red | NamedColor::DimRed => 1,
        NamedColor::Green | NamedColor::DimGreen => 2,
        NamedColor::Yellow | NamedColor::DimYellow => 3,
        NamedColor::Blue | NamedColor::DimBlue => 4,
        NamedColor::Magenta | NamedColor::DimMagenta => 5,
        NamedColor::Cyan | NamedColor::DimCyan => 6,
        NamedColor::White
        | NamedColor::DimWhite
        | NamedColor::Foreground
        | NamedColor::Background
        | NamedColor::Cursor
        | NamedColor::BrightForeground
        | NamedColor::DimForeground => 7,
        NamedColor::BrightBlack => 8,
        NamedColor::BrightRed => 9,
        NamedColor::BrightGreen => 10,
        NamedColor::BrightYellow => 11,
        NamedColor::BrightBlue => 12,
        NamedColor::BrightMagenta => 13,
        NamedColor::BrightCyan => 14,
        NamedColor::BrightWhite => 15,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::term::Config;
    use alacritty_terminal::vte::ansi::Processor;

    struct Size {
        cols: usize,
        rows: usize,
    }

    impl Dimensions for Size {
        fn total_lines(&self) -> usize {
            self.rows
        }
        fn screen_lines(&self) -> usize {
            self.rows
        }
        fn columns(&self) -> usize {
            self.cols
        }
    }

    fn term_with(bytes: &[u8], cols: usize, rows: usize) -> Term<VoidListener> {
        let mut term = Term::new(
            Config { scrolling_history: 100, ..Default::default() },
            &Size { cols, rows },
            VoidListener,
        );
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, bytes);
        term
    }

    /// **The repaint reproduces the screen.** Feeding it into a fresh
    /// terminal yields the same text and cursor, which is what a reattaching
    /// client needs.
    #[test]
    fn the_repaint_reproduces_the_screen() {
        let source = term_with(b"hello\r\n\x1b[31mred\x1b[0m world", 20, 5);
        let bytes = repaint(&source);
        let repainted = term_with(&bytes, 20, 5);
        let before = crate::screen::extract(&source);
        let after = crate::screen::extract(&repainted);
        assert_eq!(after.text, before.text, "the text survives the repaint");
        assert_eq!(after.cursor, before.cursor, "and so does the cursor");
    }

    /// A coloured cell carries its colour, so a reattaching client draws the
    /// same screen rather than a monochrome one.
    #[test]
    fn a_coloured_cell_carries_its_colour() {
        let term = term_with(b"\x1b[31mred", 20, 5);
        let bytes = repaint(&term);
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("\x1b[0m\x1b[31;49m"), "the red foreground is emitted: {text:?}");
    }

    /// A truecolour cell carries its RGB, not a nearest named colour.
    #[test]
    fn a_truecolour_cell_carries_its_rgb() {
        let term = term_with(b"\x1b[38;2;1;2;3mX", 20, 5);
        let bytes = repaint(&term);
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("38;2;1;2;3"), "{text:?}");
    }

    /// The repaint ends with the cursor where the program left it, so a
    /// client that attaches mid-line types in the right place.
    #[test]
    fn the_repaint_ends_at_the_cursor() {
        let term = term_with(b"abc", 20, 5);
        let bytes = repaint(&term);
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.ends_with("\x1b[1;4H"), "the cursor is after 'abc': {text:?}");
    }
}
