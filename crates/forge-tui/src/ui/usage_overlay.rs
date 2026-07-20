//! `/usage` render: a pinned summary header over a scrollable
//! token/cost table (by project or model), on the shared full-screen
//! page scaffold. The table is responsive - labels render in full and
//! the numeric columns spread to the right edge of the body.

use forge_primitives::token_usage::{UsageReport, UsageRow, WindowUsage};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use crate::app::App;
use crate::app::usage_overlay::{Grouping, Window};
use crate::ui::theme;

/// Summary + selector + column-header rows pinned above the table.
const SUMMARY_ROWS: u16 = 9;
/// The rule + TOTAL rows pinned below the table.
const TOTAL_ROWS: u16 = 2;
/// Left indent shared by every row.
const INDENT: usize = 2;
/// Floor for the label column so short names still read as a column.
const MIN_LABEL: usize = 16;
/// Content width of a single numeric column before slack is spread in.
const NUM_W: usize = 11;
/// The six numeric columns: input, cache-wr, cache-rd, output, tokens, cost.
const NUM_COLS: usize = 6;

/// Column widths for the current render: the left label plus the six
/// numeric columns, sized so `INDENT + label + sum(cols)` fills the body.
struct TableLayout {
    label_w: usize,
    fields: [usize; NUM_COLS],
}

pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    super::page::render_page(frame, "Usage", None, footer_hints(), |frame, body| {
        render_report(frame, body, app);
    });
}

fn render_report(frame: &mut Frame, body: Rect, app: &mut App) {
    let content_w = usize::from(body.width).saturating_sub(INDENT * 2);
    let chunks = Layout::vertical([
        Constraint::Length(SUMMARY_ROWS),
        Constraint::Min(1),
        Constraint::Length(TOTAL_ROWS),
    ])
    .split(body);
    let body_rows = chunks[1].height;

    // Mutating phase: bail to the placeholder when there's no report yet,
    // otherwise clamp the scroll and read the values render needs.
    let (scroll, group, window, longest) = {
        let Some(overlay) = app.usage_overlay.as_mut() else {
            return;
        };
        if overlay.report.is_none() {
            render_placeholder(frame, body, overlay.scan_failed);
            return;
        }
        let rows_len = u16::try_from(overlay.rows().len()).unwrap_or(u16::MAX);
        overlay.scroll = overlay.scroll.min(rows_len.saturating_sub(body_rows));
        let longest = overlay.rows().iter().map(|row| row.label.chars().count()).max().unwrap_or(0);
        (overlay.scroll, overlay.group, overlay.window, longest)
    };

    let Some(report) = app.usage_overlay.as_ref().and_then(|overlay| overlay.report.as_ref())
    else {
        return;
    };
    let table = window_for(report, window);
    let layout = layout_for(longest, content_w);

    frame.render_widget(
        Paragraph::new(header_lines(report, group, window, content_w, &layout)),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new(body_lines(table, group, &layout)).scroll((scroll, 0)),
        chunks[1],
    );
    frame.render_widget(Paragraph::new(total_lines(table, content_w, &layout)), chunks[2]);
}

fn render_placeholder(frame: &mut Frame, body: Rect, scan_failed: bool) {
    let (message, style) = if scan_failed {
        ("  usage scan failed · Esc to close · reopen /usage to retry", warning())
    } else {
        ("  scanning usage…", dim())
    };
    frame.render_widget(Paragraph::new(Line::from(span(message, style))), body);
}

fn footer_hints() -> Line<'static> {
    Line::from(vec![
        span("  ↑↓ ", dim()),
        span("scroll", accent()),
        span("  ·  g ", dim()),
        span("group", accent()),
        span("  ·  w ", dim()),
        span("window", accent()),
        span("  ·  Esc ", dim()),
        span("close", accent()),
    ])
}

fn window_for(report: &UsageReport, window: Window) -> &WindowUsage {
    match window {
        Window::Today => &report.today,
        Window::Week => &report.week,
        Window::Month => &report.month,
        Window::Lifetime => &report.lifetime,
    }
}

fn rows_for(table: &WindowUsage, group: Grouping) -> &[UsageRow] {
    match group {
        Grouping::Project => &table.by_project,
        Grouping::Model => &table.by_model,
    }
}

/// Size the label column to the longest current name (never truncating
/// what fits), then spread the numeric columns across the remaining
/// width so the table reaches the body's right edge. When the body is
/// too narrow for label + six columns the label absorbs the shortfall
/// and `pad_label` clips it as a last resort.
fn layout_for(longest: usize, content_w: usize) -> TableLayout {
    let desired = longest.saturating_add(1).max(MIN_LABEL);
    let numbers_min = NUM_COLS * NUM_W;
    let label_w = desired.min(content_w.saturating_sub(numbers_min));
    let slack = content_w.saturating_sub(label_w).saturating_sub(numbers_min);
    let gap = slack / NUM_COLS;
    let extra = slack % NUM_COLS;
    let mut fields = [NUM_W; NUM_COLS];
    for (index, field) in fields.iter_mut().enumerate() {
        *field = NUM_W + gap + usize::from(index < extra);
    }
    TableLayout { label_w, fields }
}

fn header_lines(
    report: &UsageReport,
    group: Grouping,
    window: Window,
    content_w: usize,
    layout: &TableLayout,
) -> Vec<Line<'static>> {
    let life = &report.lifetime.total;

    // Headline: lifetime tokens + notional cost + counts, with the
    // pricing caption right-justified to the body edge. Without a pricing
    // table every cost is a placeholder 0, so the cost blanks to a dash
    // rather than a misleading $0.00.
    let headline_cost = if report.pricing_available {
        span(format!("{}≈", fmt_cost_headline(life.cost_usd)), accent_bold())
    } else {
        span("-", dim())
    };
    let mut headline = vec![
        span("  LIFETIME  ", bold()),
        span(format!("{} ", fmt_tokens(life.tokens())), bold()),
        span("tokens   ·   ", dim()),
        headline_cost,
        span(
            format!(
                "        {} projects · {} models",
                report.lifetime.by_project.len(),
                report.lifetime.by_model.len(),
            ),
            dim(),
        ),
    ];
    let (caption, caption_style) = if report.pricing_available {
        ("notional · at API pricing · not a bill", dim())
    } else {
        ("pricing pending or failed", warning())
    };
    let target_w = INDENT + content_w;
    let used: usize = headline.iter().map(|s| s.content.chars().count()).sum();
    let pad = target_w.saturating_sub(used).saturating_sub(caption.chars().count());
    headline.push(span(" ".repeat(pad), dim()));
    headline.push(span(caption, caption_style));

    // Window totals spread across the body width so they're not cramped.
    let month = window_block("this month", &report.month);
    let week = window_block("this week", &report.week);
    let today = window_block("today", &report.today);
    let blocks_w = spans_width(&month) + spans_width(&week) + spans_width(&today);
    let slack = content_w.saturating_sub(blocks_w);
    let mut windows = vec![span("  ", dim())];
    windows.extend(month);
    windows.push(span(" ".repeat(slack / 2 + slack % 2), dim()));
    windows.extend(week);
    windows.push(span(" ".repeat(slack / 2), dim()));
    windows.extend(today);

    let cache_pct =
        if life.tokens() == 0 { 0 } else { life.cache_read.saturating_mul(100) / life.tokens() };
    let split = Line::from(vec![
        span("  split   input ", dim()),
        span(format!("{}    ", fmt_tokens(life.input)), Style::default()),
        span("cache-write ", warning()),
        span(
            format!("{}    ", fmt_tokens(life.cache_write_1h.saturating_add(life.cache_write_5m))),
            warning(),
        ),
        span("cache-read ", success()),
        span(format!("{} ({cache_pct}%)    ", fmt_tokens(life.cache_read)), success()),
        span("output ", dim()),
        span(fmt_tokens(life.output), Style::default()),
    ]);

    let selector = Line::from(vec![
        span("  group:  ", dim()),
        span("by model", group_style(group == Grouping::Model)),
        span("  ·  ", dim()),
        span("by project", group_style(group == Grouping::Project)),
        span("      window: ", dim()),
        span("today", group_style(window == Window::Today)),
        span(" · ", dim()),
        span("week", group_style(window == Window::Week)),
        span(" · ", dim()),
        span("month", group_style(window == Window::Month)),
        span(" · ", dim()),
        span("lifetime", group_style(window == Window::Lifetime)),
    ]);

    vec![
        Line::from(headline),
        rule_line(content_w),
        Line::from(windows),
        split,
        rule_line(content_w),
        selector,
        rule_line(content_w),
        column_header_line(group, layout),
        rule_line(content_w),
    ]
}

fn window_block(label: &str, window: &WindowUsage) -> Vec<Span<'static>> {
    vec![
        span(format!("{label} "), dim()),
        span(format!("{}  ", fmt_tokens(window.total.tokens())), Style::default()),
        span(fmt_cost(window.total.cost_usd), accent()),
    ]
}

fn spans_width(spans: &[Span<'static>]) -> usize {
    spans.iter().map(|s| s.content.chars().count()).sum()
}

fn column_header_line(group: Grouping, layout: &TableLayout) -> Line<'static> {
    let axis = if group == Grouping::Project { "PROJECT" } else { "MODEL" };
    let headers = ["INPUT", "CACHE·wr", "CACHE·rd", "OUTPUT", "TOKENS", "COST≈"];
    let mut text = format!("  {axis:<label$}", label = layout.label_w);
    for (name, width) in headers.iter().zip(layout.fields.iter()) {
        text.push_str(&num_cell(name, *width));
    }
    Line::from(span(text, dim()))
}

fn body_lines(table: &WindowUsage, group: Grouping, layout: &TableLayout) -> Vec<Line<'static>> {
    rows_for(table, group).iter().map(|row| data_row(row, group, false, layout)).collect()
}

fn total_lines(table: &WindowUsage, content_w: usize, layout: &TableLayout) -> Vec<Line<'static>> {
    vec![rule_line(content_w), data_row(&table.total, Grouping::Project, true, layout)]
}

fn data_row(
    row: &UsageRow,
    group: Grouping,
    is_total: bool,
    layout: &TableLayout,
) -> Line<'static> {
    let dimmed = !is_total && (row.label == "<synthetic>" || row.label == "scratch");
    let label_style = if is_total {
        accent_bold()
    } else if dimmed {
        dim()
    } else {
        bold()
    };
    let cost_style = if is_total {
        accent_bold()
    } else if row.cost_usd <= 0.0 {
        dim()
    } else if group == Grouping::Model && is_gpt(&row.label) {
        Style::default().fg(theme::EXPERIMENTAL)
    } else {
        accent()
    };

    let fields = &layout.fields;
    let mut spans = vec![
        span(format!("  {}", pad_label(&row.label, layout.label_w)), label_style),
        span(num_cell(&fmt_tokens(row.input), fields[0]), dim()),
        span(
            num_cell(&fmt_tokens(row.cache_write_1h.saturating_add(row.cache_write_5m)), fields[1]),
            warning(),
        ),
        span(num_cell(&fmt_tokens(row.cache_read), fields[2]), success()),
        span(num_cell(&fmt_tokens(row.output), fields[3]), dim()),
        span(num_cell(&fmt_tokens(row.tokens()), fields[4]), bold()),
        span(num_cell(&fmt_cost(row.cost_usd), fields[5]), cost_style),
    ];
    if group == Grouping::Model && !is_total && is_gpt(&row.label) {
        spans.push(span("  (GPT approx)", dim()));
    }
    Line::from(spans)
}

fn is_gpt(label: &str) -> bool {
    label.starts_with("gpt") || label.contains("codex")
}

/// Left-justify the label to `width`, clipping with an ellipsis only when
/// the name genuinely doesn't fit (a narrow body).
fn pad_label(label: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if label.chars().count() > width {
        format!("{}…", label.chars().take(width.saturating_sub(1)).collect::<String>())
    } else {
        format!("{label:<width$}")
    }
}

fn num_cell(value: &str, width: usize) -> String {
    format!("{value:>width$}")
}

fn rule_line(width: usize) -> Line<'static> {
    Line::from(span(format!("  {}", "─".repeat(width)), dim()))
}

/// One-decimal human token count via integer math (no float cast).
fn fmt_tokens(n: u64) -> String {
    const B: u64 = 1_000_000_000;
    const M: u64 = 1_000_000;
    const K: u64 = 1_000;
    let (unit, div) = if n >= B {
        ('B', B)
    } else if n >= M {
        ('M', M)
    } else if n >= K {
        ('K', K)
    } else {
        return n.to_string();
    };
    format!("{}.{}{unit}", n / div, (n % div) * 10 / div)
}

/// Table-cell cost: a dash for an unpriced (or synthetic) row.
fn fmt_cost(cost: f64) -> String {
    if cost > 0.0 { format!("${cost:.2}") } else { "-".to_owned() }
}

/// Summary headline cost: always shows the figure.
fn fmt_cost_headline(cost: f64) -> String {
    format!("${cost:.2}")
}

fn span(content: impl Into<String>, style: Style) -> Span<'static> {
    Span::styled(content.into(), style)
}

fn dim() -> Style {
    Style::default().fg(theme::DIM)
}

fn accent() -> Style {
    Style::default().fg(theme::RUST_ORANGE)
}

fn accent_bold() -> Style {
    accent().add_modifier(Modifier::BOLD)
}

fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

fn success() -> Style {
    Style::default().fg(theme::REVIEW_RESOLVED)
}

fn warning() -> Style {
    Style::default().fg(theme::STATUS_WARNING)
}

fn group_style(active: bool) -> Style {
    if active { accent_bold() } else { dim() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::{Buffer, Cell};

    fn text(line: &Line) -> String {
        line.spans.iter().map(|span| span.content.as_ref()).collect()
    }

    fn buffer_text(buffer: &Buffer) -> String {
        let width = usize::from(buffer.area.width);
        buffer
            .content
            .chunks(width)
            .map(|row| row.iter().map(ratatui::buffer::Cell::symbol).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Rightmost non-space column in the row containing `needle`.
    fn rightmost_nonspace_col(buffer: &Buffer, needle: &str) -> Option<usize> {
        let width = usize::from(buffer.area.width);
        buffer.content.chunks(width).find_map(|row| {
            let line: String = row.iter().map(Cell::symbol).collect();
            line.contains(needle).then(|| row.iter().rposition(|c| c.symbol() != " "))?
        })
    }

    fn any_layout() -> TableLayout {
        layout_for(20, 120)
    }

    fn report_with(projects: &[&str], priced: bool) -> UsageReport {
        // An unpriced report mirrors reality: every cost is a placeholder 0.
        let cost = |value: f64| if priced { value } else { 0.0 };
        let window = || WindowUsage {
            by_project: projects.iter().map(|p| row(p, 1_000_000, 500_000, cost(1.5))).collect(),
            by_model: vec![row("opus-4.8", 2_000_000, 1_000_000, cost(3.0))],
            total: row("TOTAL", 3_000_000, 1_500_000, cost(4.5)),
        };
        UsageReport {
            today: window(),
            week: window(),
            month: window(),
            lifetime: window(),
            pricing_available: priced,
        }
    }

    fn app_with(report: Option<UsageReport>) -> App {
        let mut app = App::test_default();
        app.active_view = crate::app::ActiveView::Usage;
        app.usage_overlay = Some(crate::app::UsageOverlayState {
            report,
            group: Grouping::Project,
            window: Window::Lifetime,
            scroll: 0,
            scan_failed: false,
        });
        app
    }

    fn row(label: &str, input: u64, output: u64, cost: f64) -> UsageRow {
        UsageRow {
            label: label.to_owned(),
            input,
            cache_write_1h: 0,
            cache_write_5m: 0,
            cache_read: 0,
            output,
            cost_usd: cost,
        }
    }

    #[test]
    fn fmt_tokens_scales_units() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(950), "950");
        assert_eq!(fmt_tokens(1_500), "1.5K");
        assert_eq!(fmt_tokens(680_500_000), "680.5M");
        assert_eq!(fmt_tokens(27_163_800_000), "27.1B");
    }

    #[test]
    fn fmt_cost_dashes_the_unpriced() {
        assert_eq!(fmt_cost(0.0), "-");
        assert_eq!(fmt_cost(12.5), "$12.50");
    }

    #[test]
    fn data_row_lays_out_label_tokens_and_cost() {
        let line = data_row(
            &row("opus-4.8", 1_000_000, 500_000, 3.5),
            Grouping::Model,
            false,
            &any_layout(),
        );
        let rendered = text(&line);
        assert!(rendered.contains("opus-4.8"), "{rendered}");
        assert!(rendered.contains("1.0M"), "input formatted: {rendered}");
        assert!(rendered.contains("$3.50"), "cost formatted: {rendered}");
    }

    #[test]
    fn gpt_row_is_flagged_approx_in_model_view() {
        let line =
            data_row(&row("gpt-5-codex", 100, 100, 3.6), Grouping::Model, false, &any_layout());
        assert!(text(&line).contains("(GPT approx)"));
    }

    fn cost_color(line: &Line) -> Option<ratatui::style::Color> {
        line.spans.iter().find(|span| span.content.contains('$')).and_then(|span| span.style.fg)
    }

    #[test]
    fn gpt_amber_cost_is_model_view_only() {
        let gpt = row("gpt-5-codex", 100, 100, 3.6);
        assert_eq!(
            cost_color(&data_row(&gpt, Grouping::Model, false, &any_layout())),
            Some(theme::EXPERIMENTAL),
            "GPT model rows are amber",
        );
        // A project that happens to be named like a GPT id must not go
        // amber - the flag is a per-model approximation, not per-project.
        assert_eq!(
            cost_color(&data_row(&gpt, Grouping::Project, false, &any_layout())),
            Some(theme::RUST_ORANGE),
            "project rows keep the normal cost color",
        );
    }

    #[test]
    fn body_lines_emit_one_line_per_row() {
        let table = WindowUsage {
            by_project: vec![row("a", 0, 0, 0.0), row("b", 0, 0, 0.0)],
            by_model: Vec::new(),
            total: row("TOTAL", 0, 0, 0.0),
        };
        assert_eq!(body_lines(&table, Grouping::Project, &any_layout()).len(), 2);
    }

    #[test]
    fn render_shows_summary_and_total() {
        let mut app = app_with(Some(report_with(&["forge", "trader-cc"], true)));
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
        terminal.draw(|frame| render(frame, &mut app)).expect("draw");
        let rendered = buffer_text(terminal.backend().buffer());
        assert!(rendered.contains("LIFETIME"), "{rendered}");
        assert!(rendered.contains("TOTAL"), "{rendered}");
        assert!(rendered.contains("forge"), "{rendered}");
    }

    #[test]
    fn wide_terminal_renders_full_model_id_and_reaches_right_edge() {
        let mut report = report_with(&["forge"], true);
        report.lifetime.by_model = vec![row("claude-sonnet-4-5", 1_000_000, 500_000, 3.0)];
        let mut app = app_with(Some(report));
        if let Some(overlay) = app.usage_overlay.as_mut() {
            overlay.group = Grouping::Model;
        }
        let mut terminal = Terminal::new(TestBackend::new(200, 30)).expect("terminal");
        terminal.draw(|frame| render(frame, &mut app)).expect("draw");
        let buffer = terminal.backend().buffer();
        let rendered = buffer_text(buffer);
        assert!(rendered.contains("claude-sonnet-4-5"), "full id, no ellipsis: {rendered}");
        assert!(!rendered.contains('…'), "no truncation at width 200: {rendered}");
        let right = rightmost_nonspace_col(buffer, "claude-sonnet-4-5").expect("model row");
        assert!(right > 100, "table fills toward the right edge, got column {right}");
    }

    #[test]
    fn narrow_terminal_renders_without_panic() {
        let mut app =
            app_with(Some(report_with(&["a-really-long-project-name-that-overflows"], true)));
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
        terminal.draw(|frame| render(frame, &mut app)).expect("draw");
        let rendered = buffer_text(terminal.backend().buffer());
        assert!(rendered.contains("LIFETIME"), "summary present: {rendered}");
        assert!(rendered.contains("TOTAL"), "total present: {rendered}");
    }

    #[test]
    fn render_shows_scanning_before_the_first_report() {
        let mut app = app_with(None);
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).expect("terminal");
        terminal.draw(|frame| render(frame, &mut app)).expect("draw");
        assert!(buffer_text(terminal.backend().buffer()).contains("scanning"));
    }

    #[test]
    fn render_shows_a_retry_hint_when_the_scan_failed() {
        let mut app = app_with(None);
        if let Some(overlay) = app.usage_overlay.as_mut() {
            overlay.scan_failed = true;
        }
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).expect("terminal");
        terminal.draw(|frame| render(frame, &mut app)).expect("draw");
        assert!(buffer_text(terminal.backend().buffer()).contains("usage scan failed"));
    }

    #[test]
    fn render_survives_a_tiny_area() {
        let mut app = app_with(Some(report_with(&["forge"], true)));
        let mut terminal = Terminal::new(TestBackend::new(30, 5)).expect("terminal");
        // Must not panic when the area is smaller than the pinned chrome.
        terminal.draw(|frame| render(frame, &mut app)).expect("draw");
    }

    #[test]
    fn scroll_clamps_so_the_last_project_stays_reachable() {
        let projects: Vec<String> = (0..20).map(|i| format!("p{i:02}")).collect();
        let refs: Vec<&str> = projects.iter().map(String::as_str).collect();
        let mut app = app_with(Some(report_with(&refs, true)));
        if let Some(overlay) = app.usage_overlay.as_mut() {
            overlay.scroll = 100; // far past the end
        }
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).expect("terminal");
        terminal.draw(|frame| render(frame, &mut app)).expect("draw");
        // inner 18, body 17, table = 17 - SUMMARY_ROWS(9) - TOTAL_ROWS(2) = 6;
        // max scroll = 20 - 6 = 14.
        let scroll = app.usage_overlay.as_ref().expect("overlay").scroll;
        assert_eq!(scroll, 14, "scroll clamps to the last page, never past it");
        assert!(
            buffer_text(terminal.backend().buffer()).contains("p19"),
            "the last project is reachable",
        );
    }

    #[test]
    fn banner_shows_the_notional_caption_when_priced() {
        let mut app = app_with(Some(report_with(&["forge"], true)));
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");
        terminal.draw(|frame| render(frame, &mut app)).expect("draw");
        let rendered = buffer_text(terminal.backend().buffer());
        assert!(rendered.contains("notional · at API pricing · not a bill"), "{rendered}");
    }

    #[test]
    fn missing_pricing_blanks_costs_instead_of_reading_zero() {
        let mut app = app_with(Some(report_with(&["forge"], false)));
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");
        terminal.draw(|frame| render(frame, &mut app)).expect("draw");
        let rendered = buffer_text(terminal.backend().buffer());
        assert!(rendered.contains("pricing pending or failed"), "caption signals it: {rendered}");
        assert!(!rendered.contains("$0.00"), "no misleading $0.00 anywhere: {rendered}");
    }
}
