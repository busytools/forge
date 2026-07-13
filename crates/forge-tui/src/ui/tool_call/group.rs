//! Render the L2 summary for a grouped run of consecutive
//! collapsed-by-default tool calls. EVERY run - single-kind or many -
//! renders one consistent tree: a parent count row with one `├─`/`└─`
//! child per kind, matching the projects-pane connectors + `│` spine so
//! chat and the side panes read as one tree system. Description kinds
//! (bash / web / lsp / ...) word-wrap their target across continuation
//! rows with the spine held; search clips; read nests one child row per
//! file (project-root-relative, middle-ellipsis when too wide), a lone
//! file riding the read row inline.
//!
//! The L1 (title rows) and L0 (full bodies) levels are produced by the
//! standard per-tool render path threaded with a `force_collapsed`
//! flag from the caller.
//!
//! See `docs/superpowers/specs/2026-06-01-chat-tool-grouping-v2.md`
//! decision 6.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::agent::model::ToolCallStatus;
use crate::ui::chat_tree;
use crate::ui::message::grouping::{KindSummary, READ_GLYPH, kind_target_wraps};
use crate::ui::theme;
use crate::ui::tool_call::status_icon;

/// Trailing affordance on the parent count row.
const EXPAND_HINT: &str = "   ctrl+x to expand";

/// Absolute floor for a target slot even on extremely narrow chat
/// areas, so something useful always renders.
const MIN_TARGET_BUDGET: usize = 8;

/// Render the L2 summary tree for a grouped run (the module doc has the
/// shape). `aggregate_status` drives the parent status_icon;
/// `spinner_glyph` is the active style's current frame (used only while
/// InProgress) and animates without a re-measure - only the leading icon
/// cell changes. `project_root` is the base read paths relativize against.
pub fn render_group_summary_line(
    summary: &KindSummary,
    aggregate_status: ToolCallStatus,
    spinner_glyph: char,
    max_width: usize,
    project_root: Option<&str>,
) -> Vec<Line<'static>> {
    let (icon_glyph, icon_color) = status_icon(aggregate_status, spinner_glyph);
    let icon_style = Style::default().fg(icon_color).add_modifier(Modifier::BOLD);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(theme::DIM);

    // Wrapping is pre-computed here so the outer message layout (which
    // char-wraps WITHOUT the tree gutter) never re-wraps a row and shears
    // the tree - see the module doc / message.rs gotcha.
    let mark = Style::default().fg(theme::DIM);
    let total = summary.total();
    let mut lines = vec![Line::from(vec![
        Span::raw("  ".to_owned()),
        Span::styled(format!("{icon_glyph} "), icon_style),
        Span::styled(
            format!("{total} {}", if total == 1 { "tool call" } else { "tool calls" }),
            bold,
        ),
        Span::styled(EXPAND_HINT.to_owned(), dim),
    ])];

    let n = summary.lines.len();
    let label_w = summary.lines.iter().map(|l| cells(&l.label)).max().unwrap_or(0);
    for (i, line) in summary.lines.iter().enumerate() {
        let last = i + 1 == n;
        let connector = chat_tree::connector(last);
        // The spine under a kind holds `│` while a later kind follows,
        // blank on the last kind (matches the projects-pane tree).
        let spine = if last { " " } else { chat_tree::SPINE };

        // Read nests one child row per file (project-root-relative)
        // rather than an inline target. Paths middle-ellipsis when wider
        // than the row so the filename stays visible. A path-less read
        // falls through to the capped form below so its count still shows.
        let read_paths: Vec<String> = if line.glyph == READ_GLYPH {
            read_child_paths(line, project_root)
        } else {
            Vec::new()
        };
        if line.glyph == READ_GLYPH && !read_paths.is_empty() {
            if let [only] = read_paths.as_slice()
                && line.count == 1
            {
                // A genuinely single read rides the read row inline (no
                // child row). Guard on `count == 1` too: a multi-call
                // read that only resolved one path must still nest, so
                // the row never implies "1 file" when it was N calls.
                let prefix_cells = 2 + 3 + cells(line.glyph) + 1 + label_w + 1;
                let budget = max_width.saturating_sub(prefix_cells).max(MIN_TARGET_BUDGET);
                lines.push(Line::from(vec![
                    Span::raw("  ".to_owned()),
                    Span::styled(connector.to_owned(), mark),
                    Span::styled(format!("{} ", line.glyph), bold),
                    Span::styled(format!("{} ", pad_right(&line.label, label_w)), bold),
                    Span::styled(clip_middle(only, budget), dim),
                ]));
                continue;
            }
            // Parent read row: glyph + bare label, no inline target.
            lines.push(Line::from(vec![
                Span::raw("  ".to_owned()),
                Span::styled(connector.to_owned(), mark),
                Span::styled(format!("{} ", line.glyph), bold),
                Span::styled(line.label.clone(), bold),
            ]));
            // File children nest one level deeper: base(2) + spine(1) +
            // gap(2) + child connector(3) = path column 8.
            let child_prefix = 2 + 1 + 2 + 3;
            let budget = max_width.saturating_sub(child_prefix).max(MIN_TARGET_BUDGET);
            let child_n = read_paths.len();
            for (ci, path) in read_paths.iter().enumerate() {
                let child_last = ci + 1 == child_n;
                let child_conn = chat_tree::connector(child_last);
                lines.push(Line::from(vec![
                    Span::raw("  ".to_owned()),
                    Span::styled(spine.to_owned(), mark),
                    Span::raw("  ".to_owned()),
                    Span::styled(child_conn.to_owned(), mark),
                    Span::styled(clip_middle(path, budget), dim),
                ]));
            }
            continue;
        }

        // Non-read kind (and path-less read): one row, wrapped
        // (description kinds) or capped (search).
        // indent(2) + connector(3) + glyph+space(1) + label(pad)+space(1)
        // = the column the target text starts at; continuations align
        // to it under the spine.
        let prefix_cells = 2 + 3 + cells(line.glyph) + 1 + label_w + 1;
        let budget = max_width.saturating_sub(prefix_cells).max(MIN_TARGET_BUDGET);
        let target = format_targets(&line.targets, line.count);
        let segments = if kind_target_wraps(line.glyph) {
            wrap_words(&target, budget)
        } else {
            vec![clip_to_width(&target, budget)]
        };
        let mut seg_iter = segments.into_iter();
        let first = seg_iter.next().unwrap_or_default();

        let mut row = vec![
            Span::raw("  ".to_owned()),
            Span::styled(connector.to_owned(), mark),
            Span::styled(format!("{} ", line.glyph), bold),
        ];
        if first.is_empty() {
            // Target-less kind: the label carries no trailing pad/space
            // (avoids a trailing-whitespace row, the #321 guard).
            row.push(Span::styled(line.label.clone(), bold));
        } else {
            row.push(Span::styled(format!("{} ", pad_right(&line.label, label_w)), bold));
            row.push(Span::styled(first, dim));
        }
        lines.push(Line::from(row));

        // Continuation rows: the spine then padding to align the wrapped
        // text under the first row's target column.
        let cont_pad = " ".repeat(prefix_cells.saturating_sub(3));
        for seg in seg_iter {
            lines.push(Line::from(vec![
                Span::raw("  ".to_owned()),
                Span::styled(spine.to_owned(), mark),
                Span::raw(cont_pad.clone()),
                Span::styled(seg, dim),
            ]));
        }
    }
    lines
}

/// The read children of a group: each file relativized against the
/// project root (so the tree shows `crates/...` not the absolute
/// prefix).
fn read_child_paths(
    line: &crate::ui::message::grouping::KindLine,
    project_root: Option<&str>,
) -> Vec<String> {
    line.targets.iter().map(|p| relativize(p, project_root)).collect()
}

/// Make a read `file_path` relative to the project root for display.
/// A path strictly UNDER the root drops the root prefix; anything else
/// (a sibling sharing the root's name, the root itself, a path outside
/// the root, or no root known) shows as-is. The `/` boundary check
/// stops `/repo` from mangling a sibling `/repository/...` into a fake
/// `sitory/...` relative path.
fn relativize(path: &str, project_root: Option<&str>) -> String {
    if let Some(root) = project_root {
        let root = root.trim_end_matches('/');
        if let Some(rest) = path.strip_prefix(root).and_then(|r| r.strip_prefix('/'))
            && !rest.is_empty()
        {
            return rest.to_owned();
        }
    }
    path.to_owned()
}

/// Clip `s` to `budget` display cells keeping the head AND tail with an
/// ASCII `...` in the middle, so a path keeps its filename visible when
/// it is genuinely wider than the row. The tail (filename side) gets the
/// larger half of the budget.
fn clip_middle(s: &str, budget: usize) -> String {
    if cells(s) <= budget {
        return s.to_owned();
    }
    if budget <= 3 {
        return ".".repeat(budget);
    }
    let keep = budget - 3;
    let tail_cells = keep - keep / 2;
    let head_cells = keep - tail_cells;
    let mut head = String::new();
    let mut head_w = 0_usize;
    for c in s.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if head_w + cw > head_cells {
            break;
        }
        head.push(c);
        head_w += cw;
    }
    let mut tail_rev = String::new();
    let mut tail_w = 0_usize;
    for c in s.chars().rev() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if tail_w + cw > tail_cells {
            break;
        }
        tail_rev.push(c);
        tail_w += cw;
    }
    let tail: String = tail_rev.chars().rev().collect();
    format!("{head}...{tail}")
}

/// Greedily word-wrap `s` into segments no wider than `budget` display
/// cells. Splits on ASCII spaces; a word wider than `budget` is
/// hard-split at a cell boundary so no segment overflows - that keeps
/// the outer char-wrap (which has no tree gutter) from firing and
/// shearing the alignment. Never returns empty: empty / all-space input
/// yields one empty segment so the caller still emits a row.
fn wrap_words(s: &str, budget: usize) -> Vec<String> {
    let budget = budget.max(1);
    let mut segments: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_w = 0_usize;
    for word in s.split(' ').filter(|w| !w.is_empty()) {
        let word_w = cells(word);
        if word_w > budget {
            if !current.is_empty() {
                segments.push(std::mem::take(&mut current));
                current_w = 0;
            }
            for chunk in hard_split(word, budget) {
                segments.push(chunk);
            }
            continue;
        }
        let sep = usize::from(!current.is_empty());
        if current_w + sep + word_w > budget {
            segments.push(std::mem::take(&mut current));
            current_w = 0;
        }
        if !current.is_empty() {
            current.push(' ');
            current_w += 1;
        }
        current.push_str(word);
        current_w += word_w;
    }
    if !current.is_empty() {
        segments.push(current);
    }
    if segments.is_empty() {
        segments.push(String::new());
    }
    segments
}

/// Break a single over-budget word into cell-bounded chunks so no
/// chunk exceeds `budget`.
fn hard_split(word: &str, budget: usize) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_w = 0_usize;
    for c in word.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if current_w + cw > budget && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            current_w = 0;
        }
        current.push(c);
        current_w += cw;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn cells(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// A kind line's target text: `t1, t2, t3 +N`, or `×N` for a target-
/// less kind with more than one call, or empty for a lone call.
fn format_targets(targets: &[String], count: usize) -> String {
    if targets.is_empty() {
        return if count > 1 { format!("\u{d7}{count}") } else { String::new() };
    }
    let names = targets.join(", ");
    let overflow = count.saturating_sub(targets.len());
    if overflow > 0 { format!("{names} +{overflow}") } else { names }
}

fn pad_right(s: &str, target_cells: usize) -> String {
    let w = cells(s);
    if w >= target_cells { s.to_owned() } else { format!("{s}{}", " ".repeat(target_cells - w)) }
}

/// Clip `s` to `budget` display cells, appending `...` when over so the
/// result still fits.
fn clip_to_width(s: &str, budget: usize) -> String {
    if cells(s) <= budget {
        return s.to_owned();
    }
    if budget <= 3 {
        return ".".repeat(budget);
    }
    let mut out = String::new();
    let mut acc = 0_usize;
    let cap = budget - 3;
    for c in s.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if acc + cw > cap {
            break;
        }
        out.push(c);
        acc += cw;
    }
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::message::grouping::KindLine;

    fn kl(glyph: &'static str, label: &str, count: usize, targets: &[&str]) -> KindLine {
        KindLine {
            glyph,
            label: label.to_owned(),
            count,
            targets: targets.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn summary(lines: Vec<KindLine>) -> KindSummary {
        KindSummary { lines }
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect::<String>()
    }

    fn render(s: &KindSummary, status: ToolCallStatus, width: usize) -> Vec<Line<'static>> {
        render_group_summary_line(s, status, '\u{280B}', width, None)
    }

    fn render_rooted(
        s: &KindSummary,
        status: ToolCallStatus,
        width: usize,
        root: &str,
    ) -> Vec<Line<'static>> {
        render_group_summary_line(s, status, '\u{280B}', width, Some(root))
    }

    /// A single-kind run is just a one-child tree - the `= ` one-liner
    /// is retired. A lone search renders the parent count row + a single
    /// `└─ ⌕ search <pattern>` child (capped), no `=` / box anywhere.
    #[test]
    fn single_kind_renders_one_child_tree() {
        let s = summary(vec![kl("\u{2315}", "search", 3, &["FooBar", "Baz"])]);
        let lines = render(&s, ToolCallStatus::Completed, 80);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(lines.len(), 2, "parent + one child; got {text:?}");
        assert!(text[0].contains("3 tool calls"), "parent count row: {text:?}");
        assert!(text[0].contains("ctrl+x to expand"));
        assert!(
            text[1].contains("\u{2514}\u{2500} \u{2315} search FooBar, Baz +1"),
            "{:?}",
            text[1]
        );
        assert!(!text.iter().any(|t| t.contains("= ")), "no `=` marker anywhere: {text:?}");
        assert!(!text.iter().any(|t| t.contains('\u{250c}')), "no box corner: {text:?}");
    }

    /// A pure-read run (single kind, all reads) does NOT use the `= `
    /// one-liner - read always nests, so it renders the tree with read
    /// as the sole (`└─`) kind and one child per file.
    #[test]
    fn pure_read_run_nests() {
        let s = summary(vec![kl("\u{2b1a}", "read", 3, &["a.rs", "b.rs", "c.rs"])]);
        let lines = render(&s, ToolCallStatus::Completed, 80);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert!(!text.iter().any(|t| t.contains("= ")), "pure-read must NOT use `=`: {text:?}");
        assert!(text[0].contains("3 tool calls"), "parent count row: {text:?}");
        assert!(text[1].contains("\u{2514}\u{2500} \u{2b1a} read"), "read as sole kind: {text:?}");
        assert!(text.iter().any(|t| t.trim_start().starts_with("\u{251c}\u{2500} a.rs")));
        assert!(text.iter().any(|t| t.trim_start().starts_with("\u{2514}\u{2500} c.rs")));
    }

    /// A lone read of ONE file rides the read row inline (no separate
    /// child row) - the tighter form for the common single-read case.
    #[test]
    fn single_read_one_file_inlines() {
        let s = summary(vec![kl("\u{2b1a}", "read", 1, &["src/main.rs"])]);
        let lines = render(&s, ToolCallStatus::Completed, 80);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(lines.len(), 2, "parent + one inline read row; got {text:?}");
        assert!(text[0].contains("1 tool call"), "parent count row: {text:?}");
        assert!(text[1].contains("\u{2514}\u{2500} \u{2b1a} read src/main.rs"), "inline: {text:?}");
    }

    /// A read with more CALLS than resolved paths (3 calls, one path
    /// extracted) must NOT inline as a single file - it nests, so the
    /// row never implies "1 file" for what was an N-call read.
    #[test]
    fn multi_call_read_with_one_resolved_path_nests() {
        let s = summary(vec![kl("\u{2b1a}", "read", 3, &["src/main.rs"])]);
        let lines = render(&s, ToolCallStatus::Completed, 80);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        // The read row is bare (no inline path); the one file nests below.
        assert!(
            text.iter().any(|t| t.contains("\u{2b1a} read") && !t.contains("src/main.rs")),
            "read row must stay bare (no inline path): {text:?}",
        );
        assert!(
            text.iter().any(|t| t.trim_start().starts_with("\u{2514}\u{2500} src/main.rs")),
            "the resolved path nests as a child row: {text:?}",
        );
    }

    /// 2+ kinds render a tree: a parent count row (no box corner) then
    /// one `├─`/`└─` child per kind in order, the last carrying `└─`.
    /// Read nests one child row per file; no trailing footer line.
    #[test]
    fn multi_kind_renders_tree() {
        let s = summary(vec![
            kl("\u{2b1a}", "read", 2, &["a.rs", "b.rs"]),
            kl("\u{25b6}", "bash", 1, &["cargo check"]),
            kl("\u{2295}", "web", 1, &["docs.rs/tokio"]),
        ]);
        let lines = render(&s, ToolCallStatus::Completed, 80);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        // parent + read parent + 2 read children + bash + web = 6 rows.
        assert_eq!(lines.len(), 6, "parent + read(+2 files) + bash + web; got {text:?}");

        let parent = &text[0];
        assert!(!parent.contains('\u{250c}'), "parent must NOT draw a box corner: {parent:?}");
        assert!(parent.contains("4 tool calls"), "parent count expected: {parent:?}");
        assert!(parent.contains("ctrl+x to expand"));

        // Read is the first kind: `├─ ⬚ read` with no inline target, then
        // each file nested under a `│` spine.
        assert!(text[1].contains("\u{251c}\u{2500} \u{2b1a} read"), "{:?}", text[1]);
        assert!(!text[1].contains("a.rs"), "read files nest, not inline: {:?}", text[1]);
        assert!(text[2].contains("\u{2502}  \u{251c}\u{2500} a.rs"), "{:?}", text[2]);
        assert!(text[3].contains("\u{2502}  \u{2514}\u{2500} b.rs"), "{:?}", text[3]);
        // Bash (middle) and web (last, `└─`).
        assert!(text[4].contains("\u{251c}\u{2500} \u{25b6} bash cargo check"), "{:?}", text[4]);
        assert!(text[5].contains("\u{2514}\u{2500} \u{2295} web"), "{:?}", text[5]);
        assert!(!text.last().unwrap().trim_end().ends_with('\u{2514}'), "no bare `└` footer line");
    }

    /// When read is the LAST kind its file children carry a blank spine
    /// (no `│`) - the tree terminates cleanly.
    #[test]
    fn read_last_kind_children_have_blank_spine() {
        let s = summary(vec![
            kl("\u{25b6}", "bash", 1, &["cargo check"]),
            kl("\u{2b1a}", "read", 2, &["a.rs", "b.rs"]),
        ]);
        let lines = render(&s, ToolCallStatus::Completed, 80);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        // parent + bash + read parent + 2 children.
        assert!(text.iter().any(|t| t.contains("\u{2514}\u{2500} \u{2b1a} read")), "{text:?}");
        // Children indent with 5 leading spaces (blank spine), NOT `│`.
        assert!(text.iter().any(|t| t.starts_with("     \u{251c}\u{2500} a.rs")), "{text:?}");
        assert!(text.iter().any(|t| t.starts_with("     \u{2514}\u{2500} b.rs")), "{text:?}");
        assert!(
            !text.iter().any(|t| t.contains("\u{2502}")),
            "read-last children must not draw a spine: {text:?}",
        );
    }

    /// Read file paths render relative to the project root (the absolute
    /// prefix is stripped) and nest one per row.
    #[test]
    fn read_paths_relativize_against_project_root() {
        let s = summary(vec![
            kl(
                "\u{2b1a}",
                "read",
                2,
                &["/repo/crates/forge-tui/src/ui/message.rs", "/repo/docs/forge-map.html"],
            ),
            kl("\u{25b6}", "bash", 1, &["cargo check"]),
        ]);
        let lines = render_rooted(&s, ToolCallStatus::Completed, 90, "/repo");
        let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("crates/forge-tui/src/ui/message.rs"), "{joined:?}");
        assert!(joined.contains("docs/forge-map.html"), "{joined:?}");
        assert!(!joined.contains("/repo/"), "absolute root prefix must be stripped: {joined:?}");
    }

    /// `relativize` only strips the root on a `/` boundary: a path
    /// strictly under the root goes relative; a sibling sharing the
    /// root's name, the root itself, and an outside path all stay
    /// absolute (no fake `sitory/...` from `/repo` + `/repository`).
    #[test]
    fn relativize_strips_root_only_on_a_slash_boundary() {
        assert_eq!(relativize("/repo/src/main.rs", Some("/repo")), "src/main.rs");
        // trailing-slash root still strips
        assert_eq!(relativize("/repo/src/main.rs", Some("/repo/")), "src/main.rs");
        // sibling sharing the root's NAME prefix (no `/` boundary) stays absolute
        assert_eq!(relativize("/repository/src/main.rs", Some("/repo")), "/repository/src/main.rs");
        assert_eq!(relativize("/repo-old/a.rs", Some("/repo")), "/repo-old/a.rs");
        // outside the root stays absolute
        assert_eq!(relativize("/etc/hosts", Some("/repo")), "/etc/hosts");
        // path == root has no relative remainder -> absolute
        assert_eq!(relativize("/repo", Some("/repo")), "/repo");
        // no root known -> as-is
        assert_eq!(relativize("/repo/src/main.rs", None), "/repo/src/main.rs");
    }

    /// The connectors align: every `├`/`└` sits at the same column
    /// (index 2, after the 2-space indent) and a wrapped kind's spine
    /// `│` lands in that same column so chat reads as one tree.
    #[test]
    fn tree_connectors_and_spine_align() {
        let s = summary(vec![
            kl("\u{2b1a}", "read", 1, &["a.rs"]),
            // A long bash description forces a wrap so the spine row
            // exists to check its column.
            kl("\u{25b6}", "bash", 1, &["run the full workspace gate including fmt and clippy"]),
            kl("\u{2295}", "web", 1, &["docs.rs/tokio"]),
        ]);
        let lines = render(&s, ToolCallStatus::Completed, 48);
        let col = |glyph: char, text: &str| text.chars().position(|c| c == glyph);
        // First kind connector `├` at column 2.
        assert_eq!(col('\u{251c}', &line_text(&lines[1])), Some(2), "├ column");
        // The bash kind is not last, so its wrap continuation carries a
        // `│` spine at the same column 2.
        let spine_row = lines
            .iter()
            .map(line_text)
            .find(|t| t.contains('\u{2502}'))
            .expect("a wrapped non-last kind emits a spine row");
        assert_eq!(col('\u{2502}', &spine_row), Some(2), "│ spine column");
        // Last kind connector `└` at column 2.
        assert_eq!(col('\u{2514}', &line_text(lines.last().unwrap())), Some(2), "└ column");
    }

    /// MCP-server lines carry the distinct `◈` glyph and the server
    /// name as label (the tally builds these; here we assert the render
    /// passes them straight through).
    #[test]
    fn mcp_lines_render_server_glyph_and_name() {
        let s = summary(vec![
            kl("\u{2b1a}", "read", 1, &["x.rs"]),
            kl("\u{25c8}", "context7", 2, &["query-docs", "resolve-library-id"]),
        ]);
        let lines = render(&s, ToolCallStatus::Completed, 90);
        // Read nests its file child, so the MCP line is the last kind
        // row - locate it by its glyph rather than a fixed index.
        let mcp = lines
            .iter()
            .map(line_text)
            .find(|t| t.contains('\u{25c8}'))
            .expect("an MCP-server kind row");
        assert!(mcp.contains("context7"), "server name label expected: {mcp:?}");
        assert!(mcp.contains("query-docs"), "MCP tool sub-name target expected: {mcp:?}");
    }

    /// A lone bash with a long description is a one-child tree whose
    /// `└─ ▶ bash <desc>` child WRAPS (no clip) - the hint rides the
    /// parent count row, continuations align under the target.
    #[test]
    fn single_kind_bash_wraps_long_description() {
        let long = "run the full workspace gate including fmt, the unicode-punct lint, \
                    clippy pedantic, nextest and the doc checks";
        let s = summary(vec![kl("\u{25b6}", "bash", 1, &[long])]);
        let lines = render(&s, ToolCallStatus::Completed, 56);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert!(lines.len() > 2, "parent + wrapped bash child rows: {text:?}");
        assert!(text[0].contains("1 tool call"), "parent count row: {:?}", text[0]);
        assert!(text[0].contains("ctrl+x to expand"), "hint on parent row: {:?}", text[0]);
        assert!(
            text[1].contains("\u{2514}\u{2500} \u{25b6} bash run"),
            "bash child: {:?}",
            text[1]
        );
        assert!(!text.iter().any(|t| t.contains("= ")), "no `=` marker: {text:?}");
        let joined = text.join(" ");
        assert!(joined.contains("doc checks"), "full desc kept (tail): {joined:?}");
        assert!(joined.contains("unicode-punct"), "full desc kept (middle): {joined:?}");
        assert!(!joined.contains("..."), "wrap, not clip: {joined:?}");
        for t in &text {
            assert!(cells(t) <= 56, "row must fit width 56: {t:?}");
        }
    }

    /// A read path wider than the row middle-ellipsis so the filename
    /// stays visible (head + `...` + tail), and the row still fits.
    #[test]
    fn read_path_middle_ellipsis_on_narrow_pane() {
        let s = summary(vec![
            kl("\u{25b6}", "bash", 1, &["cargo check"]),
            kl(
                "\u{2b1a}",
                "read",
                2,
                &[
                    "/repo/crates/forge-tui/src/ui/tool_call/group.rs",
                    "/repo/crates/forge-agent/src/env/processes.rs",
                ],
            ),
        ]);
        let lines = render_rooted(&s, ToolCallStatus::Completed, 46, "/repo");
        let text: Vec<String> = lines.iter().map(line_text).collect();
        let clipped = text.iter().find(|t| t.contains("...")).expect("a path should clip");
        // Middle-ellipsis keeps BOTH a head fragment and the filename
        // tail, not just a surviving tail: `crates/...group.rs`. (The
        // first clipped child is the group.rs path.)
        assert!(clipped.contains("crates/"), "head fragment kept: {clipped:?}");
        assert!(clipped.contains("..."), "middle ellipsis present: {clipped:?}");
        assert!(clipped.contains("group.rs"), "filename tail kept: {clipped:?}");
        for t in &text {
            assert!(cells(t) <= 46, "row must fit width 46: {t:?}");
        }
    }

    /// `clip_middle` drops the MIDDLE (head + `...` + tail) so the head
    /// prefix and the filename both survive; a short string is untouched.
    #[test]
    fn clip_middle_keeps_head_and_tail() {
        let path = "crates/forge-tui/src/ui/tool_call/group.rs";
        let clipped = clip_middle(path, 24);
        assert!(cells(&clipped) <= 24, "fits the budget: {clipped:?}");
        assert!(clipped.starts_with("crates/"), "head kept: {clipped:?}");
        assert!(clipped.contains("..."), "middle dropped: {clipped:?}");
        assert!(clipped.ends_with("group.rs"), "filename tail kept: {clipped:?}");
        assert_eq!(clip_middle("a.rs", 24), "a.rs", "a short path is untouched");
    }

    /// Narrow width keeps every kind visible and every row within the
    /// fence, so the outer layout never re-wraps and shears the tree.
    /// The search kind clips (`...`); description kinds wrap; read nests.
    #[test]
    fn narrow_width_keeps_every_kind_and_fits_width() {
        let s = summary(vec![
            kl("\u{2b1a}", "read", 8, &["very_long_filename_one.rs", "very_long_filename_two.rs"]),
            kl("\u{2315}", "search", 3, &["some_extremely_long_search_pattern_string_here"]),
            kl("\u{25b6}", "bash", 1, &["cargo build --release --all-features"]),
            kl("\u{2295}", "web", 1, &["docs.rs/some/really/long/path/here"]),
        ]);
        let lines = render(&s, ToolCallStatus::Completed, 40);
        for line in &lines {
            let w = cells(&line_text(line));
            assert!(w <= 40, "row must fit width 40; got {w}: {:?}", line_text(line));
        }
        let all = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        for glyph in ['\u{2b1a}', '\u{2315}', '\u{25b6}', '\u{2295}'] {
            assert!(all.contains(glyph), "kind glyph {glyph:?} must stay visible");
        }
        // search is a capped file-list kind: it clips rather than wraps.
        assert!(all.contains("..."), "the capped search target should clip: {all:?}");
    }

    /// A description kind that overflows the row wraps across
    /// continuation rows instead of clipping; the spine is held (`│`)
    /// while a later kind follows and blank on the last kind, and no
    /// continuation exceeds the width.
    #[test]
    fn description_kind_wraps_target_across_rows() {
        let long = "run the full workspace gate including fmt, clippy, nextest and doc checks";
        let s = summary(vec![
            kl("\u{2b1a}", "read", 1, &["config.rs"]),
            kl("\u{25b6}", "bash", 1, &[long]),
        ]);
        let lines = render(&s, ToolCallStatus::Completed, 56);
        // bash is the last kind here, so its wrap continuations carry a
        // blank spine (no `│`) and the full text is present across rows.
        let joined = lines.iter().map(line_text).collect::<Vec<_>>().join(" ");
        assert!(joined.contains("nextest and doc checks"), "wrap keeps the tail: {joined:?}");
        assert!(!joined.contains("..."), "a wrapping kind must not clip: {joined:?}");
        assert!(lines.len() > 3, "the long bash description should wrap to 2+ rows: {lines:?}");
        for line in &lines {
            assert!(cells(&line_text(line)) <= 56, "wrapped row must fit width");
        }
    }

    /// No rendered line ends with a whitespace pad (the #321 regression
    /// guard, extended to the tree). Includes a target-less wrapping
    /// kind (`lsp` count 1) whose row is just glyph + bare label - the
    /// label must not carry a trailing pad/space.
    #[test]
    fn no_line_has_trailing_whitespace_pad() {
        let s = summary(vec![
            kl("\u{2b1a}", "read", 2, &["a.rs", "b.rs"]),
            kl("\u{2699}", "lsp", 1, &[]),
            kl("\u{2295}", "web", 1, &["docs.rs/tokio"]),
        ]);
        for status in [ToolCallStatus::Completed, ToolCallStatus::InProgress] {
            for line in render(&s, status, 80) {
                let text = line_text(&line);
                assert!(!text.ends_with(' '), "trailing pad on {text:?}");
            }
        }
        let single = summary(vec![kl("\u{2b1a}", "read", 1, &["a.rs"])]);
        let text = line_text(&render(&single, ToolCallStatus::Completed, 80)[0]);
        assert!(!text.ends_with(' '), "single-kind trailing pad on {text:?}");
    }

    #[test]
    fn header_status_icon_tracks_aggregate_status() {
        let s = summary(vec![
            kl("\u{2b1a}", "read", 1, &["a.rs"]),
            kl("\u{25b6}", "bash", 1, &["cargo check"]),
        ]);
        let done = line_text(&render(&s, ToolCallStatus::Completed, 80)[0]);
        assert!(done.contains(theme::ICON_COMPLETED), "completed icon: {done:?}");
        let failed = line_text(&render(&s, ToolCallStatus::Failed, 80)[0]);
        assert!(failed.contains(theme::ICON_FAILED), "failed icon: {failed:?}");
        let pending = line_text(&render(&s, ToolCallStatus::Pending, 80)[0]);
        assert!(pending.contains('\u{25cb}'), "pending hollow circle: {pending:?}");
        let running = line_text(&render(&s, ToolCallStatus::InProgress, 80)[0]);
        assert!(
            running.chars().any(|c| ('\u{2800}'..='\u{28FF}').contains(&c)),
            "InProgress braille spinner: {running:?}"
        );
    }

    /// A target-less kind with >1 call shows `×N` on its child row; a
    /// lone target-less call shows just glyph + label.
    #[test]
    fn target_less_kind_shows_multiplier_or_bare_label() {
        let multi = summary(vec![kl("\u{2699}", "lsp", 3, &[])]);
        let lines = render(&multi, ToolCallStatus::Completed, 80);
        let child = line_text(lines.last().unwrap());
        assert!(child.contains("\u{d7}3"), "expected ×3 on the child row: {child:?}");

        let one = summary(vec![kl("\u{25cb}", "tool", 1, &[])]);
        let lines = render(&one, ToolCallStatus::Completed, 80);
        let child = line_text(lines.last().unwrap());
        assert!(child.contains("tool"), "bare label expected: {child:?}");
        assert!(!child.contains('\u{d7}'), "no multiplier for a lone call: {child:?}");
    }
}
