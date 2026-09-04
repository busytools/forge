//! Layout math and pure hunk transforms shared by the overlay state,
//! the key/mouse handlers, and the renderer at
//! [`crate::ui::diff_overlay`]: tree ordering, hunk re-narrowing,
//! height estimates, rail width, and split-row geometry.

use ratatui::text::Span;

use super::types::DiffViewMode;
use forge_workspace::env::git_diff::hunks::{DiffLine, DiffLineKind, FileHunks, Hunk};

/// Reorder a scanner-ordered file list into the FILES rail's folded-tree
/// traversal order - the one canonical display sequence the body, the
/// offset table, and the rail all walk, so the current-file arrow steps
/// monotonically down the rail as the body scrolls. Sorting the paths by
/// [`compare_tree_paths`] yields the rail's pre-order leaf sequence
/// because the rail re-sorts by the same per-level rule, so its tree is
/// a pure function of the path set (independent of input order).
pub(super) fn reorder_files_to_tree(files: &mut [FileHunks]) {
    files.sort_by(|a, b| compare_tree_paths(&a.path, &b.path));
}

/// Order two diff paths by the rail's per-level rule, as a genuine total
/// order: at each segment, a directory (a segment that isn't this path's
/// leaf) sorts before a file, then alphabetically. Comparing the
/// `(is_leaf, name)` tuple per segment keeps it transitive even when a
/// name is both a file and a directory (a file->dir refactor git emits as
/// `D z` + `A z/a`), which a length-only fallback would make cyclic.
fn compare_tree_paths(a: &str, b: &str) -> std::cmp::Ordering {
    let a: Vec<&str> = a.split('/').filter(|c| !c.is_empty()).collect();
    let b: Vec<&str> = b.split('/').filter(|c| !c.is_empty()).collect();
    for i in 0..a.len().min(b.len()) {
        let ord = (i + 1 == a.len(), a[i]).cmp(&(i + 1 == b.len(), b[i]));
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    a.len().cmp(&b.len())
}

/// Narrow a file's full-context wide hunks down to `context` lines
/// around each change, reproducing git's `-U<context>` hunking in memory:
/// a line is kept when it is a change or within `context` of one, and
/// maximal runs of kept lines become hunks (so two changes whose gap the
/// context spans fold into one). The overlay captures the wide hunks once
/// at open and re-narrows from them on an expander click - no re-fetch.
pub(super) fn narrow_hunks(wide: &[Hunk], context: usize) -> Vec<Hunk> {
    let mut out = Vec::new();
    for hunk in wide {
        let n = hunk.lines.len();
        let mut keep = vec![false; n];
        let mut has_change = false;
        for (i, line) in hunk.lines.iter().enumerate() {
            if line.kind != DiffLineKind::Context {
                has_change = true;
                let lo = i.saturating_sub(context);
                let hi = (i + context).min(n.saturating_sub(1));
                for slot in keep.iter_mut().take(hi + 1).skip(lo) {
                    *slot = true;
                }
            }
        }
        // A real diff hunk always carries a change; a changeless one (only
        // test fixtures build these) has nothing to narrow around, so keep
        // it whole rather than dropping it to an empty display.
        if !has_change {
            keep.fill(true);
        }
        let mut i = 0;
        while i < n {
            if !keep[i] {
                i += 1;
                continue;
            }
            let start = i;
            while i < n && keep[i] {
                i += 1;
            }
            out.push(hunk_from_lines(&hunk.lines[start..i]));
        }
    }
    out
}

/// Build a `Hunk` header from a contiguous run of diff lines: the start
/// line numbers are the first old-/new-side line present, the counts the
/// number of old-/new-side lines. Used by [`narrow_hunks`] to re-header a
/// slice of the wide snapshot.
fn hunk_from_lines(lines: &[DiffLine]) -> Hunk {
    let old_start = lines.iter().find_map(|l| l.old_line).unwrap_or(0);
    let new_start = lines.iter().find_map(|l| l.new_line).unwrap_or(0);
    let old_count =
        u32::try_from(lines.iter().filter(|l| l.old_line.is_some()).count()).unwrap_or(u32::MAX);
    let new_count =
        u32::try_from(lines.iter().filter(|l| l.new_line.is_some()).count()).unwrap_or(u32::MAX);
    Hunk { old_start, old_count, new_start, new_count, lines: lines.to_vec() }
}

/// Unwrapped, syntax-highlighted spans for one file's diff lines,
/// indexed `[hunk_idx][line_idx]` (the innermost `Vec` is one line's
/// spans). Cached on
/// [`DiffOverlayState::highlighted`](crate::app::diff_overlay::DiffOverlayState::highlighted)
/// and reused
/// across frames - a line's colour is layout-independent, so it
/// survives scroll, view_mode flip, and resize untouched, and a
/// plain scroll never re-runs syntect.
pub type FileHighlight = Vec<Vec<Vec<Span<'static>>>>;

/// Rendered rows the end-of-file boundary adds after every file but the
/// last: the `└─ end <path> ──` cap row plus a blank spacer before the
/// next file's banded header. Measured heights (via the renderer's
/// `push_file_body`) already include it, so the off-screen estimate must
/// add it too or the offset table drifts by two rows per file boundary.
pub(crate) const END_CAP_ROWS: u32 = 2;

/// Unified-diff context radius the initial scan runs at (`git diff`'s
/// default `-U3`). The per-file expansion level starts here and grows by
/// [`CONTEXT_STEP`] per expander click.
pub(crate) const DEFAULT_CONTEXT: u32 = 3;

/// Lines of context an expander click adds per side (so a gap shrinks by
/// up to `2 * CONTEXT_STEP` per click, GitHub's ~20-lines-per-click feel).
pub(crate) const CONTEXT_STEP: u32 = 20;

/// Cheap height estimate for an off-screen file (no wrap, no pairing):
/// 1 sticky header row + per hunk (1 `@@` header + raw line count) + the
/// context-expander rows the renderer emits (one above the first hunk
/// when it starts past line 1, one per non-zero inter-hunk gap), or 2 for
/// a collapsed deleted file. The offset table uses this for not-yet-
/// measured files; the renderer's `file_height` replaces it (storing into
/// `measured_heights`) when the file enters the window.
pub(crate) fn estimated_height(file: &FileHunks, collapsed: bool) -> u32 {
    if collapsed {
        return 2;
    }
    // A sticky header, plus (oversize files only) the one-line
    // "too large" note the renderer shows instead of expanders.
    let mut rows: u32 = if file.oversize { 2 } else { 1 };
    let mut prev_end: Option<u32> = None;
    for hunk in &file.hunks {
        // Expander row where lines are hidden before this hunk - above the
        // first hunk (leading) or across a gap - but oversize files render
        // no expanders (their snapshot can't reveal more).
        if !file.oversize {
            let hidden = match prev_end {
                None => hunk.new_start.saturating_sub(1),
                Some(end) => hunk.new_start.saturating_sub(end),
            };
            if hidden > 0 {
                rows = rows.saturating_add(1);
            }
        }
        rows = rows.saturating_add(1); // @@ header
        rows = rows.saturating_add(u32::try_from(hunk.lines.len()).unwrap_or(u32::MAX));
        prev_end = Some(hunk.new_start.saturating_add(hunk.new_count));
    }
    rows
}

/// Lines scrolled per wheel notch in the diff body. Same value as
/// `crate::app::events::mouse::MOUSE_SCROLL_LINES`; applied to the
/// `u32` document scroll (`doc_scroll`).
pub(super) const SCROLL_LINES_PER_NOTCH: u16 = 3;

/// Minimum FILES rail width when the rail is shown. Below this the
/// file list becomes unreadably narrow; we hide the rail entirely.
pub(crate) const RAIL_WIDTH_MIN: u16 = 20;
/// Rail width as a fraction of the terminal width: strict 15%. The
/// remaining 85% goes to the two-pane diff body, split evenly with
/// any odd leftover column handed to the right half so a `+`
/// addition column never reads as narrower than its `-` counterpart.
pub(crate) const RAIL_WIDTH_NUMER: u16 = 15;
pub(crate) const RAIL_WIDTH_DENOM: u16 = 100;
/// Medium-tier terminal width threshold (≥ this → rail visible).
pub(crate) const MEDIUM_MIN: u16 = 120;

/// First file row in the FILES rail. Rows above this are:
/// `0` banner (`FILES`), `1` DIM rule, `2` blank. File index 0
/// starts at `y == FIRST_FILE_ROW_Y`. The renderer at
/// `ui::diff_overlay::render_rail` chose this geometry; the click
/// handler uses it for the inverse mapping.
pub(crate) const FIRST_FILE_ROW_Y: u16 = 3;

/// Pick the FILES rail width for the current terminal width.
/// Returns `0` at Narrow tier (rail hidden). Shared with the
/// renderer at `crate::ui::diff_overlay::render` so the rail's
/// width and the click-handler's column threshold never drift.
pub(crate) fn rail_width_for(terminal_width: u16) -> u16 {
    if terminal_width < MEDIUM_MIN {
        return 0;
    }
    let proportional = terminal_width.saturating_mul(RAIL_WIDTH_NUMER) / RAIL_WIDTH_DENOM;
    proportional.max(RAIL_WIDTH_MIN)
}

/// Narrowest gutter, so single-digit line numbers don't shift the
/// marker column relative to two-digit ones in the same hunk.
pub(super) const SPLIT_GUTTER_MIN: usize = 2;
/// Widest gutter. Beyond this the gutter is under-reserved and the
/// row's divider shifts right of where `split_layout` puts it.
pub(super) const SPLIT_GUTTER_MAX: usize = 6;

/// Gutter width for a file's split / unified line numbers.
pub(crate) fn gutter_width_for(file: &FileHunks) -> usize {
    let max_line = file
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .filter_map(|l| l.new_line.or(l.old_line))
        .max()
        .unwrap_or(1);
    max_line.to_string().len().clamp(SPLIT_GUTTER_MIN, SPLIT_GUTTER_MAX)
}

/// Minimum body width (in columns) for the side-by-side split view -
/// two columns of readable code need the room. Unified renders at any
/// width (it soft-wraps), so below this the split toggle silently
/// falls back to unified rather than blocking the overlay.
pub(crate) const MIN_WIDTH_FOR_SPLIT: u16 = 100;

/// The layout actually painted: the stored choice, except a body
/// narrower than [`MIN_WIDTH_FOR_SPLIT`] forces unified. The stored
/// `view_mode` is untouched, so widening the pane restores split.
/// The hit-test reads this too, so a click resolves against the
/// layout on screen rather than the one the user last asked for.
pub(crate) fn effective_view_mode(stored: DiffViewMode, pane_width: u16) -> DiffViewMode {
    if pane_width < MIN_WIDTH_FOR_SPLIT { DiffViewMode::Unified } else { stored }
}

/// Leading indent before the left column of a split row.
pub(super) const SPLIT_INDENT_COLS: usize = 2;
/// Space, `│`, space between the two columns.
pub(super) const SPLIT_DIVIDER_COLS: usize = 3;
/// Space, marker, space between a column's gutter and its text.
pub(crate) const SPLIT_MARKER_COLS: usize = 3;

/// Column geometry of one split-view row.
pub(crate) struct SplitLayout {
    pub left_width: usize,
    pub right_width: usize,
    /// Pane-local column the `│` is painted in.
    pub divider_col: usize,
}

/// Split-row geometry, shared by the renderer and the click handler.
///
/// Each half's gutter and marker are reserved before the text columns
/// split what is left, so a row fits the pane and the divider sits on
/// the midpoint at even pane widths, one column right of it at odd
/// ones. Widening the gutter narrows both columns rather than moving
/// the divider.
pub(crate) fn split_layout(gutter_width: usize, pane_width: u16) -> SplitLayout {
    // Both halves carry a gutter and a marker zone of their own, so
    // those come out of the budget before the text columns split what
    // is left.
    let per_half_chrome = gutter_width.saturating_add(SPLIT_MARKER_COLS);
    let usable = usize::from(pane_width)
        .saturating_sub(SPLIT_INDENT_COLS)
        .saturating_sub(SPLIT_DIVIDER_COLS)
        .saturating_sub(per_half_chrome.saturating_mul(2));
    let left_width = usable / 2;
    SplitLayout {
        left_width,
        right_width: usable - left_width,
        divider_col: SPLIT_INDENT_COLS + gutter_width + SPLIT_MARKER_COLS + left_width + 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::diff_overlay::test_support::*;
    use forge_workspace::env::git_diff::hunks::FileStatus;

    #[test]
    fn effective_view_mode_forces_unified_below_split_threshold() {
        assert_eq!(effective_view_mode(DiffViewMode::Split, 80), DiffViewMode::Unified);
        assert_eq!(
            effective_view_mode(DiffViewMode::Split, MIN_WIDTH_FOR_SPLIT),
            DiffViewMode::Split,
        );
        assert_eq!(effective_view_mode(DiffViewMode::Unified, 200), DiffViewMode::Unified);
    }

    /// Each half's gutter comes out of its own text budget, so widening
    /// the gutter narrows both columns and leaves the divider where it
    /// was. A hit-test that assumed otherwise would drift per file.
    #[test]
    fn the_divider_does_not_move_with_gutter_width() {
        for pane_width in [101u16, 119, 160, 184] {
            let narrow = split_layout(SPLIT_GUTTER_MIN, pane_width);
            let wide = split_layout(SPLIT_GUTTER_MAX, pane_width);
            assert_eq!(narrow.divider_col, wide.divider_col, "pane_width={pane_width}");
            // Every gutter, not just the two a line-count fixture can reach.
            for gutter in SPLIT_GUTTER_MIN..=SPLIT_GUTTER_MAX {
                let layout = split_layout(gutter, pane_width);
                let row = SPLIT_INDENT_COLS
                    + (gutter + SPLIT_MARKER_COLS + layout.left_width)
                    + SPLIT_DIVIDER_COLS
                    + (gutter + SPLIT_MARKER_COLS + layout.right_width);
                assert_eq!(row, usize::from(pane_width), "gutter={gutter} pane={pane_width}");
            }
            // The wider gutter is paid for out of the text columns.
            assert!(
                wide.left_width < narrow.left_width,
                "pane_width={pane_width} {} {}",
                wide.left_width,
                narrow.left_width
            );
        }
    }

    #[test]
    fn rail_width_is_strict_15_percent_on_wide_terminals() {
        // 200 × 15 / 100 = 30; 300 × 15 / 100 = 45.
        assert_eq!(rail_width_for(200), 30);
        assert_eq!(rail_width_for(300), 45);
    }

    #[test]
    fn rail_width_floors_at_min_on_narrow_borderline_widths() {
        // 120 × 15 / 100 = 18 → below MIN (20), floored to 20.
        assert_eq!(rail_width_for(120), RAIL_WIDTH_MIN);
        // 140 × 15 / 100 = 21 → above MIN, kept.
        assert_eq!(rail_width_for(140), 21);
        // Anything below MEDIUM_MIN still hides the rail entirely
        // regardless of what the percentage math would produce.
        assert_eq!(rail_width_for(100), 0);
    }

    #[test]
    fn rail_width_hidden_below_medium_threshold() {
        assert_eq!(rail_width_for(119), 0);
        assert_eq!(rail_width_for(80), 0);
    }

    #[test]
    fn estimated_height_counts_expander_rows() {
        // Two hunks: the first starts at line 5 (4 hidden above → a
        // leading expander) with a 14-line gap to the second (a gap
        // expander). The estimate must fold both in.
        let file = FileHunks {
            path: "a.rs".to_owned(),
            status: FileStatus::Modified,
            oversize: false,
            hunks: vec![
                Hunk {
                    old_start: 5,
                    old_count: 1,
                    new_start: 5,
                    new_count: 1,
                    lines: vec![diff_line(DiffLineKind::Context, Some(5), Some(5))],
                },
                Hunk {
                    old_start: 20,
                    old_count: 1,
                    new_start: 20,
                    new_count: 1,
                    lines: vec![diff_line(DiffLineKind::Context, Some(20), Some(20))],
                },
            ],
        };
        // 1 header + leading expander + (@@ + line) + gap expander + (@@ + line).
        assert_eq!(estimated_height(&file, false), 7);

        // A single hunk flush at line 1 hides nothing → no expander row.
        let flush = FileHunks {
            path: "b.rs".to_owned(),
            status: FileStatus::Modified,
            oversize: false,
            hunks: vec![Hunk {
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 1,
                lines: vec![diff_line(DiffLineKind::Context, Some(1), Some(1))],
            }],
        };
        assert_eq!(estimated_height(&flush, false), 3, "no expander when nothing is hidden");
    }

    #[test]
    fn narrow_hunks_reproduces_git_hunking() {
        let wide = wide_file_with_two_changes().hunks;
        assert_eq!(narrow_hunks(&wide, 3).len(), 2, "default context keeps the gap as two hunks");
        let merged = narrow_hunks(&wide, 23);
        assert_eq!(merged.len(), 1, "wide-enough context folds the two changes into one hunk");
        assert_eq!(merged[0].lines.len(), 30, "the merged hunk carries the whole file");
    }
}
