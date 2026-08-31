//! `/account` picker overlay render.
//!
//! A centered modal listing the active session's project accounts,
//! one row each: a current marker `●`, the account name, a budget
//! block whose shape follows the account's `AccountBudget`, and a
//! trailing `usable` / `rate limited` tag. State + key handling live in
//! [`crate::app::account_picker`].

use forge_workspace::{AccountBudget, AccountRow};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::App;
use crate::ui::theme;

const WIDTH: u16 = 62;
const NAME_W: usize = 10;

pub(crate) fn render(frame: &mut Frame, area: Rect, app: &App) {
    let Some(state) = app.account_picker.as_ref() else {
        return;
    };
    let project = app.active_project_name().unwrap_or_else(|| "project".to_owned());

    let inner_w = usize::from(WIDTH.saturating_sub(2));
    let has_experimental = state.rows.iter().any(|row| row.experimental);
    // header + blank + N rows + blank + footer, inside a 1-cell border.
    // The EXPERIMENTAL group adds a blank separator + its own header.
    let group_lines = if has_experimental { 2 } else { 0 };
    let body_lines = state.rows.len().saturating_add(4).saturating_add(group_lines);
    let height = u16::try_from(body_lines).unwrap_or(0).saturating_add(2);
    let overlay = centered(area, WIDTH, height);

    frame.render_widget(Clear, overlay);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::RUST_ORANGE));
    let inner = block.inner(overlay);
    frame.render_widget(block, overlay);

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(body_lines);

    // Header: "Switch account · <project>" left, "N accounts" right.
    let header_left = format!("Switch account \u{00B7} {project}");
    let header_right = format!("{} accounts", state.rows.len());
    lines.push(padded_line(
        vec![Span::styled(
            header_left.clone(),
            Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD),
        )],
        display_len(&header_left),
        Span::styled(header_right.clone(), Style::default().fg(theme::DIM)),
        display_len(&header_right),
        inner_w,
    ));
    lines.push(Line::default());

    // Rows arrive pre-sorted [regular..., experimental...]. Emit a dim
    // EXPERIMENTAL header at the boundary (reusing the launchpad's
    // org-grouping shape). The header is a plain line - it never
    // consumes a highlight index, so arrow-nav still maps straight onto
    // `state.rows`.
    let mut experimental_header_drawn = false;
    for (idx, row) in state.rows.iter().enumerate() {
        if row.experimental && !experimental_header_drawn {
            lines.push(Line::default());
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    "EXPERIMENTAL",
                    Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD),
                ),
            ]));
            experimental_header_drawn = true;
        }
        lines.push(account_row_line(row, idx == state.highlight, inner_w));
    }

    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "\u{2191}\u{2193} move   enter switch   esc cancel   \u{25CF} current",
        Style::default().fg(theme::DIM),
    )));

    frame.render_widget(Paragraph::new(lines), inner);
}

/// Build one account row: marker + name + budget block + a
/// right-aligned status tag. The budget block is 5h/7d percentages plus
/// a reset ETA on capped rows, three spend figures, or a dash per
/// column when no usable snapshot has landed.
fn account_row_line(row: &AccountRow, selected: bool, inner_w: usize) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;

    // Current-account marker (● accent), else two blank columns.
    if row.is_current {
        spans.push(Span::styled("\u{25CF} ", Style::default().fg(theme::RUST_ORANGE)));
    } else {
        spans.push(Span::raw("  "));
    }
    used += 2;

    // Name, padded. The highlighted row is accented + bold.
    let name_style = if selected {
        Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    // `truncate_pad` pads TO the column, so a name that already fills
    // it needs the separator adding or the budget runs straight on.
    let name = format!("{} ", truncate_pad(&row.display_name, NAME_W));
    used += display_len(&name);
    spans.push(Span::styled(name, name_style));

    match &row.budget {
        AccountBudget::Unknown => {
            for (idx, label) in ["5h ", "7d "].into_iter().enumerate() {
                if idx > 0 {
                    spans.push(Span::raw("  "));
                    used += 2;
                }
                spans.push(Span::styled(label.to_owned(), Style::default().fg(theme::DIM)));
                used += 3;
                spans.push(Span::styled("-".to_owned(), Style::default().fg(theme::DIM)));
                used += 1;
            }
        }
        AccountBudget::Subscription { five_hour_util, seven_day_util, resets_at } => {
            for (idx, (label, util)) in
                [("5h ", *five_hour_util), ("7d ", *seven_day_util)].into_iter().enumerate()
            {
                if idx > 0 {
                    spans.push(Span::raw("  "));
                    used += 2;
                }
                spans.push(Span::styled(label.to_owned(), Style::default().fg(theme::DIM)));
                used += 3;
                let pct = format!("{util:.0}%");
                used += display_len(&pct);
                spans.push(Span::styled(
                    pct,
                    Style::default().fg(pct_color(util)).add_modifier(Modifier::BOLD),
                ));
            }

            // Reset ETA - only while the account is at its cap
            // (`resets_at` is populated only then), so the picker shows
            // it on limited rows.
            if let Some(resets_at) = resets_at {
                spans.push(Span::raw("  "));
                used += 2;
                let eta = format!("\u{27F3} {}", format_reset_in(*resets_at));
                used += display_len(&eta);
                spans.push(Span::styled(eta, Style::default().fg(theme::STATUS_WARNING)));
            }
        }
        AccountBudget::Api { daily, weekly, monthly } => {
            // Single-letter periods because the row has 27 columns left
            // once the name and an `experimental · usable` tag are
            // placed, and the words do not fit. `$` asserts USD: the
            // endpoint reports no currency, and OpenRouter credits are
            // dollar-denominated.
            for (idx, (amount, period)) in
                [(*daily, "d"), (*weekly, "w"), (*monthly, "m")].into_iter().enumerate()
            {
                if idx > 0 {
                    spans.push(Span::raw(" "));
                    used += 1;
                }
                let money = format!("${amount:.2}");
                used += display_len(&money);
                spans.push(Span::styled(
                    money,
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(format!(" {period}"), Style::default().fg(theme::DIM)));
                used += 2;
            }
        }
    }

    // Status tag, right-aligned. Experimental rows prefix an amber
    // `experimental` tag + a dim separator so the reason they are
    // grouped is legible even without the section header.
    let (tag, tag_color) =
        if row.usable { ("usable", Color::Green) } else { ("rate limited", theme::STATUS_ERROR) };
    let sep = " \u{00B7} ";
    let tag_block = if row.experimental {
        display_len("experimental") + display_len(sep) + display_len(tag)
    } else {
        display_len(tag)
    };
    let pad = inner_w.saturating_sub(used + tag_block);
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    }
    if row.experimental {
        spans.push(Span::styled("experimental", Style::default().fg(theme::EXPERIMENTAL)));
        spans.push(Span::styled(sep.to_owned(), Style::default().fg(theme::DIM)));
    }
    spans.push(Span::styled(tag.to_owned(), Style::default().fg(tag_color)));

    Line::from(spans)
}

/// Colour a utilization percentage by proximity to the cap: green
/// under ~70, yellow under 100, red at the cap.
fn pct_color(util: f64) -> Color {
    if util >= 100.0 {
        theme::STATUS_ERROR
    } else if util >= 70.0 {
        theme::STATUS_WARNING
    } else {
        Color::Green
    }
}

/// Compact "resets Xh Ym" for the time until `resets_at`. Sub-minute
/// remainders read "resets <1m"; a past instant (defensive) reads the
/// same.
fn format_reset_in(resets_at: std::time::SystemTime) -> String {
    let remaining = resets_at.duration_since(std::time::SystemTime::now()).unwrap_or_default();
    let mins = remaining.as_secs() / 60;
    if mins == 0 {
        return "resets <1m".to_owned();
    }
    let (h, m) = (mins / 60, mins % 60);
    if h > 0 { format!("resets {h}h {m}m") } else { format!("resets {m}m") }
}

fn display_len(s: &str) -> usize {
    s.chars().count()
}

/// Pad or hard-truncate `s` to exactly `width` columns.
fn truncate_pad(s: &str, width: usize) -> String {
    let count = s.chars().count();
    if count >= width {
        s.chars().take(width).collect()
    } else {
        let mut out = s.to_owned();
        out.extend(std::iter::repeat_n(' ', width - count));
        out
    }
}

/// A line with `left` spans on the left and `right` pushed to the far
/// edge of `inner_w`.
fn padded_line(
    mut left: Vec<Span<'static>>,
    left_w: usize,
    right: Span<'static>,
    right_w: usize,
    inner_w: usize,
) -> Line<'static> {
    let pad = inner_w.saturating_sub(left_w + right_w);
    if pad > 0 {
        left.push(Span::raw(" ".repeat(pad)));
    }
    left.push(right);
    Line::from(left)
}

/// Centre a `width` x `height` rect within `area`, clamped to fit.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    fn render_picker(app: &App, w: u16, h: u16) -> Vec<String> {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| render(frame, frame.area(), app)).expect("draw");
        let buffer = terminal.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| {
                        buffer
                            .cell((x, y))
                            .map_or(' ', |c| c.symbol().chars().next().unwrap_or(' '))
                    })
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    /// An API-billed account has no window and, uncapped, no
    /// denominator. A percentage or a reset ETA on this row would be a
    /// number forge invented.
    #[test]
    fn an_api_row_renders_spend_and_never_a_percentage() {
        let mut app = App::test_default();
        let rows = vec![AccountRow {
            display_name: "Router".to_owned(),
            config_dir: PathBuf::from("/c/router"),
            is_current: false,
            usable: true,
            budget: AccountBudget::Api { daily: 0.56, weekly: 1.25, monthly: 20.30 },
            experimental: true,
        }];
        crate::app::account_picker::open(&mut app, rows);

        let lines = render_picker(&app, 80, 16);
        let row = lines.iter().find(|l| l.contains("Router")).expect("api row present");

        assert!(row.contains("$0.56"), "today's spend on the row: {row}");
        assert!(row.contains("$1.25"), "this week's spend on the row: {row}");
        assert!(row.contains("$20.30"), "this month's spend on the row: {row}");
        assert!(!row.contains('%'), "an API account has no percentage to show: {row}");
        assert!(!row.contains("resets"), "an API account has no window to reset: {row}");
    }

    /// "Never probed" and "probed, nothing used" are different answers.
    /// Collapsing them to a green 0% reads as plenty of budget for an
    /// account forge knows nothing about.
    #[test]
    fn an_unprobed_row_is_distinguishable_from_a_probed_zero() {
        let mut app = App::test_default();
        let rows = vec![
            AccountRow {
                display_name: "Unprobed".to_owned(),
                config_dir: PathBuf::from("/c/unprobed"),
                is_current: false,
                usable: true,
                budget: AccountBudget::Unknown,
                experimental: false,
            },
            AccountRow {
                display_name: "Fresh".to_owned(),
                config_dir: PathBuf::from("/c/fresh"),
                is_current: false,
                usable: true,
                budget: AccountBudget::Subscription {
                    five_hour_util: 0.0,
                    seven_day_util: 0.0,
                    resets_at: None,
                },
                experimental: false,
            },
        ];
        crate::app::account_picker::open(&mut app, rows);

        let lines = render_picker(&app, 80, 16);
        let unprobed = lines.iter().find(|l| l.contains("Unprobed")).expect("unprobed row");
        let fresh = lines.iter().find(|l| l.contains("Fresh")).expect("probed-zero row");

        assert!(!unprobed.contains("0%"), "an unprobed account must not claim 0%: {unprobed}");
        assert!(fresh.contains("0%"), "a probed account genuinely at zero says so: {fresh}");
    }

    /// `truncate_pad` pads TO the name column, so a name that already
    /// fills it left no separator and the value ran straight on:
    /// `OpenRouter5h 20%`.
    #[test]
    fn a_full_width_name_keeps_a_separator_before_its_budget() {
        let mut app = App::test_default();
        let rows = vec![AccountRow {
            display_name: "OpenRouter".to_owned(),
            config_dir: PathBuf::from("/c/openrouter"),
            is_current: false,
            usable: true,
            budget: AccountBudget::Subscription {
                five_hour_util: 20.0,
                seven_day_util: 8.0,
                resets_at: None,
            },
            experimental: false,
        }];
        assert_eq!(
            "OpenRouter".chars().count(),
            NAME_W,
            "the fixture has to fill the name column exactly, or it proves nothing",
        );
        crate::app::account_picker::open(&mut app, rows);

        let lines = render_picker(&app, 80, 16);
        let row = lines.iter().find(|l| l.contains("OpenRouter")).expect("row present");
        assert!(
            row.contains("OpenRouter 5h"),
            "a full-width name keeps one space before its budget: {row}",
        );
    }

    #[test]
    fn renders_windows_reset_only_on_capped_and_status_tags() {
        let mut app = App::test_default();
        let future = SystemTime::now() + Duration::from_secs(3600 + 42 * 60);
        let rows = vec![
            AccountRow {
                display_name: "Gateway".to_owned(),
                config_dir: PathBuf::from("/c/gateway"),
                is_current: true,
                usable: false,
                budget: AccountBudget::Subscription {
                    five_hour_util: 100.0,
                    seven_day_util: 63.0,
                    resets_at: Some(future),
                },
                experimental: false,
            },
            AccountRow {
                display_name: "Gateway1".to_owned(),
                config_dir: PathBuf::from("/c/gateway1"),
                is_current: false,
                usable: true,
                budget: AccountBudget::Subscription {
                    five_hour_util: 34.0,
                    seven_day_util: 22.0,
                    resets_at: None,
                },
                experimental: false,
            },
        ];
        crate::app::account_picker::open(&mut app, rows);

        let lines = render_picker(&app, 80, 16);
        let joined = lines.join("\n");

        assert!(joined.contains("Switch account"), "title present: {joined}");
        assert!(joined.contains("2 accounts"), "account count in header");
        assert!(joined.contains('\u{25CF}'), "current account marked with a dot");
        assert!(joined.contains("100%") && joined.contains("63%"), "capped account windows");
        assert!(joined.contains("34%") && joined.contains("22%"), "usable account windows");
        assert!(joined.contains("rate limited"), "capped account tagged rate limited");
        assert!(joined.contains("usable"), "usable account tagged usable");
        assert!(joined.contains("move") && joined.contains("switch"), "footer hint");

        // The reset ETA shows ONLY on the capped row.
        let capped = lines
            .iter()
            .find(|l| l.contains("Gateway") && !l.contains("Gateway1"))
            .expect("capped row present");
        assert!(capped.contains("resets"), "capped account shows a reset ETA: {capped}");
        let usable = lines.iter().find(|l| l.contains("Gateway1")).expect("usable row present");
        assert!(!usable.contains("resets"), "usable account shows no reset ETA: {usable}");
    }

    #[test]
    fn renders_experimental_group_with_amber_tag() {
        let mut app = App::test_default();
        let rows = vec![
            AccountRow {
                display_name: "Gateway".to_owned(),
                config_dir: PathBuf::from("/c/gateway"),
                is_current: true,
                usable: true,
                budget: AccountBudget::Subscription {
                    five_hour_util: 10.0,
                    seven_day_util: 5.0,
                    resets_at: None,
                },
                experimental: false,
            },
            AccountRow {
                display_name: "Codex".to_owned(),
                config_dir: PathBuf::from("/c/codex"),
                is_current: false,
                usable: true,
                budget: AccountBudget::Subscription {
                    five_hour_util: 20.0,
                    seven_day_util: 8.0,
                    resets_at: None,
                },
                experimental: true,
            },
        ];
        crate::app::account_picker::open(&mut app, rows);

        let lines = render_picker(&app, 80, 16);
        let joined = lines.join("\n");

        assert!(joined.contains("EXPERIMENTAL"), "experimental section header present: {joined}");
        let codex = lines.iter().find(|l| l.contains("Codex")).expect("codex row present");
        assert!(codex.contains("experimental"), "experimental row carries the amber tag: {codex}");
        let gateway = lines.iter().find(|l| l.contains("Gateway")).expect("gateway row present");
        assert!(
            !gateway.contains("experimental"),
            "regular row has no experimental tag: {gateway}"
        );

        // The dim header separates the group: it sits after the regular
        // row and before the experimental one.
        let gateway_idx = lines.iter().position(|l| l.contains("Gateway")).expect("gateway idx");
        let header_idx = lines.iter().position(|l| l.contains("EXPERIMENTAL")).expect("header idx");
        let codex_idx = lines.iter().position(|l| l.contains("Codex")).expect("codex idx");
        assert!(gateway_idx < header_idx, "header follows the regular rows");
        assert!(header_idx < codex_idx, "header precedes the experimental rows");
    }
}
