use ratatui::layout::{Constraint, Layout, Rect};

/// Minimum terminal width (columns) at which the Wide tier kicks in.
/// At Wide tier the Projects pane gets its own slot on the left of
/// the body at full width, and the Inspector pane gets its own slot
/// on the right. Below this we fall back to Medium tier (compact
/// panes) and eventually to Narrow tier (top bar + on-demand overlays).
pub const WIDE_TIER_MIN_WIDTH: u16 = 160;

/// Minimum terminal width (columns) at which the Medium tier kicks
/// in. Between this and `WIDE_TIER_MIN_WIDTH` both side panes render
/// inline at compact width with truncation. Below this both panes
/// collapse to top-bar icons (Narrow tier overlay).
pub const MEDIUM_TIER_MIN_WIDTH: u16 = 120;

/// Width (columns) of each side pane at Wide tier.
pub const PANE_WIDTH_WIDE: u16 = 40;

/// Width (columns) of each side pane at Medium tier.
pub const PANE_WIDTH_MEDIUM: u16 = 30;

/// Width (columns) of the vertical separator column between a side
/// pane and the chat column when the pane is visible.
pub const PANE_SEPARATOR_WIDTH: u16 = 1;

/// 1-col gutter between the separator and the chat column so chat
/// content doesn't visually collide with the `│` rule. Same gutter
/// applies on both sides when both panes are visible.
const CHAT_PADDING: u16 = 1;

#[derive(Clone, Default)]
pub struct AppLayout {
    /// Single-line top bar rect, allocated only at Narrow tier
    /// (`area.width < MEDIUM_TIER_MIN_WIDTH`). `None` at Wide /
    /// Medium tiers — those use the inline left + right panes
    /// instead. Hosts the `▤` Projects icon on the left and the
    /// `▦` Inspector icon on the right.
    pub top_bar: Option<Rect>,
    /// Left-side Projects pane rect when the user has the pane
    /// visible AND the terminal is at Wide / Medium tier.
    /// `None` whenever the pane should not be rendered. Spans the
    /// full terminal height when allocated so the pane and its
    /// bottom-anchored content stay confined to the left column.
    pub pane: Option<Rect>,
    /// Single-column vertical-rule rect between the left pane and
    /// the chat column. Allocated only when `pane` is also allocated.
    /// Rendered as a column of `│` glyphs by the chat draw path.
    pub pane_separator: Option<Rect>,
    /// Right-side Inspector pane rect (mirror of `pane` on the
    /// right). Allocated when the user has it visible AND the
    /// terminal is at Wide / Medium tier.
    pub pane_right: Option<Rect>,
    /// Single-column vertical-rule rect between the chat column and
    /// the right Inspector pane. Allocated only when `pane_right` is.
    pub pane_right_separator: Option<Rect>,
    /// Chat body. When either pane is allocated this rect lives
    /// inside the chat column, between the side-pane separators.
    pub body: Rect,
    pub input_sep: Rect,
    pub input: Rect,
    pub input_bottom_sep: Rect,
    pub help: Rect,
}

/// Compute the layout rectangles for one frame.
///
/// `pane_visible` toggles the left Projects pane (persisted state).
/// `pane_right_visible` toggles the right Inspector pane (same
/// persistence model, mirror state). Either / both may be true; the
/// resulting chat column shrinks accordingly. Both vanish at Narrow
/// tier — the top bar hosts the icon affordances there instead.
pub fn compute(
    area: Rect,
    input_lines: u16,
    help_height: u16,
    pane_visible: bool,
    pane_right_visible: bool,
) -> AppLayout {
    let input_height = input_lines.max(1);

    // Horizontal split first so each visible side pane spans the
    // full terminal height, and the chat column (body + input) is
    // confined to the middle. Tier ladder:
    //   Wide   (>= 160) → 26ch pane(s) flanking the chat column
    //   Medium (>= 120) → 20ch pane(s) flanking the chat column
    //   Narrow (<  120) → no inline panes; top bar lands in chat
    //                     column below with icon affordances
    let (pane_rect, pane_separator_rect, pane_right_rect, pane_right_separator_rect, chat_area) =
        compute_horizontal_split(area, pane_visible, pane_right_visible);

    let mut layout = if chat_area.height < 8 {
        // Ultra-compact: drop the upper input separator (no room).
        let [body, input, input_bottom_sep, help] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(input_height),
            Constraint::Length(1),
            Constraint::Length(help_height),
        ])
        .areas(chat_area);
        AppLayout {
            top_bar: None,
            pane: pane_rect,
            pane_separator: pane_separator_rect,
            pane_right: pane_right_rect,
            pane_right_separator: pane_right_separator_rect,
            body,
            input_sep: Rect::new(chat_area.x, input.y, chat_area.width, 0),
            input,
            input_bottom_sep,
            help,
        }
    } else {
        let [body, input_sep, input, input_bottom_sep, help] = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(input_height),
            Constraint::Length(1),
            Constraint::Length(help_height),
        ])
        .areas(chat_area);
        AppLayout {
            top_bar: None,
            pane: pane_rect,
            pane_separator: pane_separator_rect,
            pane_right: pane_right_rect,
            pane_right_separator: pane_right_separator_rect,
            body,
            input_sep,
            input,
            input_bottom_sep,
            help,
        }
    };

    // At Narrow tier (<120), no inline panes: peel a single row off
    // the top of the body for the top bar. Only allocate when the
    // body has at least 2 rows so we keep at least one row of chat
    // behind the top bar.
    if area.width < MEDIUM_TIER_MIN_WIDTH && layout.body.height >= 2 {
        let [top, rest] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(layout.body);
        layout.top_bar = Some(top);
        layout.body = rest;
    }

    layout
}

/// Split the area horizontally into (optional left pane, optional
/// left separator, optional right pane, optional right separator,
/// chat column). At Narrow tier or when both panes are hidden, the
/// chat column takes the full area.
#[allow(clippy::type_complexity)]
fn compute_horizontal_split(
    area: Rect,
    pane_visible: bool,
    pane_right_visible: bool,
) -> (Option<Rect>, Option<Rect>, Option<Rect>, Option<Rect>, Rect) {
    if area.width < MEDIUM_TIER_MIN_WIDTH {
        // Narrow tier: no inline panes.
        return (None, None, None, None, area);
    }

    let pane_width =
        if area.width >= WIDE_TIER_MIN_WIDTH { PANE_WIDTH_WIDE } else { PANE_WIDTH_MEDIUM };

    // Build the horizontal constraint vector based on which panes
    // are visible.
    //
    // Left side (Projects pane): contributes
    //   pane_width + PANE_SEPARATOR_WIDTH + CHAT_PADDING
    // — a full `│` rule sits between the pane and the chat column.
    //
    // Right side (Inspector pane): contributes just `pane_width`.
    // No separator + no padding — the chat's own scrollbar rail
    // (`▕`, rendered at chat's right edge) acts as the visual
    // divider, and the Inspector pane's 2-col content indent
    // provides breathing room from it. This avoids the
    // "two-vertical-lines-side-by-side" effect the symmetric
    // pre-polish layout produced.
    let mut constraints: Vec<Constraint> = Vec::new();
    if pane_visible {
        constraints.push(Constraint::Length(pane_width));
        constraints.push(Constraint::Length(PANE_SEPARATOR_WIDTH));
        constraints.push(Constraint::Length(CHAT_PADDING));
    }
    constraints.push(Constraint::Min(1));
    if pane_right_visible {
        constraints.push(Constraint::Length(pane_width));
    }

    let rects = Layout::horizontal(constraints).split(area);
    let mut idx = 0;
    let pane_rect = if pane_visible {
        let r = rects[idx];
        idx += 3; // pane + separator + padding
        Some(r)
    } else {
        None
    };
    let pane_separator_rect = pane_rect.map(|_| rects[1]);
    let chat_area = rects[idx];
    idx += 1;
    let pane_right_rect = if pane_right_visible { Some(rects[idx]) } else { None };
    // No right-side separator — the chat scrollbar plays that role.
    let pane_right_separator_rect: Option<Rect> = None;
    (pane_rect, pane_separator_rect, pane_right_rect, pane_right_separator_rect, chat_area)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(w: u16, h: u16) -> Rect {
        Rect::new(0, 0, w, h)
    }

    /// Sum all layout area heights (handles optional top bar).
    fn total_height(layout: &AppLayout) -> u16 {
        layout.top_bar.map_or(0, |t| t.height)
            + layout.body.height
            + layout.input_sep.height
            + layout.input.height
            + layout.input_bottom_sep.height
            + layout.help.height
    }

    /// Collect all non-zero-height areas in top-to-bottom order.
    fn visible_areas(layout: &AppLayout) -> Vec<Rect> {
        let mut areas = Vec::new();
        if let Some(t) = layout.top_bar {
            areas.push(t);
        }
        areas.extend([
            layout.body,
            layout.input_sep,
            layout.input,
            layout.input_bottom_sep,
            layout.help,
        ]);
        areas.into_iter().filter(|r| r.height > 0).collect()
    }

    /// Assert no vertical overlap and areas are in ascending y-order.
    fn assert_no_overlap_and_ordered(layout: &AppLayout) {
        let areas = visible_areas(layout);
        for i in 1..areas.len() {
            let prev = areas[i - 1];
            let curr = areas[i];
            assert!(
                prev.y + prev.height <= curr.y,
                "Area {i}-1 ({prev:?}) overlaps or is not before area {i} ({curr:?})"
            );
        }
    }

    #[test]
    fn normal_layout_respects_requested_sections() {
        let layout = compute(area(80, 24), 5, 2, false, false);

        assert_eq!(layout.input_sep.height, 1);
        assert_eq!(layout.input.height, 5);
        assert_eq!(layout.input_bottom_sep.height, 1);
        assert_eq!(layout.help.height, 2);
        assert!(layout.body.height >= 3);
        assert_eq!(total_height(&layout), 24);
        // The chat column flush-fills the area now that the footer
        // is gone (it moved to the Projects pane).
        assert_eq!(layout.help.y + layout.help.height, 24);
    }

    #[test]
    fn compact_layout_omits_input_sep_and_allocates_remaining_space_to_input_and_help() {
        let layout = compute(area(80, 6), 3, 2, false, false);

        assert_eq!(layout.input_sep.height, 0);
        assert_eq!(layout.help.height, 2);
        assert!(layout.input.height >= 1);
        assert_eq!(total_height(&layout), 6);
    }

    #[test]
    fn layout_threshold_switches_at_height_eight() {
        let compact = compute(area(80, 7), 1, 0, false, false);
        let normal = compute(area(80, 8), 1, 1, false, false);

        // Compact path skips the input_sep allocation.
        assert_eq!(compact.input_sep.height, 0);
        // Normal path allocates an input_sep.
        assert_eq!(normal.input_sep.height, 1);
        assert_eq!(normal.help.height, 1);
    }

    #[test]
    fn layout_preserves_origin_and_width_in_both_modes() {
        // Use widths >= MEDIUM_TIER_MIN_WIDTH so the top bar is not
        // allocated; body sits at the area's `y`. Narrow-tier
        // top-bar offsets are covered by `narrow_tier_*` tests.
        let normal = compute(Rect::new(10, 5, 160, 24), 1, 0, false, false);
        let compact = compute(Rect::new(5, 10, 140, 6), 1, 0, false, false);

        for area in visible_areas(&normal) {
            assert_eq!(area.x, 10);
            assert_eq!(area.width, 160);
        }
        for area in visible_areas(&compact) {
            assert_eq!(area.x, 5);
            assert_eq!(area.width, 140);
        }
        assert_eq!(normal.body.y, 5);
        assert_eq!(compact.body.y, 10);
    }

    #[test]
    fn layout_clamps_input_and_preserves_total_height_for_degenerate_sizes() {
        let zero_height = compute(area(80, 0), 1, 0, false, false);
        let height_one = compute(area(80, 1), 1, 0, false, false);
        let width_one = compute(Rect::new(0, 0, 1, 24), 0, 0, false, false);
        let width_zero = compute(area(0, 24), 1, 0, false, false);

        assert_eq!(total_height(&zero_height), 0);
        assert_eq!(total_height(&height_one), 1);
        assert_eq!(width_one.input.height, 1);
        assert_eq!(width_one.body.width, 1);
        assert_eq!(width_zero.body.width, 0);
        assert_eq!(total_height(&width_zero), 24);
    }

    #[test]
    fn layout_squeezes_body_when_requested_sections_exceed_available_space() {
        let oversize_input = compute(area(80, 10), 50, 0, false, false);
        let competing = compute(area(80, 12), 3, 3, false, false);
        let large = compute(area(200, 100), 3, 2, false, false);

        assert_eq!(total_height(&oversize_input), 10);
        assert_eq!(total_height(&competing), 12);
        assert_eq!(total_height(&large), 100);
        assert!(large.body.height >= 3);
    }

    #[test]
    fn layout_areas_remain_ordered_in_normal_and_compact_modes() {
        // Width >= MEDIUM_TIER_MIN_WIDTH so the top bar isn't
        // allocated and the body still hugs the top of the area.
        let normal = compute(area(160, 30), 2, 1, false, false);
        let compact = compute(area(160, 6), 1, 1, false, false);

        assert_no_overlap_and_ordered(&normal);
        assert_no_overlap_and_ordered(&compact);
        assert_eq!(normal.body.y, 0);
    }

    #[test]
    fn parametric_layout_invariants_hold_across_sizes_and_feature_combinations() {
        for h in [0, 1, 2, 3, 5, 7, 8, 10, 15, 24, 50, 100] {
            for w in [0, 1, 10, 80, 200] {
                let layout = compute(Rect::new(0, 0, w, h), 1, 0, false, false);
                assert_eq!(total_height(&layout), h, "height mismatch for {w}x{h}");
                for area in visible_areas(&layout) {
                    assert_eq!(area.width, w, "width mismatch in area {area:?} for {w}x{h}");
                }
            }
        }

        for input in [0, 1, 3, 10] {
            for help in [0, 1, 3] {
                let layout = compute(area(80, 30), input, help, false, false);
                assert_eq!(
                    total_height(&layout),
                    30,
                    "height mismatch for input={input} help={help}"
                );
                assert_no_overlap_and_ordered(&layout);
            }
        }
    }

    #[test]
    fn pane_allocated_at_wide_tier_when_visible() {
        let layout = compute(area(180, 40), 1, 1, true, false);
        let pane = layout.pane.expect("pane should be allocated at width 180");
        assert_eq!(pane.width, PANE_WIDTH_WIDE);
        assert!(layout.body.width >= 1);
        assert_eq!(pane.x, 0, "pane sits on the left");
        // body sits past pane + separator + chat-padding.
        assert_eq!(layout.body.x, pane.x + pane.width + PANE_SEPARATOR_WIDTH + CHAT_PADDING);
    }

    #[test]
    fn pane_not_allocated_when_hidden() {
        let layout = compute(area(180, 40), 1, 1, false, false);
        assert!(layout.pane.is_none());
        assert_eq!(layout.body.x, 0);
    }

    #[test]
    fn pane_allocated_at_medium_tier() {
        let layout = compute(area(140, 40), 1, 1, true, false);
        let pane = layout.pane.expect("pane should be allocated at width 140");
        assert_eq!(pane.width, PANE_WIDTH_MEDIUM);
        assert!(layout.body.width >= 1);
        assert_eq!(pane.x, 0);
        assert_eq!(layout.body.x, pane.x + pane.width + PANE_SEPARATOR_WIDTH + CHAT_PADDING);
    }

    #[test]
    fn pane_not_allocated_below_medium_tier() {
        let layout = compute(area(100, 40), 1, 1, true, true);
        assert!(layout.pane.is_none(), "Narrow tier replaces inline panes with top-bar icons");
        assert!(
            layout.pane_right.is_none(),
            "Narrow tier replaces inline panes with top-bar icons"
        );
    }

    #[test]
    fn pane_widths_match_tier() {
        let wide = compute(area(180, 40), 1, 1, true, false);
        let medium = compute(area(140, 40), 1, 1, true, false);
        assert_eq!(wide.pane.unwrap().width, PANE_WIDTH_WIDE);
        assert_eq!(medium.pane.unwrap().width, PANE_WIDTH_MEDIUM);
    }

    #[test]
    fn narrow_tier_allocates_top_bar() {
        let layout = compute(area(100, 40), 1, 1, true, true);
        let top = layout.top_bar.expect("top_bar should be allocated at Narrow");
        assert_eq!(top.height, 1, "top bar is exactly one row tall");
        assert_eq!(top.y, 0, "top bar sits at the top");
        assert_eq!(top.x, 0, "top bar spans the full width from the left");
        assert_eq!(top.width, 100, "top bar spans the full terminal width");
        assert!(layout.pane.is_none(), "no inline pane at Narrow");
        assert!(layout.pane_right.is_none(), "no inline pane at Narrow");
        // Body has shrunk by one row to make room for the top bar.
        assert_eq!(layout.body.y, 1, "body starts on the row immediately below the top bar");
    }

    #[test]
    fn wide_and_medium_have_no_top_bar() {
        let wide = compute(area(180, 40), 1, 1, true, true);
        let medium = compute(area(140, 40), 1, 1, true, true);
        assert!(wide.top_bar.is_none(), "Wide tier has no top bar");
        assert!(medium.top_bar.is_none(), "Medium tier has no top bar");
    }

    #[test]
    fn narrow_tier_top_bar_independent_of_pane_visible() {
        // Top bar is the Narrow tier's permanent stand-in for the
        // inline panes — pane_visible=false (the user toggled the
        // pane off at Wide/Medium and is now resized to Narrow) must
        // still produce a top bar.
        let layout = compute(area(100, 40), 1, 1, false, false);
        assert!(layout.top_bar.is_some(), "Narrow always renders the top bar");
        assert!(layout.pane.is_none());
        assert!(layout.pane_right.is_none());
    }

    #[test]
    fn narrow_tier_top_bar_allocated_when_body_has_slack() {
        let layout = compute(area(100, 8), 1, 0, true, true);
        assert!(layout.top_bar.is_some(), "top bar still allocated when body has slack");
    }

    #[test]
    fn narrow_tier_skips_top_bar_when_body_truly_too_short() {
        // Compact-mode area (height < 8) with input+separator+help
        // chewing 1+1+1=3 of the 4 rows leaves body.height=1. The
        // skip guard refuses to peel another row off, so `top_bar`
        // stays `None`.
        let layout = compute(area(100, 4), 1, 1, true, true);
        assert!(layout.body.height < 2, "fixture must produce a 1-row body");
        assert!(
            layout.top_bar.is_none(),
            "top bar must skip when peeling would leave body with 0 rows"
        );
    }

    #[test]
    fn tier_boundaries_inclusive_at_min_widths() {
        // Wide tier kicks in inclusive at WIDE_TIER_MIN_WIDTH (160);
        // exactly one column shy collapses to Medium's 20-col pane.
        assert_eq!(
            compute(area(WIDE_TIER_MIN_WIDTH, 40), 1, 1, true, false).pane.unwrap().width,
            PANE_WIDTH_WIDE,
        );
        assert_eq!(
            compute(area(WIDE_TIER_MIN_WIDTH - 1, 40), 1, 1, true, false).pane.unwrap().width,
            PANE_WIDTH_MEDIUM,
        );
        // Medium tier kicks in inclusive at MEDIUM_TIER_MIN_WIDTH (120);
        // exactly one column shy is Narrow — no inline pane, top bar
        // takes over.
        assert_eq!(
            compute(area(MEDIUM_TIER_MIN_WIDTH, 40), 1, 1, true, false).pane.unwrap().width,
            PANE_WIDTH_MEDIUM,
        );
        assert!(compute(area(MEDIUM_TIER_MIN_WIDTH - 1, 40), 1, 1, true, false).pane.is_none());
        assert!(
            compute(area(MEDIUM_TIER_MIN_WIDTH - 1, 40), 1, 1, true, false).top_bar.is_some(),
            "Narrow tier replaces the inline pane with a top bar"
        );
        assert!(
            compute(area(MEDIUM_TIER_MIN_WIDTH, 40), 1, 1, true, false).top_bar.is_none(),
            "Medium tier hosts the inline pane, no top bar"
        );
        assert!(
            compute(area(WIDE_TIER_MIN_WIDTH, 40), 1, 1, true, false).top_bar.is_none(),
            "Wide tier hosts the inline pane, no top bar"
        );
    }

    #[test]
    fn right_pane_allocated_at_wide_tier_when_visible() {
        let layout = compute(area(180, 40), 1, 1, false, true);
        let right = layout.pane_right.expect("right pane should be allocated at width 180");
        assert_eq!(right.width, PANE_WIDTH_WIDE);
        // Right pane sits at the right edge.
        assert_eq!(right.x + right.width, 180);
        // Body sits immediately to the left of the right pane — no
        // separator column between them (the chat scrollbar fills
        // that role).
        assert_eq!(layout.body.x + layout.body.width, right.x);
        assert!(layout.pane_right_separator.is_none());
    }

    #[test]
    fn both_panes_allocated_at_wide_tier_when_both_visible() {
        let layout = compute(area(180, 40), 1, 1, true, true);
        let left = layout.pane.expect("left pane allocated");
        let right = layout.pane_right.expect("right pane allocated");
        assert_eq!(left.width, PANE_WIDTH_WIDE);
        assert_eq!(right.width, PANE_WIDTH_WIDE);
        // Left flush against x=0; right flush against the right edge.
        assert_eq!(left.x, 0);
        assert_eq!(right.x + right.width, 180);
        // Body sits strictly between the two panes — flush against the
        // right pane's left edge (no separator on the right side; the
        // chat scrollbar serves that role).
        assert!(layout.body.x >= left.x + left.width);
        assert_eq!(layout.body.x + layout.body.width, right.x);
        // Right side has no separator + no extra padding.
        assert!(layout.pane_right_separator.is_none());
    }

    #[test]
    fn right_pane_width_at_medium_tier() {
        let layout = compute(area(140, 40), 1, 1, false, true);
        let right = layout.pane_right.expect("right pane allocated at medium");
        assert_eq!(right.width, PANE_WIDTH_MEDIUM);
        assert!(layout.pane_right_separator.is_none());
    }

    #[test]
    fn right_pane_not_allocated_below_medium_tier() {
        let layout = compute(area(100, 40), 1, 1, false, true);
        assert!(layout.pane_right.is_none(), "no inline right pane at Narrow");
    }
}
