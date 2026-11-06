//! The parser's view of the screen, with the one count the grid cannot give.
//!
//! **`Term` keeps only the lines the scrollback still holds.** Its line count
//! stops at the cap, so a read cursor measured in buffer positions stops
//! advancing once the buffer fills. This wrapper forwards every handler to the
//! term and counts the linefeeds that scroll a full screen, which are exactly
//! the lines the buffer drops.

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Line;
use alacritty_terminal::term::Term;
use alacritty_terminal::vte::ansi::{
    Attr, CharsetIndex, ClearMode, CursorShape, CursorStyle, Handler, Hyperlink, KeyboardModes,
    KeyboardModesApplyBehavior, LineClearMode, Mode, ModifyOtherKeys, PrivateMode, Processor, Rgb,
    ScpCharPath, ScpUpdateMode, StandardCharset, TabulationClearMode,
};

/// Drive `parser` over `bytes`, then add the lines that entered the buffer to
/// `appended` - the lifetime count a read cursor is measured in.
pub(crate) fn advance<T: EventListener>(
    parser: &mut Processor,
    term: &mut Term<T>,
    bytes: &[u8],
    history: usize,
    appended: &mut u64,
) {
    let mut counted = Counted::new(term, history);
    parser.advance(&mut counted, bytes);
    *appended = appended.saturating_add(counted.finish());
}

/// One parse's worth of counting.
struct Counted<'a, T: EventListener> {
    term: &'a mut Term<T>,
    /// The buffer's line count when the parse started.
    total: usize,
    /// The configured scrollback cap, in lines.
    history: usize,
    /// Lines this parse pushed out of a full buffer.
    scrolled: u64,
}

impl<'a, T: EventListener> Counted<'a, T> {
    fn new(term: &'a mut Term<T>, history: usize) -> Self {
        let total = term.grid().total_lines();
        Self { term, total, history, scrolled: 0 }
    }

    /// The lines that entered the buffer: the growth it shows, plus the lines
    /// it dropped once it was full.
    fn finish(self) -> u64 {
        let growth = self.term.grid().total_lines().saturating_sub(self.total);
        u64::try_from(growth).unwrap_or(u64::MAX).saturating_add(self.scrolled)
    }

    /// Forward one call, and count it when it scrolled a full screen: the
    /// cursor line is unchanged only when the linefeed scrolled the region,
    /// and only a region ending at the screen's last line reaches the history.
    fn scrolled_linefeed(&mut self, scrolls: impl FnOnce(&mut Term<T>)) {
        let before = self.term.grid().cursor.point.line;
        scrolls(self.term);
        let grid = self.term.grid();
        let last = Line(i32::try_from(grid.screen_lines().saturating_sub(1)).unwrap_or(i32::MAX));
        let full = grid.total_lines() >= grid.screen_lines() + self.history;
        if grid.cursor.point.line == before && before == last && full {
            self.scrolled = self.scrolled.saturating_add(1);
        }
    }
}

macro_rules! forward {
    ($($name:ident($($arg:ident: $ty:ty),*);)*) => {
        $(
            fn $name(&mut self, $($arg: $ty),*) {
                self.term.$name($($arg),*);
            }
        )*
    };
}

impl<T: EventListener> Handler for Counted<'_, T> {
    fn linefeed(&mut self) {
        self.scrolled_linefeed(Handler::linefeed);
    }

    fn newline(&mut self) {
        self.scrolled_linefeed(Handler::newline);
    }

    forward!(
        set_title(a0: Option<String>);
        set_cursor_style(a0: Option<CursorStyle>);
        set_cursor_shape(a0: CursorShape);
        input(a0: char);
        goto(a0: i32, a1: usize);
        goto_line(a0: i32);
        goto_col(a0: usize);
        insert_blank(a0: usize);
        move_up(a0: usize);
        move_down(a0: usize);
        identify_terminal(a0: Option<char>);
        device_status(a0: usize);
        move_forward(a0: usize);
        move_backward(a0: usize);
        move_down_and_cr(a0: usize);
        move_up_and_cr(a0: usize);
        put_tab(a0: u16);
        backspace();
        carriage_return();
        bell();
        substitute();
        set_horizontal_tabstop();
        scroll_up(a0: usize);
        scroll_down(a0: usize);
        insert_blank_lines(a0: usize);
        delete_lines(a0: usize);
        erase_chars(a0: usize);
        delete_chars(a0: usize);
        move_backward_tabs(a0: u16);
        move_forward_tabs(a0: u16);
        save_cursor_position();
        restore_cursor_position();
        clear_line(a0: LineClearMode);
        clear_screen(a0: ClearMode);
        clear_tabs(a0: TabulationClearMode);
        set_tabs(a0: u16);
        reset_state();
        reverse_index();
        terminal_attribute(a0: Attr);
        set_mode(a0: Mode);
        unset_mode(a0: Mode);
        report_mode(a0: Mode);
        set_private_mode(a0: PrivateMode);
        unset_private_mode(a0: PrivateMode);
        report_private_mode(a0: PrivateMode);
        set_scrolling_region(a0: usize, a1: Option<usize>);
        set_keypad_application_mode();
        unset_keypad_application_mode();
        set_active_charset(a0: CharsetIndex);
        configure_charset(a0: CharsetIndex, a1: StandardCharset);
        set_color(a0: usize, a1: Rgb);
        dynamic_color_sequence(a0: String, a1: usize, a2: &str);
        reset_color(a0: usize);
        clipboard_store(a0: u8, a1: &[u8]);
        clipboard_load(a0: u8, a1: &str);
        decaln();
        push_title();
        pop_title();
        text_area_size_pixels();
        text_area_size_chars();
        set_hyperlink(a0: Option<Hyperlink>);
        report_keyboard_mode();
        push_keyboard_mode(a0: KeyboardModes);
        pop_keyboard_modes(a0: u16);
        set_keyboard_mode(a0: KeyboardModes, a1: KeyboardModesApplyBehavior);
        set_modify_other_keys(a0: ModifyOtherKeys);
        report_modify_other_keys();
        set_scp(a0: ScpCharPath, a1: ScpUpdateMode);
    );
}
