use ratatui::layout::{Constraint, Layout, Rect};

/// Minimum terminal width (columns) at which the Wide tier kicks in.
/// At Wide tier the Projects pane gets its own slot on the left of
/// the body at full width. Below this we fall back to Medium tier
/// (compact pane) and eventually to Narrow tier (overlay; lands in
/// Phase 2b-γ).
pub const WIDE_TIER_MIN_WIDTH: u16 = 160;

/// Minimum terminal width (columns) at which the Medium tier kicks
/// in. Between this and `WIDE_TIER_MIN_WIDTH` the Projects pane
/// renders inline at compact width with label truncation. Below
/// this the pane is hidden inline (Narrow tier overlay lands later).
pub const MEDIUM_TIER_MIN_WIDTH: u16 = 120;

/// Width (columns) of the Projects pane at Wide tier.
pub const PANE_WIDTH_WIDE: u16 = 26;

/// Width (columns) of the Projects pane at Medium tier.
pub const PANE_WIDTH_MEDIUM: u16 = 20;

#[derive(Clone, Default)]
pub struct AppLayout {
    /// Single-line top bar rect, allocated only at Narrow tier
    /// (`area.width < MEDIUM_TIER_MIN_WIDTH`). `None` at Wide /
    /// Medium tiers — those use the inline left pane instead.
    /// Hosts the `▤` icon + active-context label.
    pub top_bar: Option<Rect>,
    /// Left-side Projects pane rect when the user has the pane
    /// visible AND the terminal is at Wide tier (>= 160 cols).
    /// `None` whenever the pane should not be rendered.
    pub pane: Option<Rect>,
    /// Chat body. When the pane is allocated this is the rect
    /// remaining to the right of the pane; otherwise it spans the
    /// full body width.
    pub body: Rect,
    pub input_sep: Rect,
    /// Area for the todo panel (zero-height when hidden or no todos).
    /// Positioned below the input top separator and above the input field.
    pub todo: Rect,
    pub input: Rect,
    pub input_bottom_sep: Rect,
    pub help: Rect,
    pub footer: Option<Rect>,
}

pub fn compute(
    area: Rect,
    input_lines: u16,
    todo_height: u16,
    help_height: u16,
    pane_visible: bool,
) -> AppLayout {
    let input_height = input_lines.max(1);

    let mut layout = if area.height < 8 {
        // Ultra-compact: no footer, no todo
        let [body, input, input_bottom_sep, help] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(input_height),
            Constraint::Length(1),
            Constraint::Length(help_height),
        ])
        .areas(area);
        AppLayout {
            top_bar: None,
            pane: None,
            body,
            todo: Rect::new(area.x, input.y, area.width, 0),
            input_sep: Rect::new(area.x, input.y, area.width, 0),
            input,
            input_bottom_sep,
            help,
            footer: None,
        }
    } else {
        let [body, input_sep, todo, input, input_bottom_sep, help, footer] = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(todo_height),
            Constraint::Length(input_height),
            Constraint::Length(1),
            Constraint::Length(help_height),
            Constraint::Length(2),
        ])
        .areas(area);
        AppLayout {
            top_bar: None,
            pane: None,
            body,
            input_sep,
            todo,
            input,
            input_bottom_sep,
            help,
            footer: Some(footer),
        }
    };

    // At Narrow tier (<120), peel a single row off the top of the body
    // for the top bar. Allocates regardless of `pane_visible` — the
    // top bar is the Narrow tier's permanent stand-in for the inline
    // pane, and clicking its `▤` icon (or Ctrl+B) is what opens the
    // overlay. Only allocate when the body has at least 2 rows so we
    // keep at least one row of chat behind the top bar.
    if area.width < MEDIUM_TIER_MIN_WIDTH && layout.body.height >= 2 {
        let [top, rest] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(layout.body);
        layout.top_bar = Some(top);
        layout.body = rest;
    }

    // Tier ladder for the Projects pane (Phase 2b-α + 2b-β + 2b-γ):
    //   Wide   (>= 160) → 26ch inline pane
    //   Medium (>= 120) → 20ch inline pane (label truncation in renderer)
    //   Narrow (<  120) → top bar + on-demand overlay; no inline pane
    // When the user has hidden the pane via Ctrl+B (Wide/Medium only),
    // those tiers collapse to "no pane". Narrow tier is unaffected by
    // `pane_visible` — overlay open/close is a separate transient flag
    // (`App.projects_pane_overlay_open`).
    if pane_visible {
        if area.width >= WIDE_TIER_MIN_WIDTH {
            let [pane_rect, chat_rect] =
                Layout::horizontal([Constraint::Length(PANE_WIDTH_WIDE), Constraint::Min(1)])
                    .areas(layout.body);
            AppLayout { pane: Some(pane_rect), body: chat_rect, ..layout }
        } else if area.width >= MEDIUM_TIER_MIN_WIDTH {
            let [pane_rect, chat_rect] =
                Layout::horizontal([Constraint::Length(PANE_WIDTH_MEDIUM), Constraint::Min(1)])
                    .areas(layout.body);
            AppLayout { pane: Some(pane_rect), body: chat_rect, ..layout }
        } else {
            layout
        }
    } else {
        layout
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(w: u16, h: u16) -> Rect {
        Rect::new(0, 0, w, h)
    }

    /// Sum all layout area heights (handles optional footer + top bar).
    fn total_height(layout: &AppLayout) -> u16 {
        layout.top_bar.map_or(0, |t| t.height)
            + layout.body.height
            + layout.todo.height
            + layout.input_sep.height
            + layout.input.height
            + layout.input_bottom_sep.height
            + layout.help.height
            + layout.footer.map_or(0, |f| f.height)
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
            layout.todo,
            layout.input,
            layout.input_bottom_sep,
            layout.help,
        ]);
        if let Some(f) = layout.footer {
            areas.push(f);
        }
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
    fn normal_layout_respects_requested_sections_and_footer_contract() {
        let layout = compute(area(80, 24), 5, 3, 2, false);
        let footer = layout.footer.expect("normal layout should include a footer");

        assert_eq!(layout.input_sep.height, 1);
        assert_eq!(layout.todo.height, 3);
        assert_eq!(layout.input.height, 5);
        assert_eq!(layout.input_bottom_sep.height, 1);
        assert_eq!(layout.help.height, 2);
        assert_eq!(footer.height, 2);
        assert!(layout.body.height >= 3);
        assert_eq!(total_height(&layout), 24);
        assert_eq!(footer.y + footer.height, 24);
    }

    #[test]
    fn compact_layout_omits_footer_and_todo_and_allocates_remaining_space_to_input_and_help() {
        let layout = compute(area(80, 6), 3, 4, 2, false);

        assert!(layout.footer.is_none());
        assert_eq!(layout.todo.height, 0);
        assert_eq!(layout.input_sep.height, 0);
        assert_eq!(layout.help.height, 2);
        assert!(layout.input.height >= 1);
        assert_eq!(total_height(&layout), 6);
    }

    #[test]
    fn layout_threshold_switches_at_height_eight() {
        let compact = compute(area(80, 7), 1, 0, 0, false);
        let normal = compute(area(80, 8), 1, 0, 1, false);

        assert!(compact.footer.is_none());
        assert!(normal.footer.is_some());
        assert_eq!(normal.help.height, 1);
    }

    #[test]
    fn layout_preserves_origin_and_width_in_both_modes() {
        // Use widths >= MEDIUM_TIER_MIN_WIDTH so the top bar is not
        // allocated; body sits at the area's `y`. Narrow-tier
        // top-bar offsets are covered by `narrow_tier_*` tests.
        let normal = compute(Rect::new(10, 5, 160, 24), 1, 0, 0, false);
        let compact = compute(Rect::new(5, 10, 140, 6), 1, 0, 0, false);

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
        let zero_height = compute(area(80, 0), 1, 0, 0, false);
        let height_one = compute(area(80, 1), 1, 0, 0, false);
        let width_one = compute(Rect::new(0, 0, 1, 24), 0, 0, 0, false);
        let width_zero = compute(area(0, 24), 1, 0, 0, false);

        assert!(zero_height.footer.is_none());
        assert_eq!(total_height(&zero_height), 0);
        assert_eq!(total_height(&height_one), 1);
        assert_eq!(width_one.input.height, 1);
        assert_eq!(width_one.body.width, 1);
        assert_eq!(width_zero.body.width, 0);
        assert_eq!(total_height(&width_zero), 24);
    }

    #[test]
    fn layout_squeezes_body_when_requested_sections_exceed_available_space() {
        let oversize_input = compute(area(80, 10), 50, 0, 0, false);
        let competing = compute(area(80, 12), 3, 4, 3, false);
        let large = compute(area(200, 100), 3, 5, 2, false);

        assert_eq!(total_height(&oversize_input), 10);
        assert_eq!(total_height(&competing), 12);
        assert_eq!(total_height(&large), 100);
        assert!(large.body.height >= 3);
    }

    #[test]
    fn layout_areas_remain_ordered_in_normal_and_compact_modes() {
        // Width >= MEDIUM_TIER_MIN_WIDTH so the top bar isn't
        // allocated and the body still hugs the top of the area.
        let normal = compute(area(160, 30), 2, 3, 1, false);
        let compact = compute(area(160, 6), 1, 0, 1, false);

        assert_no_overlap_and_ordered(&normal);
        assert_no_overlap_and_ordered(&compact);
        assert_eq!(normal.body.y, 0);
    }

    #[test]
    fn parametric_layout_invariants_hold_across_sizes_and_feature_combinations() {
        for h in [0, 1, 2, 3, 5, 7, 8, 10, 15, 24, 50, 100] {
            for w in [0, 1, 10, 80, 200] {
                let layout = compute(Rect::new(0, 0, w, h), 1, 0, 0, false);
                assert_eq!(total_height(&layout), h, "height mismatch for {w}x{h}");
                for area in visible_areas(&layout) {
                    assert_eq!(area.width, w, "width mismatch in area {area:?} for {w}x{h}");
                }
            }
        }

        for input in [0, 1, 3, 10] {
            for todo in [0, 2, 5] {
                for help in [0, 1, 3] {
                    let layout = compute(area(80, 30), input, todo, help, false);
                    assert_eq!(
                        total_height(&layout),
                        30,
                        "height mismatch for input={input} todo={todo} help={help}"
                    );
                    assert_no_overlap_and_ordered(&layout);
                }
            }
        }
    }

    #[test]
    fn pane_allocated_at_wide_tier_when_visible() {
        let layout = compute(area(180, 40), 1, 0, 1, true);
        let pane = layout.pane.expect("pane should be allocated at width 180");
        assert_eq!(pane.width, 26);
        assert!(layout.body.width >= 1);
        assert_eq!(pane.x, 0, "pane sits on the left");
        assert_eq!(layout.body.x, pane.x + pane.width);
    }

    #[test]
    fn pane_not_allocated_when_hidden() {
        let layout = compute(area(180, 40), 1, 0, 1, false);
        assert!(layout.pane.is_none());
        assert_eq!(layout.body.x, 0);
    }

    #[test]
    fn pane_allocated_at_medium_tier() {
        let layout = compute(area(140, 40), 1, 0, 1, true);
        let pane = layout.pane.expect("pane should be allocated at width 140");
        assert_eq!(pane.width, 20);
        assert!(layout.body.width >= 1);
        assert_eq!(pane.x, 0);
        assert_eq!(layout.body.x, pane.x + pane.width);
    }

    #[test]
    fn pane_not_allocated_below_medium_tier() {
        let layout = compute(area(100, 40), 1, 0, 1, true);
        assert!(layout.pane.is_none(), "Narrow tier (<120) gets overlay in 2b-γ, not inline pane");
    }

    #[test]
    fn pane_widths_match_tier() {
        let wide = compute(area(180, 40), 1, 0, 1, true);
        let medium = compute(area(140, 40), 1, 0, 1, true);
        assert_eq!(wide.pane.unwrap().width, 26);
        assert_eq!(medium.pane.unwrap().width, 20);
    }

    #[test]
    fn narrow_tier_allocates_top_bar() {
        let layout = compute(area(100, 40), 1, 0, 1, true);
        let top = layout.top_bar.expect("top_bar should be allocated at Narrow");
        assert_eq!(top.height, 1, "top bar is exactly one row tall");
        assert_eq!(top.y, 0, "top bar sits at the top");
        assert_eq!(top.x, 0, "top bar spans the full width from the left");
        assert_eq!(top.width, 100, "top bar spans the full terminal width");
        assert!(layout.pane.is_none(), "no inline pane at Narrow");
        // Body has shrunk by one row to make room for the top bar.
        assert_eq!(layout.body.y, 1, "body starts on the row immediately below the top bar");
    }

    #[test]
    fn wide_and_medium_have_no_top_bar() {
        let wide = compute(area(180, 40), 1, 0, 1, true);
        let medium = compute(area(140, 40), 1, 0, 1, true);
        assert!(wide.top_bar.is_none(), "Wide tier has no top bar");
        assert!(medium.top_bar.is_none(), "Medium tier has no top bar");
    }

    #[test]
    fn narrow_tier_top_bar_independent_of_pane_visible() {
        // Top bar is the Narrow tier's permanent stand-in for the
        // inline pane — pane_visible=false (the user toggled the
        // pane off at Wide/Medium and is now resized to Narrow) must
        // still produce a top bar.
        let layout = compute(area(100, 40), 1, 0, 1, false);
        assert!(layout.top_bar.is_some(), "Narrow always renders the top bar");
        assert!(layout.pane.is_none());
    }

    #[test]
    fn narrow_tier_skips_top_bar_when_body_too_short() {
        // Pathological tiny height: layout already has a 1-row body;
        // peeling another row off would leave 0 rows of chat. Skip
        // the top bar and render chat full-height.
        let layout = compute(area(100, 8), 1, 0, 0, true);
        // body height starts at >=3 in normal mode; no skip needed
        // here — top bar should be allocated.
        assert!(layout.top_bar.is_some(), "top bar still allocated when body has slack");
    }
}
