//! Inspector pane (right side, Wide + Medium tiers; full-screen
//! overlay at Narrow tier).
//!
//! Mirror of the left [`crate::ui::projects_pane`] in chrome and
//! tier behaviour. Two sections separated by a DIM `─` rule:
//!
//! - `GIT` — always rendered. Shows the active session's cwd, the
//!   current branch with aggregate `+N -M` totals right-justified
//!   beside it (when there's a diff), and a box-drawing tree of
//!   the top-N changed files grouped by directory. Single-child
//!   directory chains fold so deep paths render as one row. Sourced
//!   from `UiSession.git_diff_snapshot`.
//! - `TASKS` — rendered when the active session has todos or a
//!   pending verification nudge. The live `TodoWrite` snapshot is
//!   the sole surface for the todo list; the chat-stream
//!   `TodoWrite` tool-call card is suppressed.
//!
//! Reads from per-session state on `UiSession.todos` (post PR #109)
//! and `UiSession.git_diff_snapshot` (this PR). The
//! `TodoWriteOutputMetadata.verification_nudge_needed` flag surfaces
//! as a dim-yellow notice above the `TASKS` header until the next
//! `TodoWrite` clears it.
//!
//! TASKS item rendering:
//! - `✓` green glyph + DIM crossed-out text for `Completed`
//! - `▸` RUST_ORANGE glyph + white bold text for `InProgress`
//!   (wraps onto continuation lines indented under the glyph;
//!   uses `active_form` when present, else `content`)
//! - `○` DIM glyph + gray text for `Pending` (truncates with `…`)

use forge_primitives::git::GitBranch;
use forge_workspace::env::git_diff::{GitDiffFile, GitDiffSnapshot, GitDiffView};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::theme;
use crate::app::App;
use crate::app::PaneHitTarget;
use crate::app::TodoStatus;

/// Horizontal padding inside the pane (matches the left
/// `projects_pane`'s 2-col indent).
const PANE_PAD: u16 = 2;

/// Minimum gap (cols) between the path column and the stats column
/// in a per-file diff row. Reserves visual breathing room so the
/// truncated path never butts up against the `+N -M` numbers even at
/// the worst-case width.
const PATH_STATS_GAP: usize = 2;

/// Render the Inspector pane into `area` (inline at Wide/Medium).
pub fn render(frame: &mut Frame, area: Rect, app: &mut App) {
    let lines = build_lines(app, area.width);
    frame.render_widget(Paragraph::new(lines), area);
}

/// Render the Narrow-tier full-screen Inspector overlay into `area`.
/// Shares the body builder with the inline path, wrapped in an
/// overlay-specific banner with an `INSPECTOR ▦` label on the left
/// and a `✕` glyph on the right (stamped as
/// [`PaneHitTarget::OverlayClose`] for the click handler).
pub fn render_overlay(frame: &mut Frame, area: Rect, app: &mut App) {
    app.pane_hit_targets.clear();

    let mut lines: Vec<Line<'static>> = Vec::new();

    // Banner row: `INSPECTOR ▦ … ✕` spanning the full overlay width.
    let banner_label = "INSPECTOR \u{25a6}";
    let close_glyph = "\u{2715}";
    let banner_chars = banner_label.chars().count();
    let close_chars = close_glyph.chars().count();
    let pad = usize::from(area.width).saturating_sub(banner_chars).saturating_sub(close_chars);
    lines.push(Line::from(vec![
        Span::styled(
            banner_label.to_owned(),
            Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" ".repeat(pad)),
        Span::styled(close_glyph.to_owned(), Style::default().fg(theme::DIM)),
    ]));
    // Stamp ✕ hit-target — last char on the banner row.
    let close_x_start =
        area.x.saturating_add(area.width).saturating_sub(u16::try_from(close_chars).unwrap_or(1));
    let close_x_end = area.x.saturating_add(area.width);
    app.pane_hit_targets.push(PaneHitTarget::OverlayClose {
        y: area.y,
        height: 1,
        x_start: close_x_start,
        x_end: close_x_end,
    });

    // Dim rule under the banner.
    let rule_width = usize::from(area.width);
    lines.push(Line::from(Span::styled(
        "\u{2500}".repeat(rule_width),
        Style::default().fg(theme::DIM),
    )));

    append_body(&mut lines, app, area.width);

    frame.render_widget(Paragraph::new(lines), area);
}

/// Build the full inline-pane line list: banner + rule + body.
fn build_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();

    // Banner: `INSPECTOR` in RUST_ORANGE bold (mirror of `PROJECTS`).
    lines.push(Line::from(Span::styled(
        "  INSPECTOR".to_owned(),
        Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD),
    )));
    // Dim rule under the banner.
    let rule_width = usize::from(width.saturating_sub(2));
    lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled("\u{2500}".repeat(rule_width), Style::default().fg(theme::DIM)),
    ]));

    append_body(&mut lines, app, width);

    lines
}

/// Append the body (GIT section + verification nudge + TASKS
/// section) to `lines`. Shared between the inline render and the
/// Narrow overlay render. GIT and TASKS are separated by a DIM
/// `─` rule mirroring the projects pane's project-list /
/// account-panel boundary, so the two surfaces read as visually
/// distinct rather than two `DIM bold` headers next to each other.
fn append_body(lines: &mut Vec<Line<'static>>, app: &App, width: u16) {
    append_git_section(lines, app, width);

    let todos = app.todos();
    let has_tasks = !todos.is_empty() || app.todo_verification_nudge();
    if has_tasks {
        lines.push(Line::default());
        push_section_rule(lines, width);
        lines.push(Line::default());
        append_tasks_section(lines, app, width);
    }
}

/// Append a dim `─` horizontal rule across `width − 2` cols with a
/// 1-col leading space, matching the banner-rule + projects-pane
/// section-separator shape so the inspector body reads consistently
/// with the rest of the chrome.
fn push_section_rule(lines: &mut Vec<Line<'static>>, width: u16) {
    let rule_width = usize::from(width.saturating_sub(2));
    lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled("\u{2500}".repeat(rule_width), Style::default().fg(theme::DIM)),
    ]));
}

/// Append the GIT section to `lines`. Always renders header + path.
/// Branch (with right-justified aggregate totals when the snapshot
/// carries a diff) + file tree are gated on the active session's
/// `git_diff_snapshot` — `None` (no scan yet) stops after the path
/// row.
fn append_git_section(lines: &mut Vec<Line<'static>>, app: &App, width: u16) {
    // Section header — DIM bold, flush against the rule above
    // (mirrors `TASKS`).
    lines.push(Line::from(Span::styled(
        "  GIT".to_owned(),
        Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD),
    )));
    // Blank between header and content.
    lines.push(Line::default());

    // Path row — always rendered. Head-truncated so the leaf
    // (project name) is preserved when the path overflows.
    let path_budget = usize::from(width).saturating_sub(usize::from(PANE_PAD));
    let path_value = fit_path_head_truncated(app.cwd(), path_budget);
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(path_value, Style::default().fg(theme::DIM)),
    ]));

    let Some(snapshot) = app.active_session().and_then(|s| s.git_diff_snapshot.as_ref()) else {
        // Pre-first-scan window: only the path is known.
        return;
    };

    // Carry aggregate totals (worktree dirty OR branch-vs-default,
    // whichever the scanner produced) so they can attach to the
    // branch row. `CleanDefault` / `NoRepo` carry no totals.
    let (files_slice, total_files, totals): (&[GitDiffFile], usize, Option<(u32, u32)>) =
        match &snapshot.view {
            GitDiffView::NoRepo | GitDiffView::CleanDefault => (&[], 0, None),
            GitDiffView::Worktree { files, total_files, total_added, total_removed }
            | GitDiffView::BranchVsDefault { files, total_files, total_added, total_removed } => {
                (files.as_slice(), *total_files, Some((*total_added, *total_removed)))
            }
        };

    // Branch row — gated on the resolved branch state. `NoRepo` and
    // `Unknown` collapse to no row. Aggregate `+N -M` totals (when
    // there's a diff) right-justify to the pane edge alongside the
    // branch name.
    if let Some((label, color)) = branch_row_for(snapshot) {
        lines.push(branch_line(width, &label, color, totals));
    }

    if files_slice.is_empty() {
        return;
    }

    // Blank between the branch row and the file tree.
    lines.push(Line::default());

    let tree = build_tree(files_slice);
    render_tree(lines, &tree, width);

    // Overflow row when the trimmed top-N is shorter than the total
    // changed-files count.
    if total_files > files_slice.len() {
        let more = total_files - files_slice.len();
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("+{more} more"),
                Style::default().fg(theme::DIM).add_modifier(Modifier::ITALIC),
            ),
        ]));
    }
}

/// Resolve the branch row's `(label, color)` for the snapshot.
/// `NoRepo` / `Unknown` collapse to `None` (no row).
fn branch_row_for(snapshot: &GitDiffSnapshot) -> Option<(String, Color)> {
    match &snapshot.branch {
        GitBranch::Named(name) => {
            let on_default = snapshot.default_branch.as_deref().is_some_and(|d| d == name.as_str());
            let color = if on_default { theme::DIM } else { theme::RUST_ORANGE };
            Some((name.clone(), color))
        }
        GitBranch::Detached => Some(("HEAD".to_owned(), theme::STATUS_WARNING)),
        GitBranch::NoRepo | GitBranch::Unknown => None,
    }
}

/// Render the branch row: `  ⎇ <label> …pad… +A -R  `. When `totals`
/// is `None` (clean tree on the default branch) the trailing stats
/// + their pad collapse to nothing, leaving just `  ⎇ <label>`.
fn branch_line(
    width: u16,
    label: &str,
    label_color: Color,
    totals: Option<(u32, u32)>,
) -> Line<'static> {
    let glyph_chrome = usize::from(PANE_PAD) + 2; // "  ⎇ "
    let mut spans = vec![Span::styled("  \u{2387} ".to_owned(), Style::default().fg(theme::DIM))];
    let Some((added, removed)) = totals else {
        // No diff — render the branch name without a stats column.
        // Truncate to the remaining budget if the branch name itself
        // overflows.
        let label_budget = usize::from(width).saturating_sub(glyph_chrome);
        let fitted = truncate_with_ellipsis(label, label_budget);
        spans.push(Span::styled(fitted, Style::default().fg(label_color)));
        return Line::from(spans);
    };

    let added_str = format!("+{added}");
    let removed_str = format!("-{removed}");
    let stats_width = added_str.chars().count() + 1 + removed_str.chars().count();
    // Reserve `PATH_STATS_GAP` between label and stats so a wide
    // branch name can't butt up against the numbers.
    let label_budget = usize::from(width)
        .saturating_sub(glyph_chrome)
        .saturating_sub(PATH_STATS_GAP)
        .saturating_sub(stats_width)
        .saturating_sub(usize::from(PANE_PAD));
    let fitted = truncate_with_ellipsis(label, label_budget);
    let label_chars = fitted.chars().count();
    let pad = usize::from(width)
        .saturating_sub(glyph_chrome)
        .saturating_sub(label_chars)
        .saturating_sub(stats_width)
        .saturating_sub(usize::from(PANE_PAD));
    spans.push(Span::styled(fitted, Style::default().fg(label_color)));
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(added_str, Style::default().fg(Color::Green)));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(removed_str, Style::default().fg(Color::Red)));
    spans.push(Span::raw("  "));
    Line::from(spans)
}

/// Tree node built from the diff's file list — a single trie node
/// with either a `file_stats` leaf or zero+ `children` (a
/// directory). After construction we fold any non-root dir whose
/// only child is itself a dir, collapsing chains like
/// `crates/forge-tui/src` into a single labelled row.
struct TreeNode {
    /// Display label for this node. Folded chains are joined with
    /// `/` here (e.g. `forge-agent/src/env`). The implicit root has
    /// an empty label and is never rendered.
    label: String,
    /// `Some` for file leaves (carries `(added, removed)`), `None`
    /// for directories.
    file_stats: Option<(u32, u32)>,
    /// Sorted children: directories first (alpha), then files
    /// (alpha). Empty for file leaves.
    children: Vec<TreeNode>,
}

impl TreeNode {
    fn is_dir(&self) -> bool {
        self.file_stats.is_none()
    }
}

/// Build a tree from the (already top-N trimmed + change-sorted)
/// file list. Resulting tree has the implicit root with each
/// distinct top-level component as a direct child.
fn build_tree(files: &[GitDiffFile]) -> TreeNode {
    let mut root = TreeNode { label: String::new(), file_stats: None, children: Vec::new() };
    for file in files {
        insert_file(&mut root, &file.path, (file.added, file.removed));
    }
    sort_tree(&mut root);
    fold_single_child_dirs(&mut root);
    root
}

fn insert_file(node: &mut TreeNode, path: &str, stats: (u32, u32)) {
    let mut components = path.split('/').filter(|c| !c.is_empty());
    let Some(first) = components.next() else {
        // Empty path — ignore (defensive; the scanner doesn't emit
        // empty paths but the renderer shouldn't panic if it ever
        // does).
        return;
    };
    let rest: Vec<&str> = components.collect();
    if rest.is_empty() {
        // Leaf — attach as a file child. Duplicate file names in the
        // same directory would overwrite here, but git's diff output
        // can't produce that shape (every path is unique).
        node.children.push(TreeNode {
            label: first.to_owned(),
            file_stats: Some(stats),
            children: Vec::new(),
        });
        return;
    }
    // Find or insert the directory child for `first`.
    let dir_idx = node
        .children
        .iter()
        .position(|child| child.is_dir() && child.label == first)
        .unwrap_or_else(|| {
            node.children.push(TreeNode {
                label: first.to_owned(),
                file_stats: None,
                children: Vec::new(),
            });
            node.children.len() - 1
        });
    let remainder = rest.join("/");
    insert_file(&mut node.children[dir_idx], &remainder, stats);
}

/// Sort children: directories first (alpha), then files (alpha).
/// Recurses into directory children.
fn sort_tree(node: &mut TreeNode) {
    node.children.sort_by(|a, b| match (a.is_dir(), b.is_dir()) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.label.cmp(&b.label),
    });
    for child in &mut node.children {
        sort_tree(child);
    }
}

/// Fold any directory whose only child is itself a directory by
/// concatenating labels with `/`. Repeatedly applies until no more
/// folding is possible. Recurses depth-first so deep chains
/// (`crates/forge-tui/src`) collapse in one pass.
fn fold_single_child_dirs(node: &mut TreeNode) {
    for child in &mut node.children {
        fold_single_child_dirs(child);
    }
    // Now fold this node's directory children whose only child is a
    // directory. (Files cannot be folded onto their parent — that
    // would lose the file's stats row.)
    for child in &mut node.children {
        while child.is_dir() && child.children.len() == 1 && child.children[0].is_dir() {
            let mut only = child.children.remove(0);
            child.label.push('/');
            child.label.push_str(&only.label);
            child.children = std::mem::take(&mut only.children);
        }
    }
}

/// Render the tree under `root`'s implicit-root children. Each
/// top-level entry renders flush with the pane indent (no connector);
/// deeper entries gain `├─` / `└─` connectors with `│  ` /  `   `
/// continuation prefixes for ancestor levels.
fn render_tree(lines: &mut Vec<Line<'static>>, root: &TreeNode, width: u16) {
    let count = root.children.len();
    for (idx, child) in root.children.iter().enumerate() {
        let is_last = idx + 1 == count;
        render_tree_node(lines, child, "", true, is_last, width);
    }
}

fn render_tree_node(
    lines: &mut Vec<Line<'static>>,
    node: &TreeNode,
    prefix: &str,
    is_top_level: bool,
    is_last: bool,
    width: u16,
) {
    let connector = if is_top_level {
        ""
    } else if is_last {
        "\u{2514}\u{2500} "
    } else {
        "\u{251c}\u{2500} "
    };
    let line_prefix = format!("{prefix}{connector}");
    lines.push(tree_row(width, &line_prefix, &node.label, node.file_stats));

    if node.children.is_empty() {
        return;
    }
    // Compute the prefix continuation for THIS node's children.
    // Top-level entries don't have a visible connector above them so
    // their children start with no continuation prefix; deeper nodes
    // append `│  ` (not-last) or `   ` (last) to mark which ancestor
    // columns still have unfinished siblings below.
    let continuation = if is_top_level {
        String::new()
    } else if is_last {
        format!("{prefix}   ")
    } else {
        format!("{prefix}\u{2502}  ")
    };
    let count = node.children.len();
    for (idx, child) in node.children.iter().enumerate() {
        let child_is_last = idx + 1 == count;
        render_tree_node(lines, child, &continuation, false, child_is_last, width);
    }
}

/// One tree row: pane indent + tree prefix + label + (for file
/// leaves) right-justified `+N -M` stats. Directory rows render the
/// label DIM with no stats column.
fn tree_row(
    width: u16,
    tree_prefix: &str,
    label: &str,
    file_stats: Option<(u32, u32)>,
) -> Line<'static> {
    let prefix_chars = tree_prefix.chars().count();
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(tree_prefix.to_owned(), Style::default().fg(theme::DIM)),
    ];

    let Some((added, removed)) = file_stats else {
        // Directory row — just the label, no stats. Truncate the
        // label if it overflows the available width (rare in
        // practice since folding keeps single-child chains
        // collapsed).
        let label_budget = usize::from(width)
            .saturating_sub(usize::from(PANE_PAD))
            .saturating_sub(prefix_chars)
            .saturating_sub(usize::from(PANE_PAD));
        let fitted = truncate_with_ellipsis(label, label_budget);
        spans.push(Span::styled(fitted, Style::default().fg(theme::DIM)));
        return Line::from(spans);
    };

    let added_str = format!("+{added}");
    let removed_str = format!("-{removed}");
    let stats_width = added_str.chars().count() + 1 + removed_str.chars().count();
    // Reserve `PATH_STATS_GAP` cols between label and stats so the
    // stats column always has breathing room even when the filename
    // is exactly at budget.
    let label_budget = usize::from(width)
        .saturating_sub(usize::from(PANE_PAD))
        .saturating_sub(prefix_chars)
        .saturating_sub(PATH_STATS_GAP)
        .saturating_sub(stats_width)
        .saturating_sub(usize::from(PANE_PAD));
    let fitted = truncate_with_ellipsis(label, label_budget);
    let label_chars = fitted.chars().count();
    let pad = usize::from(width)
        .saturating_sub(usize::from(PANE_PAD))
        .saturating_sub(prefix_chars)
        .saturating_sub(label_chars)
        .saturating_sub(stats_width)
        .saturating_sub(usize::from(PANE_PAD));
    spans.push(Span::styled(fitted, Style::default().fg(theme::DIM)));
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(added_str, Style::default().fg(Color::Green)));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(removed_str, Style::default().fg(Color::Red)));
    spans.push(Span::raw("  "));
    Line::from(spans)
}

/// Head-truncate `s` to at most `max_chars` characters with a
/// leading `…` ellipsis. Preserves the tail (so the leaf component
/// of a path / filename stays visible). When `s` contains `/`
/// separators the truncation prefers to drop whole leading
/// components (yielding `…/foo/bar.rs` rather than chopping
/// mid-name); falls back to character-level head-truncation when
/// even the basename is too long. Returns the original string when
/// it already fits; collapses to `…` at `max_chars` ≤ 1.
fn fit_path_head_truncated(s: &str, max_chars: usize) -> String {
    let total = s.chars().count();
    if total <= max_chars {
        return s.to_owned();
    }
    if max_chars <= 1 {
        return "\u{2026}".to_owned();
    }
    // Try component-aware truncation: walk left-to-right over the
    // path components and return the first tail that fits when
    // prefixed with `…/`. Lands at a `/` boundary so the result
    // reads as a clean partial path rather than a chopped string.
    let components: Vec<&str> = s.split('/').collect();
    if components.len() > 1 {
        for start in 1..components.len() {
            let tail = components[start..].join("/");
            let with_prefix = format!("\u{2026}/{tail}");
            if with_prefix.chars().count() <= max_chars {
                return with_prefix;
            }
        }
    }
    // Even the basename overflows — fall back to char-level cut
    // so we at least preserve the trailing characters.
    let keep = max_chars - 1;
    let skip = total - keep;
    let mut out = String::from("\u{2026}");
    out.extend(s.chars().skip(skip));
    out
}

fn append_tasks_section(lines: &mut Vec<Line<'static>>, app: &App, width: u16) {
    let todos = app.todos();

    // Verification nudge row sits between the rule and the TASKS
    // header when the flag is set. Dim-yellow one-liner.
    if app.todo_verification_nudge() {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "\u{26a0} verify before declaring complete".to_owned(),
                Style::default().fg(theme::STATUS_WARNING),
            ),
        ]));
        lines.push(Line::default());
    }

    if todos.is_empty() {
        return;
    }

    // TASKS section header — DIM bold, 2-col indent (matches the
    // left pane's `ACTIVE` / `INACTIVE` section headers).
    lines.push(Line::from(Span::styled(
        "  TASKS".to_owned(),
        Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD),
    )));
    // Blank between header and first item.
    lines.push(Line::default());

    // Item rendering budget: full width minus the 2-col indent, the
    // 1-col glyph, and the 1-col space after the glyph. Continuation
    // lines for the wrapped in-progress item indent under the text
    // column (start col 5 from the pane's x=0).
    let glyph_indent = PANE_PAD + 2; // "  " + glyph + " "
    let text_budget = usize::from(width.saturating_sub(glyph_indent));

    let todo_count = todos.len();
    for (idx, todo) in todos.iter().enumerate() {
        let (glyph, glyph_color) = match todo.status {
            TodoStatus::Completed => ("\u{2713}", Color::Green), // ✓
            TodoStatus::InProgress => ("\u{25b8}", theme::RUST_ORANGE), // ▸
            TodoStatus::Pending => ("\u{25cb}", theme::DIM),     // ○
        };
        let text_style = match todo.status {
            TodoStatus::Completed => {
                Style::default().fg(theme::DIM).add_modifier(Modifier::CROSSED_OUT)
            }
            TodoStatus::InProgress => {
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
            }
            TodoStatus::Pending => Style::default().fg(Color::Gray),
        };
        let display_text = if todo.status == TodoStatus::InProgress && !todo.active_form.is_empty()
        {
            todo.active_form.clone()
        } else {
            todo.content.clone()
        };

        if todo.status == TodoStatus::InProgress {
            // Wrap onto continuation lines, indented under the text
            // column so the glyph stays visually associated with the
            // first wrapped row.
            let wrapped = wrap_text(&display_text, text_budget);
            let mut iter = wrapped.into_iter();
            if let Some(first) = iter.next() {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(glyph.to_owned(), Style::default().fg(glyph_color)),
                    Span::raw(" "),
                    Span::styled(first, text_style),
                ]));
            } else {
                // Empty `display_text` — still render the glyph row
                // so the pane shape stays consistent.
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(glyph.to_owned(), Style::default().fg(glyph_color)),
                ]));
            }
            for rest in iter {
                lines.push(Line::from(vec![
                    Span::raw(" ".repeat(usize::from(glyph_indent))),
                    Span::styled(rest, text_style),
                ]));
            }
        } else {
            // Truncate with `…` at the right edge.
            let truncated = truncate_with_ellipsis(&display_text, text_budget);
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(glyph.to_owned(), Style::default().fg(glyph_color)),
                Span::raw(" "),
                Span::styled(truncated, text_style),
            ]));
        }
        // Blank between tasks for breathing room. Skipped after the
        // last item so we don't leave a trailing blank at the end of
        // the TASKS section.
        if idx + 1 < todo_count {
            lines.push(Line::default());
        }
    }
}

/// Wrap `s` onto multiple lines so that each piece fits within
/// `max_chars` columns. Breaks on whitespace where possible; falls
/// back to hard-cut on long single tokens. Returns an empty `Vec`
/// for an empty / whitespace-only input.
fn wrap_text(s: &str, max_chars: usize) -> Vec<String> {
    if max_chars == 0 {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in s.split_whitespace() {
        let word_chars = word.chars().count();
        if word_chars > max_chars {
            // Long single token — flush current, then hard-cut the
            // long word across multiple lines.
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            let mut chars = word.chars().peekable();
            while chars.peek().is_some() {
                let piece: String = chars.by_ref().take(max_chars).collect();
                out.push(piece);
            }
            continue;
        }
        if current.is_empty() {
            current.push_str(word);
        } else if current.chars().count() + 1 + word_chars <= max_chars {
            current.push(' ');
            current.push_str(word);
        } else {
            out.push(std::mem::take(&mut current));
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Truncate `s` to at most `max_chars` characters with a trailing
/// `…` ellipsis. Returns the original string if it already fits.
/// When `max_chars` is `0` or `1` the result is just `…`.
fn truncate_with_ellipsis(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_owned();
    }
    if max_chars <= 1 {
        return "\u{2026}".to_owned();
    }
    let mut out: String = s.chars().take(max_chars - 1).collect();
    out.push('\u{2026}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_short_text_returns_single_line() {
        assert_eq!(wrap_text("hello world", 20), vec!["hello world".to_owned()]);
    }

    #[test]
    fn wrap_long_text_breaks_on_whitespace() {
        let wrapped = wrap_text("Adding tests for the near-threshold branch", 18);
        assert_eq!(
            wrapped,
            vec![
                "Adding tests for".to_owned(),
                "the near-threshold".to_owned(),
                "branch".to_owned()
            ]
        );
    }

    #[test]
    fn wrap_long_token_hard_cuts() {
        let wrapped = wrap_text("supercalifragilisticexpialidocious tail", 10);
        // The 34-char token cuts into 10+10+10+4. Remaining `tail`
        // starts its own line because the hard-cut path emits its
        // pieces directly without joining.
        assert_eq!(
            wrapped,
            vec![
                "supercalif".to_owned(),
                "ragilistic".to_owned(),
                "expialidoc".to_owned(),
                "ious".to_owned(),
                "tail".to_owned(),
            ]
        );
    }

    #[test]
    fn wrap_empty_returns_empty_vec() {
        assert!(wrap_text("", 10).is_empty());
        assert!(wrap_text("   ", 10).is_empty());
    }

    #[test]
    fn truncate_short_unchanged() {
        assert_eq!(truncate_with_ellipsis("hi", 10), "hi");
    }

    #[test]
    fn truncate_long_with_ellipsis() {
        assert_eq!(truncate_with_ellipsis("supercalifragilistic", 10), "supercali\u{2026}");
    }

    #[test]
    fn truncate_max_one_returns_just_ellipsis() {
        assert_eq!(truncate_with_ellipsis("anything", 1), "\u{2026}");
    }

    #[test]
    fn head_truncate_short_unchanged() {
        assert_eq!(fit_path_head_truncated("~/Projects/forge", 32), "~/Projects/forge");
    }

    #[test]
    fn head_truncate_keeps_tail_with_leading_ellipsis() {
        // Component-aware truncation lands at a `/` boundary, so the
        // result is `≤ max_chars` (not necessarily exactly equal) and
        // always starts `…/` when at least one component was dropped.
        let out = fit_path_head_truncated("~/Projects/forge/crates/forge-tui", 16);
        assert!(out.chars().count() <= 16, "got {out:?}");
        assert!(out.starts_with("\u{2026}/"), "got {out:?}");
        assert!(out.ends_with("forge-tui"), "got {out:?}");
    }

    #[test]
    fn head_truncate_drops_leading_components_first() {
        // 29-char budget — too tight for the full path, but
        // `…/src/env/git_diff.rs` (21 chars) fits cleanly at a
        // component boundary.
        let out = fit_path_head_truncated("crates/forge-agent/src/env/git_diff.rs", 29);
        assert_eq!(out, "\u{2026}/src/env/git_diff.rs");
    }

    #[test]
    fn head_truncate_basename_overflow_falls_back_to_char_cut() {
        // No `/` separators at all — has to char-cut.
        let out = fit_path_head_truncated("supercalifragilisticexpialidocious", 10);
        assert_eq!(out.chars().count(), 10);
        assert!(out.starts_with('\u{2026}'));
        assert!(out.ends_with("docious"));
    }

    #[test]
    fn head_truncate_max_one_returns_just_ellipsis() {
        assert_eq!(fit_path_head_truncated("anything", 1), "\u{2026}");
    }

    fn snap(branch: GitBranch, default: Option<&str>, view: GitDiffView) -> GitDiffSnapshot {
        GitDiffSnapshot { branch, default_branch: default.map(str::to_owned), view }
    }

    #[test]
    fn branch_row_named_default_renders_dim() {
        let s = snap(GitBranch::Named("main".into()), Some("main"), GitDiffView::CleanDefault);
        let (label, color) = branch_row_for(&s).expect("named branch should render a row");
        assert_eq!(label, "main");
        assert_eq!(color, theme::DIM);
    }

    #[test]
    fn branch_row_named_feature_renders_rust_orange() {
        let s = snap(GitBranch::Named("feat/x".into()), Some("main"), GitDiffView::CleanDefault);
        let (label, color) = branch_row_for(&s).expect("feature branch should render a row");
        assert_eq!(label, "feat/x");
        assert_eq!(color, theme::RUST_ORANGE);
    }

    #[test]
    fn branch_row_named_unknown_default_renders_rust_orange() {
        // `default_branch == None` means we can't prove the branch IS
        // the default, so the feature-branch styling applies.
        let s = snap(GitBranch::Named("main".into()), None, GitDiffView::CleanDefault);
        let (_label, color) = branch_row_for(&s).expect("named branch should render a row");
        assert_eq!(color, theme::RUST_ORANGE);
    }

    #[test]
    fn branch_row_detached_renders_yellow() {
        let s = snap(GitBranch::Detached, Some("main"), GitDiffView::CleanDefault);
        let (label, color) = branch_row_for(&s).expect("detached HEAD should render a row");
        assert_eq!(label, "HEAD");
        assert_eq!(color, theme::STATUS_WARNING);
    }

    #[test]
    fn branch_row_no_repo_collapses_to_none() {
        let s = snap(GitBranch::NoRepo, None, GitDiffView::NoRepo);
        assert!(branch_row_for(&s).is_none());
    }

    #[test]
    fn branch_row_unknown_collapses_to_none() {
        let s = snap(GitBranch::Unknown, None, GitDiffView::CleanDefault);
        assert!(branch_row_for(&s).is_none());
    }

    fn file(path: &str, added: u32, removed: u32) -> GitDiffFile {
        GitDiffFile { path: path.to_owned(), added, removed }
    }

    /// One row in the flattened tree shape used by the tree-builder
    /// tests: `(depth, label, file_stats)`. Aliased here to dodge
    /// clippy's `type_complexity` lint without an inline `#[allow]`.
    type FlatRow = (usize, String, Option<(u32, u32)>);

    /// Build the tree from a representative diff and walk it to a
    /// flat `(depth, label, file_stats)` list so the test reads as a
    /// structure-checking shape rather than poking at private fields.
    fn flatten(tree: &TreeNode, depth: usize, out: &mut Vec<FlatRow>) {
        if !tree.label.is_empty() {
            out.push((depth, tree.label.clone(), tree.file_stats));
        }
        for child in &tree.children {
            flatten(child, if tree.label.is_empty() { depth } else { depth + 1 }, out);
        }
    }

    #[test]
    fn build_tree_folds_single_child_directory_chains() {
        let files = vec![
            file("crates/forge-agent/src/env/git_diff.rs", 648, 0),
            file("crates/forge-agent/src/env/git.rs", 0, 559),
        ];
        let tree = build_tree(&files);
        let mut flat = Vec::new();
        flatten(&tree, 0, &mut flat);
        assert_eq!(
            flat,
            vec![
                (0, "crates/forge-agent/src/env".to_owned(), None),
                (1, "git.rs".to_owned(), Some((0, 559))),
                (1, "git_diff.rs".to_owned(), Some((648, 0))),
            ]
        );
    }

    #[test]
    fn build_tree_stops_folding_at_first_split() {
        // `crates` has two dir children → don't fold.
        // `forge-tui` has one dir child (`src`) → fold into `forge-tui/src`.
        // `forge-tui/src` has two dir children (`app`, `ui`) → stop.
        let files = vec![
            file("crates/forge-agent/src/env/git_diff.rs", 648, 0),
            file("crates/forge-tui/src/app/git_diff.rs", 427, 0),
            file("crates/forge-tui/src/ui/inspector_pane.rs", 340, 21),
        ];
        let tree = build_tree(&files);
        let mut flat = Vec::new();
        flatten(&tree, 0, &mut flat);
        assert_eq!(
            flat,
            vec![
                (0, "crates".to_owned(), None),
                (1, "forge-agent/src/env".to_owned(), None),
                (2, "git_diff.rs".to_owned(), Some((648, 0))),
                (1, "forge-tui/src".to_owned(), None),
                (2, "app".to_owned(), None),
                (3, "git_diff.rs".to_owned(), Some((427, 0))),
                (2, "ui".to_owned(), None),
                (3, "inspector_pane.rs".to_owned(), Some((340, 21))),
            ]
        );
    }

    #[test]
    fn build_tree_directories_sort_before_files_within_a_node() {
        let files = vec![
            file("Cargo.toml", 3, 1),
            file("crates/forge-tui/src/app.rs", 10, 2),
            file("README.md", 1, 0),
        ];
        let tree = build_tree(&files);
        let mut flat = Vec::new();
        flatten(&tree, 0, &mut flat);
        // `crates/...` (folded) directory entry should come first;
        // then files in alpha order at the root level.
        assert_eq!(
            flat,
            vec![
                (0, "crates/forge-tui/src".to_owned(), None),
                (1, "app.rs".to_owned(), Some((10, 2))),
                (0, "Cargo.toml".to_owned(), Some((3, 1))),
                (0, "README.md".to_owned(), Some((1, 0))),
            ]
        );
    }

    #[test]
    fn build_tree_does_not_fold_dir_with_single_file_child() {
        // `ui` has one child but it's a file — fold rule is dir-dir
        // only, so `ui` stays as its own row above the file leaf.
        let files = vec![file("crates/forge-tui/src/ui/inspector_pane.rs", 340, 21)];
        let tree = build_tree(&files);
        let mut flat = Vec::new();
        flatten(&tree, 0, &mut flat);
        assert_eq!(
            flat,
            vec![
                // The whole chain folds because every directory has
                // a single dir child up to the leaf's parent.
                (0, "crates/forge-tui/src/ui".to_owned(), None),
                (1, "inspector_pane.rs".to_owned(), Some((340, 21))),
            ]
        );
    }
}
