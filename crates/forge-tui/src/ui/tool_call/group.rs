//! Render the L2 summary for a grouped run of consecutive
//! collapsed-by-default tool calls. EVERY run - single-kind or many -
//! renders one consistent tree: a parent count row with one `├─`/`└─`
//! child per kind, matching the projects-pane connectors + `│` spine so
//! chat and the side panes read as one tree system.
//!
//! Every kind follows one rule: a genuine single call (one call, one
//! target) rides the kind row inline; a kind with more calls or targets
//! nests one clipped child row per target (uncapped); a target-less
//! kind shows a bare row with `×N`. Nothing wraps - each row is a single
//! line clipped to fit. Read relativizes its paths against the project
//! root and clips with a middle-ellipsis (keeps the filename); every
//! other kind clips with an end-ellipsis (keeps the head - the command
//! name / domain / pattern start).
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
use crate::ui::message::grouping::{KindSummary, READ_GLYPH};
use crate::ui::theme;
use crate::ui::tool_call::status_icon;

/// Trailing affordance on the parent count row.
const EXPAND_HINT: &str = "   ctrl+x to expand";

/// The only things that differ between a tool group's summary tree and a
/// messaging group's. Everything structural - indent, connectors, spine,
/// status icon, per-row budgeting and clipping - is shared.
pub struct SummaryChrome {
    /// Singular and plural nouns for the parent count row.
    pub noun: (&'static str, &'static str),
    /// NOT a shared constant - the tool hint stays understated by
    /// decision, so sharing one would silently reverse it.
    pub expand_hint: &'static str,
    /// Tool groups inline a kind holding one call with one target.
    /// Messaging always nests, so peer names share a column.
    pub allow_inline: bool,
}

impl SummaryChrome {
    pub const TOOL: Self =
        Self { noun: ("tool call", "tool calls"), expand_hint: EXPAND_HINT, allow_inline: true };
    pub const MESSAGING: Self = Self {
        noun: ("message", "messages"),
        expand_hint: "   click or ctrl+x to expand",
        allow_inline: false,
    };
}

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
    chrome: &SummaryChrome,
) -> Vec<Line<'static>> {
    let (icon_glyph, icon_color) = status_icon(aggregate_status, spinner_glyph);
    let icon_style = Style::default().fg(icon_color).add_modifier(Modifier::BOLD);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(theme::DIM);

    let mark = Style::default().fg(theme::DIM);
    let total = summary.total();
    let mut lines = vec![Line::from(vec![
        Span::raw("  ".to_owned()),
        Span::styled(format!("{icon_glyph} "), icon_style),
        Span::styled(
            format!("{total} {}", if total == 1 { chrome.noun.0 } else { chrome.noun.1 }),
            bold,
        ),
        Span::styled(chrome.expand_hint.to_owned(), dim),
    ])];

    let n = summary.lines.len();
    let label_w = summary.lines.iter().map(|l| cells(&l.label)).max().unwrap_or(0);
    // Every row below is clipped to fit `max_width` so the outer message
    // layout (which char-wraps WITHOUT the tree gutter) never re-wraps a
    // row and shears the tree - see the module doc / message.rs gotcha.
    for (i, line) in summary.lines.iter().enumerate() {
        let last = i + 1 == n;
        let connector = chat_tree::connector(last);
        // The spine under a kind holds `│` while a later kind follows,
        // blank on the last kind (matches the projects-pane tree).
        let spine = if last { " " } else { chat_tree::SPINE };

        // Read relativizes each path against the project root; every
        // other kind takes its target verbatim.
        let is_read = line.glyph == READ_GLYPH;
        let kind_style = if line.warn { bold.fg(theme::STATUS_WARNING) } else { bold };
        let targets: Vec<String> = if is_read {
            line.targets.iter().map(|t| relativize(t, project_root)).collect()
        } else {
            line.targets.clone()
        };

        // Target-less kind: the bare kind row, `×N` when called more
        // than once. No child rows.
        if targets.is_empty() {
            let mult = multiplier(line.count);
            let mut row = vec![
                Span::raw("  ".to_owned()),
                Span::styled(connector.to_owned(), mark),
                Span::styled(format!("{} ", line.glyph), kind_style),
            ];
            if mult.is_empty() {
                // Bare label carries no trailing pad/space.
                row.push(Span::styled(line.label.clone(), kind_style));
            } else {
                row.push(Span::styled(format!("{} ", pad_right(&line.label, label_w)), kind_style));
                row.push(Span::styled(mult, dim));
            }
            lines.push(Line::from(row));
            continue;
        }

        // A genuine single (one call, one target) rides the kind row
        // inline. Guard on BOTH count and target count: an N-call kind
        // that only resolved one target still nests, so the row never
        // implies "1 call".
        if chrome.allow_inline && line.count == 1 && targets.len() == 1 {
            let prefix_cells = 2 + 3 + cells(line.glyph) + 1 + label_w + 1;
            let budget = max_width.saturating_sub(prefix_cells).max(MIN_TARGET_BUDGET);
            lines.push(Line::from(vec![
                Span::raw("  ".to_owned()),
                Span::styled(connector.to_owned(), mark),
                Span::styled(format!("{} ", line.glyph), bold),
                Span::styled(format!("{} ", pad_right(&line.label, label_w)), bold),
                Span::styled(clip_target(&targets[0], budget, is_read), dim),
            ]));
            continue;
        }

        // Otherwise nest: the bare kind row, then one clipped child row
        // per target (uncapped) one level deeper under the spine.
        let kind_style = if line.warn { bold.fg(theme::STATUS_WARNING) } else { bold };
        lines.push(Line::from(vec![
            Span::raw("  ".to_owned()),
            Span::styled(connector.to_owned(), mark),
            Span::styled(format!("{} ", line.glyph), kind_style),
            Span::styled(line.label.clone(), kind_style),
        ]));
        // Children nest one level deeper: base(2) + spine(1) + gap(2) +
        // child connector(3) = target column 8.
        let child_prefix = 2 + 1 + 2 + 3;
        let budget = max_width.saturating_sub(child_prefix).max(MIN_TARGET_BUDGET);
        let child_n = targets.len();
        for (ci, target) in targets.iter().enumerate() {
            let child_last = ci + 1 == child_n;
            let child_conn = chat_tree::connector(child_last);
            lines.push(Line::from(vec![
                Span::raw("  ".to_owned()),
                Span::styled(spine.to_owned(), mark),
                Span::raw("  ".to_owned()),
                Span::styled(child_conn.to_owned(), mark),
                Span::styled(clip_target(target, budget, is_read), dim),
            ]));
        }
    }
    lines
}

/// Clip a target to `budget` display cells: read keeps its filename via
/// a middle-ellipsis, every other kind keeps its head via an
/// end-ellipsis.
fn clip_target(s: &str, budget: usize, is_read: bool) -> String {
    if is_read { clip_middle(s, budget) } else { clip_to_width(s, budget) }
}

/// The `×N` multiplier for a target-less kind called more than once;
/// empty for a lone call.
fn multiplier(count: usize) -> String {
    if count > 1 { format!("\u{d7}{count}") } else { String::new() }
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

fn cells(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

fn pad_right(s: &str, target_cells: usize) -> String {
    let w = cells(s);
    if w >= target_cells { s.to_owned() } else { format!("{s}{}", " ".repeat(target_cells - w)) }
}

/// Clip `s` to `budget` display cells, appending `...` when over so the
/// result still fits.
pub(crate) fn clip_to_width(s: &str, budget: usize) -> String {
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
            warn: false,
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
        render_group_summary_line(s, status, '\u{280B}', width, None, &SummaryChrome::TOOL)
    }

    fn render_rooted(
        s: &KindSummary,
        status: ToolCallStatus,
        width: usize,
        root: &str,
    ) -> Vec<Line<'static>> {
        render_group_summary_line(s, status, '\u{280B}', width, Some(root), &SummaryChrome::TOOL)
    }

    /// A single-kind run nests one child row per instance. A search with
    /// 3 calls but 2 resolved patterns renders the parent count row + a
    /// bare `└─ ⌕ search` row + one child per pattern - no comma-join, no
    /// `+1` overflow, no `=` / box anywhere.
    #[test]
    fn single_kind_renders_one_child_tree() {
        let s = summary(vec![kl("\u{2315}", "search", 3, &["FooBar", "Baz"])]);
        let lines = render(&s, ToolCallStatus::Completed, 80);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(lines.len(), 4, "parent + bare search row + 2 children; got {text:?}");
        assert!(text[0].contains("3 tool calls"), "parent count row: {text:?}");
        assert!(text[0].contains("ctrl+x to expand"));
        assert!(
            text[1].contains("\u{2514}\u{2500} \u{2315} search"),
            "bare kind row: {:?}",
            text[1]
        );
        assert!(!text[1].contains("FooBar"), "patterns nest, not inline: {:?}", text[1]);
        assert!(text[2].trim_start().starts_with("\u{251c}\u{2500} FooBar"), "{:?}", text[2]);
        assert!(text[3].trim_start().starts_with("\u{2514}\u{2500} Baz"), "{:?}", text[3]);
        assert!(!text.iter().any(|t| t.contains("+1")), "no overflow suffix: {text:?}");
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

    /// A lone read (count 1, one file) whose path is wider than the row
    /// rides the read row inline AND middle-ellipsis-clips, so the
    /// filename stays visible. Direct coverage for the inline-read clip
    /// branch: the nest tests exercise the child-row clip, and the
    /// short-path inline test never fires the clip.
    #[test]
    fn single_read_long_path_inlines_middle_ellipsis() {
        let s = summary(vec![kl(
            "\u{2b1a}",
            "read",
            1,
            &["/repo/crates/forge-tui/src/ui/tool_call/group.rs"],
        )]);
        let lines = render_rooted(&s, ToolCallStatus::Completed, 40, "/repo");
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(lines.len(), 2, "parent + one inline read row, no child: {text:?}");
        assert!(text[1].contains("\u{2514}\u{2500} \u{2b1a} read "), "read inlines: {:?}", text[1]);
        // Middle-ellipsis keeps a head fragment AND the filename tail.
        assert!(text[1].contains("crates/"), "head fragment kept: {:?}", text[1]);
        assert!(text[1].contains("..."), "middle ellipsis present: {:?}", text[1]);
        assert!(text[1].contains("group.rs"), "filename tail kept: {:?}", text[1]);
        for t in &text {
            assert!(cells(t) <= 40, "row must fit width 40: {t:?}");
        }
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
    /// (index 2, after the 2-space indent) and a nested non-last kind's
    /// spine `│` lands in that same column so chat reads as one tree.
    #[test]
    fn tree_connectors_and_spine_align() {
        let s = summary(vec![
            kl("\u{2b1a}", "read", 1, &["a.rs"]),
            // A multi-instance bash (not the last kind) nests children
            // under a `│` spine so the spine row exists to check its
            // column.
            kl("\u{25b6}", "bash", 3, &["cargo check", "cargo build", "cargo test"]),
            kl("\u{2295}", "web", 1, &["docs.rs/tokio"]),
        ]);
        let lines = render(&s, ToolCallStatus::Completed, 48);
        let col = |glyph: char, text: &str| text.chars().position(|c| c == glyph);
        // First kind connector `├` at column 2 (the inline read row).
        assert_eq!(col('\u{251c}', &line_text(&lines[1])), Some(2), "├ column");
        // Bash is not the last kind, so its nested children carry a `│`
        // spine at the same column 2.
        let spine_row = lines
            .iter()
            .map(line_text)
            .find(|t| t.contains('\u{2502}'))
            .expect("a nested non-last kind emits a spine row");
        assert_eq!(col('\u{2502}', &spine_row), Some(2), "│ spine column");
        // Last kind connector `└` at column 2 (the inline web row).
        assert_eq!(col('\u{2514}', &line_text(lines.last().unwrap())), Some(2), "└ column");
    }

    /// MCP-server lines carry the distinct `◈` glyph and the server
    /// name as label. A server with 2 tool calls nests: the server name
    /// rides the parent row, each tool sub-name on its own child row (no
    /// comma-join).
    #[test]
    fn mcp_lines_render_server_glyph_and_name() {
        let s = summary(vec![
            kl("\u{2b1a}", "read", 1, &["x.rs"]),
            kl("\u{25c8}", "context7", 2, &["query-docs", "resolve-library-id"]),
        ]);
        let lines = render(&s, ToolCallStatus::Completed, 90);
        let mcp_parent = lines
            .iter()
            .map(line_text)
            .find(|t| t.contains('\u{25c8}'))
            .expect("an MCP-server kind row");
        assert!(mcp_parent.contains("context7"), "server name label expected: {mcp_parent:?}");
        assert!(!mcp_parent.contains("query-docs"), "tool names nest, not inline: {mcp_parent:?}");
        let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("query-docs"), "tool sub-name child expected: {joined:?}");
        assert!(joined.contains("resolve-library-id"), "tool sub-name child expected: {joined:?}");
        assert!(!joined.contains("query-docs, resolve-library-id"), "no comma-join: {joined:?}");
    }

    /// A lone bash (count 1, one target) rides the kind row INLINE and
    /// CLIPS with an end-ellipsis - the head (command name) is kept, the
    /// tail dropped, and the row stays a single line that fits the width.
    /// This is the deliberate flip from the old wrap-to-multiple-rows.
    #[test]
    fn single_kind_bash_inlines_clipped() {
        let long = "run the full workspace gate including fmt, the unicode-punct lint, \
                    clippy pedantic, nextest and the doc checks";
        let s = summary(vec![kl("\u{25b6}", "bash", 1, &[long])]);
        let lines = render(&s, ToolCallStatus::Completed, 56);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(lines.len(), 2, "parent + one inline bash row, no wrap: {text:?}");
        assert!(text[0].contains("1 tool call"), "parent count row: {:?}", text[0]);
        assert!(text[0].contains("ctrl+x to expand"), "hint on parent row: {:?}", text[0]);
        assert!(
            text[1].contains("\u{2514}\u{2500} \u{25b6} bash run"),
            "inline head: {:?}",
            text[1]
        );
        assert!(text[1].contains("..."), "end-ellipsis clip: {:?}", text[1]);
        assert!(!text[1].contains("doc checks"), "tail dropped by the clip: {:?}", text[1]);
        assert!(!text.iter().any(|t| t.contains("= ")), "no `=` marker: {text:?}");
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
    /// Every kind nests + clips per instance; read middle-ellipsis, other
    /// kinds end-ellipsis - both produce `...`.
    #[test]
    fn narrow_width_keeps_every_kind_and_fits_width() {
        let s = summary(vec![
            kl("\u{2b1a}", "read", 8, &["very_long_filename_one.rs", "very_long_filename_two.rs"]),
            kl("\u{2315}", "search", 3, &["some_extremely_long_search_pattern_string_here"]),
            kl(
                "\u{25b6}",
                "bash",
                2,
                &["cargo build --release --all-features", "cargo nextest run"],
            ),
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
        // search now nests its lone long pattern on a child row and clips.
        let search_parent =
            all.lines().find(|t| t.contains('\u{2315}')).expect("a search kind row");
        assert!(
            !search_parent.contains("some_extremely"),
            "search nests: pattern not inline on the kind row: {search_parent:?}",
        );
        assert!(all.contains("..."), "long nested targets clip: {all:?}");
    }

    /// A multi-instance bash (count 4, 4 commands) nests one clipped
    /// child row per command - none hidden, none comma-joined, none
    /// wrapped. A long command clips with an end-ellipsis; every row
    /// fits the width.
    #[test]
    fn multi_bash_nests_one_child_per_command() {
        let long =
            "cargo build --release --all-features --workspace with a long trailing tail here";
        let s = summary(vec![kl(
            "\u{25b6}",
            "bash",
            4,
            &[long, "npm run test-suite", "git status --short", "grep -rn needle"],
        )]);
        let lines = render(&s, ToolCallStatus::Completed, 50);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        // parent count + bare bash row + 4 children = 6 rows, no wrap.
        assert_eq!(lines.len(), 6, "parent + bash row + 4 children: {text:?}");
        assert!(text[1].contains("\u{2514}\u{2500} \u{25b6} bash"), "bare bash row: {:?}", text[1]);
        assert!(!text[1].contains("cargo build"), "commands nest, not inline: {:?}", text[1]);
        let joined = text.join("\n");
        assert!(joined.contains("cargo build"), "long command head kept: {joined:?}");
        assert!(joined.contains("npm run test-suite"), "command kept: {joined:?}");
        assert!(joined.contains("git status --short"), "command kept: {joined:?}");
        assert!(joined.contains("grep -rn needle"), "command kept: {joined:?}");
        let clipped = text.iter().find(|t| t.contains("cargo build")).unwrap();
        assert!(clipped.contains("..."), "long command clips end-ellipsis: {clipped:?}");
        assert!(!clipped.contains("trailing tail"), "clip drops the tail: {clipped:?}");
        for t in &text {
            assert!(cells(t) <= 50, "row must fit width 50: {t:?}");
        }
    }

    /// No rendered line ends with a whitespace pad (the trailing-pad
    /// regression guard, extended to the tree). Includes a target-less
    /// kind (`lsp` count 1) whose row is just glyph + bare label, and a
    /// nested non-read kind (`bash` count 3) whose bare parent row must not
    /// carry a trailing pad/space either.
    #[test]
    fn no_line_has_trailing_whitespace_pad() {
        let s = summary(vec![
            kl("\u{2b1a}", "read", 2, &["a.rs", "b.rs"]),
            kl("\u{25b6}", "bash", 3, &["cargo check", "cargo build", "cargo test"]),
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

    /// A lone web call (count 1, one target) rides the kind row inline
    /// and clips end-ellipsis when the URL is wider than the row - the
    /// head (domain) stays visible, no child row.
    #[test]
    fn lone_web_inlines_clipped() {
        let long = "docs.rs/tokio/latest/tokio/runtime/struct.Runtime.html#method.block_on";
        let s = summary(vec![kl("\u{2295}", "web", 1, &[long])]);
        let lines = render(&s, ToolCallStatus::Completed, 44);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(lines.len(), 2, "parent + one inline web row: {text:?}");
        assert!(
            text[1].contains("\u{2514}\u{2500} \u{2295} web docs.rs"),
            "inline head: {:?}",
            text[1]
        );
        assert!(text[1].contains("..."), "end-ellipsis clip: {:?}", text[1]);
        assert!(!text[1].contains("block_on"), "tail dropped: {:?}", text[1]);
        for t in &text {
            assert!(cells(t) <= 44, "row must fit width 44: {t:?}");
        }
    }

    /// A lone MCP call (count 1, one tool sub-name) rides the server row
    /// inline - no child row.
    #[test]
    fn lone_mcp_inlines() {
        let s = summary(vec![kl("\u{25c8}", "context7", 1, &["query-docs"])]);
        let lines = render(&s, ToolCallStatus::Completed, 80);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(lines.len(), 2, "parent + one inline mcp row: {text:?}");
        assert!(
            text[1].contains("\u{2514}\u{2500} \u{25c8} context7 query-docs"),
            "inline server + tool: {:?}",
            text[1],
        );
    }

    /// Multi-instance web and MCP kinds each nest one clipped child per
    /// instance (no comma-join). The non-last kind's children carry the
    /// `│` spine; the last kind's carry a blank spine.
    #[test]
    fn multi_web_and_mcp_nest_one_child_per_instance() {
        let s = summary(vec![
            kl("\u{2295}", "web", 2, &["docs.rs/tokio", "example.com/foo"]),
            kl("\u{25c8}", "context7", 3, &["query-docs", "resolve-library-id", "get-docs"]),
        ]);
        let lines = render(&s, ToolCallStatus::Completed, 80);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        // parent + web row + 2 web children + context7 row + 3 children.
        assert_eq!(lines.len(), 8, "web(+2) + context7(+3): {text:?}");
        assert!(text[1].contains("\u{251c}\u{2500} \u{2295} web"), "bare web row: {:?}", text[1]);
        assert!(text[2].contains("\u{2502}  \u{251c}\u{2500} docs.rs/tokio"), "{:?}", text[2]);
        assert!(text[3].contains("\u{2502}  \u{2514}\u{2500} example.com/foo"), "{:?}", text[3]);
        assert!(
            text[4].contains("\u{2514}\u{2500} \u{25c8} context7"),
            "bare mcp row: {:?}",
            text[4]
        );
        assert!(text[5].starts_with("     \u{251c}\u{2500} query-docs"), "{:?}", text[5]);
        assert!(text[7].starts_with("     \u{2514}\u{2500} get-docs"), "{:?}", text[7]);
        let joined = text.join("\n");
        assert!(!joined.contains("docs.rs/tokio, "), "no comma-join: {joined:?}");
        assert!(!joined.contains("query-docs, "), "no comma-join: {joined:?}");
        for t in &text {
            assert!(cells(t) <= 80, "row must fit width 80: {t:?}");
        }
    }
}
