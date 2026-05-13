//! Inspector pane (right side, Wide + Medium tiers; full-screen
//! overlay at Narrow tier).
//!
//! Mirror of the left [`crate::ui::projects_pane`] in chrome and
//! tier behaviour. Two sections:
//!
//! - `GIT` — always rendered. Shows the active session's cwd plus a
//!   snapshot-driven branch row, subtitle (`worktree` /
//!   `vs <default>`), aggregate `+N -M` totals, and the top file
//!   list — sourced from `UiSession.git_diff_snapshot`.
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
/// Narrow overlay render.
fn append_body(lines: &mut Vec<Line<'static>>, app: &App, width: u16) {
    append_git_section(lines, app, width);

    let todos = app.todos();
    let has_tasks = !todos.is_empty() || app.todo_verification_nudge();
    if has_tasks {
        // Blank separator between GIT and TASKS sections.
        lines.push(Line::default());
        append_tasks_section(lines, app, width);
    }
}

/// Append the GIT section to `lines`. Always renders header + path.
/// Branch, subtitle, totals, and file list are gated on the
/// active session's `git_diff_snapshot` — `None` (no scan yet)
/// stops after the path row.
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

    // Branch row — gated on the resolved branch state. `NoRepo` and
    // `Unknown` collapse to no row.
    if let Some((label, color)) = branch_row_for(snapshot) {
        let branch_chrome = usize::from(PANE_PAD) + 2; // "  ⎇ "
        let branch_budget = usize::from(width).saturating_sub(branch_chrome);
        let label = truncate_with_ellipsis(&label, branch_budget);
        lines.push(Line::from(vec![
            Span::styled("  \u{2387} ".to_owned(), Style::default().fg(theme::DIM)),
            Span::styled(label, Style::default().fg(color)),
        ]));
    }

    match &snapshot.view {
        GitDiffView::NoRepo | GitDiffView::CleanDefault => {
            // No subtitle, no file list. Path (+ optional branch) is the whole section.
        }
        GitDiffView::Worktree { files, total_files, total_added, total_removed } => {
            append_diff_block(
                lines,
                width,
                DiffSectionHeader { label: "worktree", color: theme::STATUS_WARNING },
                DiffSectionTotals {
                    files,
                    total_files: *total_files,
                    total_added: *total_added,
                    total_removed: *total_removed,
                },
            );
        }
        GitDiffView::BranchVsDefault { files, total_files, total_added, total_removed } => {
            // `BranchVsDefault` is only constructed by the scanner
            // when a default branch resolved (see `git_diff.rs` —
            // `default_branch.as_deref()`'s `let-else` returns
            // `CleanDefault` when `None`). The `unwrap_or_default`
            // is a defensive fallback for future drift; an empty
            // string renders as `vs ` which is preferable to a
            // panic.
            let default = snapshot.default_branch.as_deref().unwrap_or_default();
            let subtitle = format!("vs {default}");
            append_diff_block(
                lines,
                width,
                DiffSectionHeader { label: &subtitle, color: theme::DIM },
                DiffSectionTotals {
                    files,
                    total_files: *total_files,
                    total_added: *total_added,
                    total_removed: *total_removed,
                },
            );
        }
    }
}

/// Subtitle label + colour for a diff section. `worktree` renders
/// in `STATUS_WARNING`; `vs <default>` renders in `DIM`.
#[derive(Clone, Copy)]
struct DiffSectionHeader<'a> {
    label: &'a str,
    color: Color,
}

/// Aggregate stats + per-file rows for a diff section. `files` is
/// already trimmed to the top-N; `total_files` covers the full
/// changed-file count for the `+N more` overflow line.
#[derive(Clone, Copy)]
struct DiffSectionTotals<'a> {
    files: &'a [GitDiffFile],
    total_files: usize,
    total_added: u32,
    total_removed: u32,
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

/// Append the diff block (blank + subtitle row + blank + file rows +
/// overflow indicator) under the branch row.
fn append_diff_block(
    lines: &mut Vec<Line<'static>>,
    width: u16,
    header: DiffSectionHeader<'_>,
    totals: DiffSectionTotals<'_>,
) {
    // Blank between the branch row and the subtitle.
    lines.push(Line::default());

    // Subtitle row with right-justified `+N -M` totals. Layout:
    // `  <subtitle> …pad…  +N -M  ` — left indent + right gutter
    // both `PANE_PAD`.
    lines.push(stat_line(
        width,
        header.label,
        header.color,
        totals.total_added,
        totals.total_removed,
    ));

    // Blank between the subtitle row and the per-file rows. When
    // there are no per-file rows (empty list — defensive, only if
    // git reported counts but no parseable numstat lines), we still
    // emit the blank for visual consistency with the populated case.
    if !totals.files.is_empty() {
        lines.push(Line::default());
    }

    for file in totals.files {
        let f_added = format!("+{}", file.added);
        let f_removed = format!("-{}", file.removed);
        let f_added_chars = f_added.chars().count();
        let f_removed_chars = f_removed.chars().count();
        // Path budget = width - left indent - stats column - right gutter.
        let stats_width = f_added_chars + 1 + f_removed_chars; // `+N -M`
        let path_budget = usize::from(width)
            .saturating_sub(usize::from(PANE_PAD)) // left indent
            .saturating_sub(stats_width)
            .saturating_sub(usize::from(PANE_PAD)); // right gutter
        let path_value = fit_path_head_truncated(&file.path, path_budget);
        let path_chars = path_value.chars().count();
        let pad = usize::from(width)
            .saturating_sub(usize::from(PANE_PAD))
            .saturating_sub(path_chars)
            .saturating_sub(stats_width)
            .saturating_sub(usize::from(PANE_PAD));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(path_value, Style::default().fg(theme::DIM)),
            Span::raw(" ".repeat(pad)),
            Span::styled(f_added, Style::default().fg(Color::Green)),
            Span::raw(" "),
            Span::styled(f_removed, Style::default().fg(Color::Red)),
            Span::raw("  "),
        ]));
    }

    // Overflow row when the trimmed top-N is shorter than the total
    // changed-files count.
    if totals.total_files > totals.files.len() {
        let more = totals.total_files - totals.files.len();
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("+{more} more"),
                Style::default().fg(theme::DIM).add_modifier(Modifier::ITALIC),
            ),
        ]));
    }
}

/// One line laying out `  <label>  …pad…  +A -R  ` for the subtitle
/// row. Path-row layout uses the same shape but with per-file
/// budgeting — see `append_diff_block`.
fn stat_line(
    width: u16,
    label: &str,
    label_color: Color,
    added: u32,
    removed: u32,
) -> Line<'static> {
    let added_str = format!("+{added}");
    let removed_str = format!("-{removed}");
    let added_chars = added_str.chars().count();
    let removed_chars = removed_str.chars().count();
    let label_chars = label.chars().count();
    let stats_width = added_chars + 1 + removed_chars;
    let pad = usize::from(width)
        .saturating_sub(usize::from(PANE_PAD))
        .saturating_sub(label_chars)
        .saturating_sub(stats_width)
        .saturating_sub(usize::from(PANE_PAD));
    Line::from(vec![
        Span::raw("  "),
        Span::styled(label.to_owned(), Style::default().fg(label_color)),
        Span::raw(" ".repeat(pad)),
        Span::styled(added_str, Style::default().fg(Color::Green)),
        Span::raw(" "),
        Span::styled(removed_str, Style::default().fg(Color::Red)),
        Span::raw("  "),
    ])
}

/// Head-truncate `s` to at most `max_chars` characters with a
/// leading `…` ellipsis. Preserves the tail (so the leaf component
/// of a path / filename stays visible). Returns the original
/// string when it already fits; collapses to `…` at `max_chars` ≤ 1.
fn fit_path_head_truncated(s: &str, max_chars: usize) -> String {
    let total = s.chars().count();
    if total <= max_chars {
        return s.to_owned();
    }
    if max_chars <= 1 {
        return "\u{2026}".to_owned();
    }
    let keep = max_chars - 1; // room for the leading ellipsis
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
        // 12 chars budget; the tail (after the dropped head) plus the
        // leading `…` must come out to exactly 12 chars.
        let out = fit_path_head_truncated("~/Projects/forge/crates/forge-tui", 12);
        assert_eq!(out.chars().count(), 12);
        assert!(out.starts_with('\u{2026}'));
        assert!(out.ends_with("forge-tui"));
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
}
