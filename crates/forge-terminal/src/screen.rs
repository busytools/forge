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
/// **The cursor is a lifetime line number, not a buffer position.** The
/// scrollback is bounded, so the buffer's own count stops advancing once it
/// fills; `appended` is how many lines have ever entered it, and the buffer's
/// first line is `appended - total`. A caller that falls further behind than
/// the buffer gets the tail and `truncated` says so.
///
/// A read that reaches the buffer's last line hands back that line's index,
/// so the next read repeats it: the last line can still change while a
/// program is writing.
pub fn read<T: EventListener>(
    term: &Term<T>,
    appended: u64,
    since: Option<u64>,
    max_lines: usize,
    max_bytes: usize,
) -> ReadResult {
    let grid = term.grid();
    let top = grid.topmost_line().0;
    let bottom = grid.bottommost_line().0;
    let total = u64::try_from(bottom - top + 1).unwrap_or(0);
    let first = appended.saturating_sub(total);
    let tail = total.saturating_sub(u64::try_from(max_lines).unwrap_or(u64::MAX));
    // A cursor inside the buffer reads forward; a cursor the buffer has
    // already dropped means the caller fell behind, and the tail is what it
    // gets, reported as truncated. A cursor at the lifetime end is caught up.
    let (start, mut truncated) = match since {
        Some(since) if since >= first => (since - first, false),
        Some(_) => (0, true),
        None => (tail, tail > 0),
    };
    let start = start.min(total);
    let mut lines = Vec::new();
    let mut bytes = 0usize;
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
    let end = start.saturating_add(u64::try_from(lines.len()).unwrap_or(u64::MAX));
    let cursor = if end >= total && total > 0 { appended.saturating_sub(1) } else { first + end };
    ReadResult {
        text: lines.join("\n"),
        cursor,
        cols: u16::try_from(grid.columns()).unwrap_or(u16::MAX),
        rows: u16::try_from(grid.screen_lines()).unwrap_or(u16::MAX),
        truncated,
    }
}

/// The dimensions a resize drives the grid with.
struct GridSize {
    cols: usize,
    rows: usize,
}

impl Dimensions for GridSize {
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

/// Resize the grid, moving the lifetime read cursor with the reflow.
///
/// **A reflow is not output.** The grid's line count changes without a byte
/// arriving, so a cursor read against `appended - total` would slide to a
/// different line; moving the lifetime count by the same amount keeps the
/// buffer's first line where the cursor was measured against it.
pub(crate) fn resize<T: EventListener>(
    term: &mut Term<T>,
    cols: usize,
    rows: usize,
    appended: &mut u64,
) {
    let before = term.grid().total_lines();
    term.resize(GridSize { cols, rows });
    let after = term.grid().total_lines();
    if after >= before {
        *appended = appended.saturating_add(u64::try_from(after - before).unwrap_or(u64::MAX));
    } else {
        *appended = appended.saturating_sub(u64::try_from(before - after).unwrap_or(u64::MAX));
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
        driven(bytes, cols, rows, 100).0
    }

    /// A term fed `bytes`, and the lifetime line count a read cursor is
    /// measured against.
    fn driven(bytes: &[u8], cols: usize, rows: usize, history: usize) -> (Term<VoidListener>, u64) {
        let mut term = Term::new(
            Config { scrolling_history: history, ..Default::default() },
            &Size { cols, rows },
            VoidListener,
        );
        let mut appended = u64::try_from(term.grid().total_lines()).unwrap_or(0);
        let mut parser: Processor = Processor::new();
        crate::parser::advance(&mut parser, &mut term, bytes, history, &mut appended);
        (term, appended)
    }

    fn feed(
        term: &mut Term<VoidListener>,
        parser: &mut Processor,
        bytes: &[u8],
        history: usize,
        appended: &mut u64,
    ) {
        crate::parser::advance(parser, term, bytes, history, appended);
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
        let (term, appended) = driven(b"one\r\ntwo\r\nthree", 20, 5, 100);
        let all = read(&term, appended, None, 200, 32 * 1024);
        assert!(all.text.contains("one"));
        assert!(all.text.contains("three"));
        let after = read(&term, appended, Some(all.cursor), 200, 32 * 1024);
        assert!(after.text.is_empty(), "nothing new after the cursor: {:?}", after.text);
        assert!(!after.truncated, "caught up is not truncated");
    }

    /// The bounds are honoured and `truncated` says so, so a read never
    /// floods the model.
    #[test]
    fn a_read_is_bounded_and_says_when_it_truncated() {
        let (term, appended) = driven(b"a\r\nb\r\nc\r\nd\r\ne", 20, 5, 100);
        let bounded = read(&term, appended, None, 2, 32 * 1024);
        assert!(bounded.truncated, "two lines of five is truncated");
        assert_eq!(bounded.text.lines().count(), 2);
    }

    /// A bound that cuts a read leaves the cursor on the last line it
    /// returned, so the lines it did not return are still reachable.
    #[test]
    fn a_bounded_read_leaves_the_cursor_at_the_last_line_it_returned() {
        let (term, appended) = driven(b"a\r\nb\r\nc\r\nd\r\ne", 20, 5, 100);
        let first = read(&term, appended, Some(0), 2, 32 * 1024);
        assert_eq!(first.text.lines().collect::<Vec<_>>(), ["a", "b"]);
        assert!(first.truncated, "the bound cut the read");
        let next = read(&term, appended, Some(first.cursor), 10, 32 * 1024);
        assert_eq!(next.text.lines().collect::<Vec<_>>(), ["c", "d", "e"]);
        assert!(!next.truncated, "the second read reached the end");
    }

    /// Once the scrollback is full the buffer's line count stops growing;
    /// the cursor is a lifetime line number, so it keeps advancing and a
    /// read still returns what was written after it.
    #[test]
    fn a_read_cursor_advances_after_the_scrollback_is_full() {
        let mut term = Term::new(
            Config { scrolling_history: 2, ..Default::default() },
            &Size { cols: 20, rows: 2 },
            VoidListener,
        );
        let mut appended = u64::try_from(term.grid().total_lines()).unwrap_or(0);
        let mut parser: Processor = Processor::new();
        feed(&mut term, &mut parser, b"1\r\n2\r\n3\r\n4\r\n5\r\n6\r\n7\r\n8\r\n", 2, &mut appended);
        let caught = read(&term, appended, None, 10, 32 * 1024);
        assert_eq!(caught.text.lines().collect::<Vec<_>>(), ["6", "7", "8"]);
        let bound = caught.cursor;
        feed(&mut term, &mut parser, b"9\r\n10\r\n", 2, &mut appended);
        let next = read(&term, appended, Some(bound), 10, 32 * 1024);
        assert_eq!(next.text.lines().collect::<Vec<_>>(), ["9", "10"]);
        assert!(!next.truncated, "nothing fell out of the buffer behind the cursor");
    }

    /// **A wrapped line reflows, and its characters survive.** A terminal
    /// narrowed under a running program re-lays the line rather than
    /// truncating it, which is what a person's own terminal does.
    #[test]
    fn a_wrapped_line_reflows_when_the_terminal_narrows() {
        let mut term = Term::new(
            Config { scrolling_history: 100, ..Default::default() },
            &Size { cols: 20, rows: 12 },
            VoidListener,
        );
        let mut appended = 12u64;
        let mut parser: Processor = Processor::new();
        feed(
            &mut term,
            &mut parser,
            b"\x1b[32mok\x1b[0m aaaaaaaaaaaaaaaaaaaaaaaa\r\n\x1b[1mbold\x1b[0m done",
            100,
            &mut appended,
        );
        let wide = read(&term, 10_000, None, 500, 1 << 20);
        let wide_flat: String = wide.text.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(wide_flat.contains("okaaaaaaaaaaaaaaaaaaaaaaaa"), "{:?}", wide.text);
        term.resize(Size { cols: 10, rows: 12 });
        // The buffer, not the viewport: a reflow can move a row into the
        // scrollback, and the characters are what must survive.
        let narrow = read(&term, 10_000, None, 500, 1 << 20);
        let flat: String = narrow.text.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            flat.contains("okaaaaaaaaaaaaaaaaaaaaaaaa"),
            "the wrapped run survives the reflow: {:?}",
            narrow.text
        );
        assert!(flat.contains("bolddone"), "{:?}", narrow.text);
    }

    /// **A reflow does not move the read window.** A cursor is a lifetime
    /// line number read against `appended - total`; the resize changes the
    /// grid's line count without output, so the count moves with it and the
    /// buffer's first line stays where the cursor was measured against it.
    #[test]
    fn a_reflow_keeps_the_read_windows_first_line() {
        let mut term = Term::new(
            Config { scrolling_history: 100, ..Default::default() },
            &Size { cols: 10, rows: 4 },
            VoidListener,
        );
        let mut appended = u64::try_from(term.grid().total_lines()).unwrap_or(0);
        let mut parser: Processor = Processor::new();
        feed(
            &mut term,
            &mut parser,
            b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n",
            100,
            &mut appended,
        );
        let first = appended.saturating_sub(u64::try_from(term.grid().total_lines()).unwrap_or(0));
        // Widening reflows the wrapped line into one row: the grid shrinks
        // without a byte arriving.
        resize(&mut term, 40, 4, &mut appended);
        let after = appended.saturating_sub(u64::try_from(term.grid().total_lines()).unwrap_or(0));
        assert_eq!(after, first, "the reflow moved the buffer's first line under the cursor");
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
