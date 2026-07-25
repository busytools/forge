//! Cadence + cell style for the focused-input block cursor.
//!
//! forge drives the blink itself (rather than leaning on the terminal's
//! native cursor blink) so the cursor looks identical across terminals:
//! a solid reverse-video block for one interval, hidden the next. The
//! render loop keeps a focused input redrawing on this cadence.

use ratatui::style::{Modifier, Style};
use std::time::Duration;

/// Half-period of the cursor blink: the block is visible for one
/// interval, then hidden for the next.
pub const CURSOR_BLINK_INTERVAL: Duration = Duration::from_millis(530);

/// Blink phase for `elapsed` since the blink epoch: `true` = the cursor
/// block is visible (on-phase), `false` = hidden (off-phase).
pub fn blink_on(elapsed: Duration) -> bool {
    let interval = CURSOR_BLINK_INTERVAL.as_millis();
    interval != 0 && (elapsed.as_millis() / interval).is_multiple_of(2)
}

/// Cursor-cell style for a focused input at blink phase `on`: a solid
/// reverse-video block when visible, invisible (default) when hidden.
pub fn cursor_style(on: bool) -> Style {
    if on { Style::default().add_modifier(Modifier::REVERSED) } else { Style::default() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_is_visible_on_the_first_interval_and_hidden_on_the_next() {
        assert!(blink_on(Duration::ZERO), "cursor starts visible");
        assert!(
            blink_on(CURSOR_BLINK_INTERVAL.saturating_sub(Duration::from_millis(1))),
            "still visible just before the first flip",
        );
        assert!(!blink_on(CURSOR_BLINK_INTERVAL), "hidden on the second interval");
        assert!(blink_on(CURSOR_BLINK_INTERVAL * 2), "visible again on the third interval");
    }

    #[test]
    fn on_phase_reverses_the_cell_off_phase_hides_it() {
        assert!(
            cursor_style(true).add_modifier.contains(Modifier::REVERSED),
            "on-phase paints a reverse-video block",
        );
        assert_eq!(cursor_style(false), Style::default(), "off-phase hides the cursor");
    }
}
