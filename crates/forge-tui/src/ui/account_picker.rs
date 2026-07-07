//! `/account` picker overlay render.
//!
//! A centered modal listing the active session's project accounts,
//! one row each: a current marker `●`, the account name, the 5h + 7d
//! window utilization (coloured by proximity to the cap), a reset ETA
//! shown only while the account is at its cap, and a trailing
//! `usable` / `rate limited` tag. State + key handling live in
//! [`crate::app::account_picker`].

use forge_workspace::AccountRow;
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
    // header + blank + N rows + blank + footer, inside a 1-cell border.
    let body_lines = state.rows.len().saturating_add(4);
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

    for (idx, row) in state.rows.iter().enumerate() {
        lines.push(account_row_line(row, idx == state.highlight, inner_w));
    }

    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "\u{2191}\u{2193} move   enter switch   esc cancel   \u{25CF} current",
        Style::default().fg(theme::DIM),
    )));

    frame.render_widget(Paragraph::new(lines), inner);
}

/// Build one account row: marker + name + 5h/7d windows + reset ETA
/// (capped rows only) + a right-aligned status tag.
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
    let name = truncate_pad(&row.display_name, NAME_W);
    used += display_len(&name);
    spans.push(Span::styled(name, name_style));

    // 5h window.
    spans.push(Span::styled("5h ".to_owned(), Style::default().fg(theme::DIM)));
    used += 3;
    let five = format!("{:.0}%", row.five_hour_util);
    used += display_len(&five);
    spans.push(Span::styled(
        five,
        Style::default().fg(pct_color(row.five_hour_util)).add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::raw("  "));
    used += 2;

    // 7d window.
    spans.push(Span::styled("7d ".to_owned(), Style::default().fg(theme::DIM)));
    used += 3;
    let seven = format!("{:.0}%", row.seven_day_util);
    used += display_len(&seven);
    spans.push(Span::styled(
        seven,
        Style::default().fg(pct_color(row.seven_day_util)).add_modifier(Modifier::BOLD),
    ));

    // Reset ETA - only while the account is at its cap (resets_at is
    // populated only then), so the picker shows it on limited rows.
    if let Some(resets_at) = row.resets_at {
        spans.push(Span::raw("  "));
        used += 2;
        let eta = format!("\u{27F3} {}", format_reset_in(resets_at));
        used += display_len(&eta);
        spans.push(Span::styled(eta, Style::default().fg(theme::STATUS_WARNING)));
    }

    // Status tag, right-aligned.
    let (tag, tag_color) =
        if row.usable { ("usable", Color::Green) } else { ("rate limited", theme::STATUS_ERROR) };
    let pad = inner_w.saturating_sub(used + display_len(tag));
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
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

    #[test]
    fn renders_windows_reset_only_on_capped_and_status_tags() {
        let mut app = App::test_default();
        let future = SystemTime::now() + Duration::from_secs(3600 + 42 * 60);
        let rows = vec![
            AccountRow {
                display_name: "Granite".to_owned(),
                config_dir: PathBuf::from("/c/granite"),
                is_current: true,
                usable: false,
                five_hour_util: 100.0,
                seven_day_util: 63.0,
                resets_at: Some(future),
            },
            AccountRow {
                display_name: "Granite1".to_owned(),
                config_dir: PathBuf::from("/c/granite1"),
                is_current: false,
                usable: true,
                five_hour_util: 34.0,
                seven_day_util: 22.0,
                resets_at: None,
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
            .find(|l| l.contains("Granite") && !l.contains("Granite1"))
            .expect("capped row present");
        assert!(capped.contains("resets"), "capped account shows a reset ETA: {capped}");
        let usable = lines.iter().find(|l| l.contains("Granite1")).expect("usable row present");
        assert!(!usable.contains("resets"), "usable account shows no reset ETA: {usable}");
    }
}
