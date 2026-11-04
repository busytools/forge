//! The screen as text: what the agent reads, and what a reattaching client is
//! repainted from.
//!
//! **Plain text, never raw escapes.** A tool result carries the characters a
//! person would see, with trailing blanks trimmed; the client gets the same
//! grid through the repaint in [`crate::repaint`], not through this.

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::Line;
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::{Cell, Flags};

/// Where the cursor sits on the visible screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub col: u16,
    pub row: u16,
}

/// The visible screen as text, with the cursor and the size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Screen {
    pub text: String,
    pub cursor: Cursor,
    pub cols: u16,
    pub rows: u16,
}

/// One bounded read of the scrollback and the screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadResult {
    pub text: String,
    /// The cursor to pass as `since` on the next read.
    pub cursor: u64,
    pub cols: u16,
    pub rows: u16,
    pub truncated: bool,
}

/// The visible screen, as text.
pub fn extract<T: EventListener>(term: &Term<T>) -> Screen {
    let grid = term.grid();
    let rows = grid.screen_lines();
    let cols = grid.columns();
    let mut lines = Vec::with_capacity(rows);
    for row in 0..rows {
        lines.push(line_text(grid, Line(i32::try_from(row).unwrap_or(i32::MAX))));
    }
    let point = grid.cursor.point;
    let cursor = Cursor {
        col: u16::try_from(point.column.0).unwrap_or(u16::MAX),
        row: u16::try_from(
            point.line.0.clamp(0, i32::try_from(rows.saturating_sub(1)).unwrap_or(i32::MAX)),
        )
        .unwrap_or(0),
    };
    Screen {
        text: lines.join("\n"),
        cursor,
        cols: u16::try_from(cols).unwrap_or(u16::MAX),
        rows: u16::try_from(rows).unwrap_or(u16::MAX),
    }
}

/// A bounded read of the buffer from `since`, or the tail when `since` is
/// absent or older than the buffer.
///
/// **The cursor is the buffer's line count, not a lifetime total.** The
/// scrollback is bounded, so a caller that falls further behind than the
/// buffer gets the tail and `truncated` says so rather than pretending it
/// read everything.
pub fn read<T: EventListener>(
    term: &Term<T>,
    since: Option<u64>,
    max_lines: usize,
    max_bytes: usize,
) -> ReadResult {
    let grid = term.grid();
    let top = grid.topmost_line().0;
    let bottom = grid.bottommost_line().0;
    let total = u64::try_from(bottom - top + 1).unwrap_or(0);
    let tail = total.saturating_sub(u64::try_from(max_lines).unwrap_or(u64::MAX));
    // A cursor inside the buffer reads forward; a cursor past it means the
    // caller fell further behind than the scrollback holds, and the tail is
    // what it gets, reported as truncated.
    let (start, fell_behind) = match since {
        Some(since) if since <= total => (since, false),
        _ => (tail, since.is_some_and(|since| since > total)),
    };
    let mut lines = Vec::new();
    let mut bytes = 0usize;
    let mut truncated = fell_behind || start > 0;
    for index in start..total {
        let Ok(offset) = i32::try_from(index) else { break };
        let text = line_text(grid, Line(top + offset));
        let len = text.len() + 1;
        if lines.len() >= max_lines || bytes + len > max_bytes {
            truncated = true;
            break;
        }
        bytes += len;
        lines.push(text);
    }
    ReadResult {
        text: lines.join("\n"),
        cursor: total,
        cols: u16::try_from(grid.columns()).unwrap_or(u16::MAX),
        rows: u16::try_from(grid.screen_lines()).unwrap_or(u16::MAX),
        truncated,
    }
}

/// One grid line as text, trailing blanks trimmed.
fn line_text(grid: &Grid<Cell>, line: Line) -> String {
    let row = &grid[line];
    let mut text = String::with_capacity(row.len());
    for cell in row {
        if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
            continue;
        }
        text.push(cell.c);
    }
    let trimmed = text.trim_end();
    trimmed.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::Config;
    use alacritty_terminal::vte::ansi::Processor;

    struct Size {
        cols: usize,
        rows: usize,
    }

    impl alacritty_terminal::grid::Dimensions for Size {
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

    /// The extractor returns the characters a person sees, with trailing
    /// blanks trimmed and the cursor where the program left it.
    #[test]
    fn the_screen_is_the_text_a_person_sees() {
        let term = term_with(b"hello\r\nworld", 20, 5);
        let screen = extract(&term);
        assert_eq!(screen.text.lines().next(), Some("hello"));
        assert_eq!(screen.text.lines().nth(1), Some("world"));
        assert_eq!(screen.cursor, Cursor { col: 5, row: 1 });
        assert_eq!((screen.cols, screen.rows), (20, 5));
    }

    /// A read from a cursor returns only the lines after it, and the cursor
    /// moves to the end.
    #[test]
    fn a_read_from_a_cursor_returns_the_new_lines() {
        let term = term_with(b"one\r\ntwo\r\nthree", 20, 5);
        let all = read(&term, None, 200, 32 * 1024);
        assert!(all.text.contains("one"));
        assert!(all.text.contains("three"));
        let after = read(&term, Some(all.cursor), 200, 32 * 1024);
        assert!(after.text.is_empty(), "nothing new after the cursor: {:?}", after.text);
    }

    /// The bounds are honoured and `truncated` says so, so a read never
    /// floods the model.
    #[test]
    fn a_read_is_bounded_and_says_when_it_truncated() {
        let term = term_with(b"a\r\nb\r\nc\r\nd\r\ne", 20, 5);
        let bounded = read(&term, None, 2, 32 * 1024);
        assert!(bounded.truncated, "two lines of five is truncated");
        assert_eq!(bounded.text.lines().count(), 2);
    }

    /// A wide character occupies one cell and is extracted once, not as its
    /// spacer.
    #[test]
    fn a_wide_character_is_extracted_once() {
        let term = term_with("日本".as_bytes(), 20, 5);
        let screen = extract(&term);
        assert_eq!(screen.text.lines().next(), Some("日本"));
    }
}
