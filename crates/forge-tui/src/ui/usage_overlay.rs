//! `/usage` overlay render: a pinned summary header over a scrollable
//! token/cost table (by project or model), matching the approved mock.

use forge_primitives::token_usage::{UsageReport, UsageRow, WindowUsage};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use crate::app::App;
use crate::app::usage_overlay::{Grouping, Window};
use crate::ui::theme;

/// Lines occupied by the pinned summary + selector + column header.
const HEADER_ROWS: u16 = 10;
/// Lines occupied by the pinned total + key hints.
const FOOTER_ROWS: u16 = 3;
const LABEL_W: usize = 15;
const NUM_W: usize = 11;

pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    frame.render_widget(Clear, area);

    let Some(overlay) = app.usage_overlay.as_mut() else {
        return;
    };
    if overlay.report.is_none() {
        let line = Line::from(Span::styled("  scanning usage…", dim()));
        frame.render_widget(Paragraph::new(line), area);
        return;
    }

    // Clamp scroll against the visible body height before rendering.
    let body_rows = area.height.saturating_sub(HEADER_ROWS + FOOTER_ROWS);
    let rows_len = u16::try_from(overlay.rows().len()).unwrap_or(u16::MAX);
    overlay.scroll = overlay.scroll.min(rows_len.saturating_sub(body_rows));
    let scroll = overlay.scroll;
    let group = overlay.group;
    let window = overlay.window;

    let Some(report) = overlay.report.as_ref() else {
        return;
    };
    let table = window_for(report, window);
    let rule_w = usize::from(area.width).saturating_sub(4).min(88);

    let chunks = Layout::vertical([
        Constraint::Length(HEADER_ROWS),
        Constraint::Min(1),
        Constraint::Length(FOOTER_ROWS),
    ])
    .split(area);

    frame.render_widget(Paragraph::new(header_lines(report, group, window, rule_w)), chunks[0]);
    frame.render_widget(Paragraph::new(body_lines(table, group)).scroll((scroll, 0)), chunks[1]);
    frame.render_widget(Paragraph::new(footer_lines(table, rule_w)), chunks[2]);
}

fn window_for(report: &UsageReport, window: Window) -> &WindowUsage {
    match window {
        Window::Today => &report.today,
        Window::Week => &report.week,
        Window::Month => &report.month,
        Window::Lifetime => &report.lifetime,
    }
}

fn header_lines(
    report: &UsageReport,
    group: Grouping,
    window: Window,
    rule_w: usize,
) -> Vec<Line<'static>> {
    let life = &report.lifetime.total;
    let rule = rule_line(rule_w);

    // Summary headline: lifetime tokens + notional cost + counts.
    let headline = vec![
        span("  LIFETIME  ", bold()),
        span(format!("{} ", fmt_tokens(life.tokens())), bold()),
        span("tokens   ·   ", dim()),
        span(format!("{}≈", fmt_cost_headline(life.cost_usd)), accent_bold()),
        span(
            format!(
                "        {} projects · {} models",
                report.lifetime.by_project.len(),
                report.lifetime.by_model.len(),
            ),
            dim(),
        ),
    ];

    let windows = Line::from(vec![
        span("  this month ", dim()),
        span(format!("{}  ", fmt_tokens(report.month.total.tokens())), Style::default()),
        span(format!("{}     ", fmt_cost(report.month.total.cost_usd)), accent()),
        span("this week ", dim()),
        span(format!("{}  ", fmt_tokens(report.week.total.tokens())), Style::default()),
        span(format!("{}     ", fmt_cost(report.week.total.cost_usd)), accent()),
        span("today ", dim()),
        span(format!("{}  ", fmt_tokens(report.today.total.tokens())), Style::default()),
        span(fmt_cost(report.today.total.cost_usd), accent()),
    ]);

    let cache_pct =
        if life.tokens() == 0 { 0 } else { life.cache_read.saturating_mul(100) / life.tokens() };
    let split = Line::from(vec![
        span("  split   input ", dim()),
        span(format!("{}    ", fmt_tokens(life.input)), Style::default()),
        span("cache-write ", warning()),
        span(format!("{}    ", fmt_tokens(life.cache_write_1h + life.cache_write_5m)), warning()),
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

    let axis = if group == Grouping::Project { "PROJECT" } else { "MODEL" };
    let column_header = Line::from(vec![span(
        format!(
            "  {:<label$}{:>num$}{:>num$}{:>num$}{:>num$}{:>num$}{:>num$}",
            axis,
            "INPUT",
            "CACHE·wr",
            "CACHE·rd",
            "OUTPUT",
            "TOKENS",
            "COST≈",
            label = LABEL_W,
            num = NUM_W,
        ),
        dim(),
    )]);

    vec![
        Line::from(vec![span("  USAGE", accent_bold()), span(" · all accounts", dim())]),
        rule.clone(),
        Line::from(headline),
        windows,
        split,
        rule.clone(),
        selector,
        rule.clone(),
        column_header,
        rule,
    ]
}

fn body_lines(table: &WindowUsage, group: Grouping) -> Vec<Line<'static>> {
    let rows = match group {
        Grouping::Project => &table.by_project,
        Grouping::Model => &table.by_model,
    };
    rows.iter().map(|row| data_row(row, group, false)).collect()
}

fn footer_lines(table: &WindowUsage, rule_w: usize) -> Vec<Line<'static>> {
    vec![
        rule_line(rule_w),
        data_row(&table.total, Grouping::Project, true),
        Line::from(vec![
            span("  ↑↓ ", dim()),
            span("scroll", accent()),
            span("  ·  g ", dim()),
            span("group", accent()),
            span("  ·  w ", dim()),
            span("window", accent()),
            span("  ·  Esc ", dim()),
            span("close", accent()),
        ]),
    ]
}

fn data_row(row: &UsageRow, group: Grouping, is_total: bool) -> Line<'static> {
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
    } else if is_gpt(&row.label) {
        Style::default().fg(theme::EXPERIMENTAL)
    } else {
        accent()
    };

    let mut spans = vec![
        span(format!("  {}", pad_label(&row.label)), label_style),
        span(num(&fmt_tokens(row.input)), dim()),
        span(num(&fmt_tokens(row.cache_write_1h + row.cache_write_5m)), warning()),
        span(num(&fmt_tokens(row.cache_read)), success()),
        span(num(&fmt_tokens(row.output)), dim()),
        span(num(&fmt_tokens(row.tokens())), bold()),
        span(num(&fmt_cost(row.cost_usd)), cost_style),
    ];
    if group == Grouping::Model && !is_total && is_gpt(&row.label) {
        spans.push(span("  (GPT approx)", dim()));
    }
    Line::from(spans)
}

fn is_gpt(label: &str) -> bool {
    label.starts_with("gpt") || label.contains("codex")
}

fn pad_label(label: &str) -> String {
    let truncated: String = if label.chars().count() > LABEL_W - 1 {
        format!("{}…", label.chars().take(LABEL_W - 2).collect::<String>())
    } else {
        label.to_owned()
    };
    format!("{truncated:<LABEL_W$}")
}

fn num(value: &str) -> String {
    format!("{value:>NUM_W$}")
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

    fn text(line: &Line) -> String {
        line.spans.iter().map(|span| span.content.as_ref()).collect()
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
        let line = data_row(&row("opus-4.8", 1_000_000, 500_000, 3.5), Grouping::Model, false);
        let rendered = text(&line);
        assert!(rendered.contains("opus-4.8"), "{rendered}");
        assert!(rendered.contains("1.0M"), "input formatted: {rendered}");
        assert!(rendered.contains("$3.50"), "cost formatted: {rendered}");
    }

    #[test]
    fn gpt_row_is_flagged_approx_in_model_view() {
        let line = data_row(&row("gpt-5-codex", 100, 100, 3.6), Grouping::Model, false);
        assert!(text(&line).contains("(GPT approx)"));
    }

    #[test]
    fn body_lines_emit_one_line_per_row() {
        let table = WindowUsage {
            by_project: vec![row("a", 0, 0, 0.0), row("b", 0, 0, 0.0)],
            by_model: Vec::new(),
            total: row("TOTAL", 0, 0, 0.0),
        };
        assert_eq!(body_lines(&table, Grouping::Project).len(), 2);
    }
}
