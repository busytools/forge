//! `/account` picker overlay render.
//!
//! A centered modal listing the active session's project accounts,
//! one row each: a current marker `●`, the account name, a budget
//! block whose shape follows the account's `AccountBudget`, and a
//! trailing status tag (`usable`, `limit hit`, or
//! `auth failed or expired`). State + key handling live in
//! [`crate::app::account_picker`].

use forge_workspace::{AccountBudget, AccountRow, Unusable};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::App;
use crate::ui::theme;

const WIDTH: u16 = 62;
const NAME_W: usize = 10;
/// Columns kept between a row's budget block and its status tag,
/// reserved out of the name and budget widths rather than floored on
/// the padding - a floor can only overflow a row that is already full.
/// The paragraph does not wrap, so an overrun is cut with no ellipsis.
const TAG_GAP: usize = 1;

pub(crate) fn render(frame: &mut Frame, area: Rect, app: &App) {
    let Some(state) = app.account_picker.as_ref() else {
        return;
    };
    let project = app.active_project_name().unwrap_or_else(|| "project".to_owned());

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

    // The width rows are built for has to be the width they are painted
    // into. `centered` clamps the overlay to the terminal, so on a split
    // pane the constant is wider than the rect and every row's tag is
    // cut off the end.
    let inner_w = usize::from(inner.width);

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

    // Status tag, right-aligned. Experimental rows prefix an amber
    // `experimental` tag + a dim separator so the reason they are
    // grouped is legible even without the section header. Measured
    // before the name, because on a narrow row the name is what gives
    // way to keep it.
    let (tag, tag_color) = match row.unusable {
        None => ("usable", Color::Green),
        Some(Unusable::Saturated) => ("limit hit", theme::STATUS_ERROR),
        Some(Unusable::ProbeBlocked | Unusable::Bailed) => {
            ("auth failed or expired", theme::STATUS_ERROR)
        }
    };
    let sep = " \u{00B7} ";
    let tag_w = display_len(tag);
    // The amber prefix is the first thing to give way on a narrow row:
    // the EXPERIMENTAL header above the group already says why these
    // rows sit apart, and the widest tag would otherwise push the row
    // past the pane.
    let show_prefix = row.experimental
        && used + display_len("experimental") + display_len(sep) + tag_w + TAG_GAP <= inner_w;
    let tag_block =
        if show_prefix { display_len("experimental") + display_len(sep) + tag_w } else { tag_w };

    // Name, padded, then a separator column - `truncate_pad` pads TO
    // its width, so a name that already fills it would otherwise run
    // straight into the budget. The column shrinks on a row too narrow
    // to hold it beside the tag, rather than pushing the tag off.
    let name_style = if selected {
        Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    let name_w = NAME_W.min(inner_w.saturating_sub(used + tag_block + TAG_GAP));
    let name = format!("{} ", truncate_pad(&row.display_name, name_w));
    used += display_len(&name);
    spans.push(Span::styled(name, name_style));

    // Both the tag (`auth failed or expired` is sixteen wider than
    // `usable`) and the figures vary with the data, so the block is
    // sized against what is left rather than a fixed count.
    let budget_room = inner_w.saturating_sub(used + tag_block + TAG_GAP);
    let budget = budget_spans(&row.budget, budget_room);
    let budget_w: usize = budget.iter().map(|s| display_len(&s.content)).sum();
    spans.extend(budget);
    used += budget_w;

    let pad = inner_w.saturating_sub(used + tag_block);
    spans.push(Span::raw(" ".repeat(pad)));
    if show_prefix {
        spans.push(Span::styled("experimental", Style::default().fg(theme::EXPERIMENTAL)));
        spans.push(Span::styled(sep.to_owned(), Style::default().fg(theme::DIM)));
    }
    spans.push(Span::styled(tag.to_owned(), Style::default().fg(tag_color)));

    Line::from(spans)
}

/// The budget block, rendered to fit `room`.
///
/// Every caller-visible number is preserved or the block is not
/// emitted at all - it degrades by dropping punctuation and then whole
/// periods, never by letting the widget cut a figure mid-digit. When
/// even the shortest form does not fit, an ellipsis stands in, so a
/// squeezed row reads as truncated rather than as a smaller number.
fn budget_spans(budget: &AccountBudget, room: usize) -> Vec<Span<'static>> {
    let candidates = match budget {
        AccountBudget::Unknown { spend_billed: false } => vec![window_spans(None, None)],
        AccountBudget::Subscription { five_hour_util, seven_day_util, resets_at } => {
            let windows = window_spans(*five_hour_util, *seven_day_util);
            match resets_at {
                // The ETA is the first thing to go: it is a convenience
                // on a row whose percentages already say the account is
                // capped.
                Some(at) => {
                    let mut with_eta = windows.clone();
                    with_eta.push(Span::raw("  "));
                    with_eta.push(Span::styled(
                        format!("\u{27F3} {}", format_reset_in(*at)),
                        Style::default().fg(theme::STATUS_WARNING),
                    ));
                    vec![with_eta, windows]
                }
                None => vec![windows],
            }
        }
        // An API account has no window, so its unprobed row must not
        // print `5h` / `7d` labels - that would assert a shape it does
        // not have, which is the same invention as a fabricated zero.
        AccountBudget::Unknown { spend_billed: true } => {
            vec![spend_spans(None, SpendDensity::Full)]
        }
        AccountBudget::Api { daily, weekly, monthly } => {
            let figures = Some((*daily, *weekly, *monthly));
            vec![
                spend_spans(figures, SpendDensity::Full),
                spend_spans(figures, SpendDensity::LeadingSymbolOnly),
                spend_spans(figures, SpendDensity::Tight),
                spend_spans(figures, SpendDensity::MonthlyOnly),
            ]
        }
    };

    for candidate in candidates {
        let width: usize = candidate.iter().map(|s| display_len(&s.content)).sum();
        if width <= room {
            return candidate;
        }
    }
    // A row with no room left carries no block at all; the ellipsis is
    // itself a column, and emitting it here would overflow the very
    // budget it is standing in for.
    if room == 0 {
        return Vec::new();
    }
    vec![Span::styled("\u{2026}".to_owned(), Style::default().fg(theme::DIM))]
}

/// How much punctuation the spend figures can afford, widest first.
#[derive(Clone, Copy)]
enum SpendDensity {
    /// `$0.56 d $1.25 w $20.30 m`
    Full,
    /// `$0.56 d 1.25 w 20.30 m` - the symbol reads once for the row.
    LeadingSymbolOnly,
    /// `$0.56d 1.25w 20.30m`
    Tight,
    /// `…$20.30m` - the period that answers "where am I this billing
    /// cycle", kept when nothing else fits. Carries the ellipsis so a
    /// row that dropped two figures reads as truncated; without it the
    /// same account shows three figures when usable and one when rate
    /// limited, and the short form looks like complete data.
    MonthlyOnly,
}

/// The `d` / `w` / `m` spend columns. `None` figures render dashes for
/// an account whose provider bills by spend but which has no reading.
fn spend_spans(figures: Option<(f64, f64, f64)>, density: SpendDensity) -> Vec<Span<'static>> {
    let amount = |value: Option<f64>, symbol: bool| -> String {
        match value {
            Some(v) if symbol => format!("${v:.2}"),
            Some(v) => format!("{v:.2}"),
            None if symbol => "$-".to_owned(),
            None => "-".to_owned(),
        }
    };
    let (daily, weekly, monthly) = match figures {
        Some((d, w, m)) => (Some(d), Some(w), Some(m)),
        None => (None, None, None),
    };
    let columns: Vec<(String, &str)> = match density {
        SpendDensity::MonthlyOnly => vec![(format!("\u{2026}{}", amount(monthly, true)), "m")],
        SpendDensity::Full => vec![
            (amount(daily, true), "d"),
            (amount(weekly, true), "w"),
            (amount(monthly, true), "m"),
        ],
        SpendDensity::LeadingSymbolOnly | SpendDensity::Tight => vec![
            (amount(daily, true), "d"),
            (amount(weekly, false), "w"),
            (amount(monthly, false), "m"),
        ],
    };
    let period_gap =
        if matches!(density, SpendDensity::Tight | SpendDensity::MonthlyOnly) { "" } else { " " };

    let mut spans = Vec::new();
    for (idx, (money_text, period)) in columns.into_iter().enumerate() {
        if idx > 0 {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(
            money_text,
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(format!("{period_gap}{period}"), Style::default().fg(theme::DIM)));
    }
    spans
}

/// The `5h` / `7d` pair. A column with no figure paints a dim `-`
/// rather than a zero: absent and zero are different answers, and only
/// one of them is something forge measured.
fn window_spans(five_hour: Option<f64>, seven_day: Option<f64>) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (idx, (label, util)) in [("5h ", five_hour), ("7d ", seven_day)].into_iter().enumerate() {
        if idx > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(label.to_owned(), Style::default().fg(theme::DIM)));
        match util {
            Some(util) => spans.push(Span::styled(
                format!("{util:.0}%"),
                Style::default().fg(pct_color(util)).add_modifier(Modifier::BOLD),
            )),
            None => {
                spans.push(Span::styled("-".to_owned(), Style::default().fg(theme::DIM)));
            }
        }
    }
    spans
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

/// Terminal columns `s` occupies, not chars. A CJK glyph is one char
/// and two columns, so counting chars builds a row that fits on paper
/// and overruns the rect it is painted into.
fn display_len(s: &str) -> usize {
    Span::raw(s).width()
}

/// Pad or truncate `s` to exactly `width` terminal columns. Truncation
/// walks columns rather than chars, and stops short by one when the
/// next glyph is double-width, so the result never overshoots.
fn truncate_pad(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let w = Span::raw(ch.to_string()).width();
        if used + w > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.extend(std::iter::repeat_n(' ', width - used));
    out
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
            unusable: None,
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

    /// Three different answers that must not render alike: no snapshot,
    /// a snapshot carrying no figure for a column, and a real zero.
    /// Only the last is a number forge actually measured.
    #[test]
    fn a_column_with_no_figure_is_a_dash_not_a_zero() {
        let mut app = App::test_default();
        let rows = vec![
            AccountRow {
                display_name: "Unprobed".to_owned(),
                config_dir: PathBuf::from("/c/unprobed"),
                is_current: false,
                unusable: None,
                budget: AccountBudget::Unknown { spend_billed: false },
                experimental: false,
            },
            AccountRow {
                display_name: "NoFigures".to_owned(),
                config_dir: PathBuf::from("/c/nofigures"),
                is_current: false,
                unusable: None,
                // The lenient mapper's documented steady state: a 200
                // that carried no windows at all.
                budget: AccountBudget::Subscription {
                    five_hour_util: None,
                    seven_day_util: None,
                    resets_at: None,
                },
                experimental: false,
            },
            AccountRow {
                display_name: "HalfKnown".to_owned(),
                config_dir: PathBuf::from("/c/halfknown"),
                is_current: false,
                unusable: None,
                // The strict mapper requires five_hour only, so a 200
                // with just the session window lands here.
                budget: AccountBudget::Subscription {
                    five_hour_util: Some(12.0),
                    seven_day_util: None,
                    resets_at: None,
                },
                experimental: false,
            },
            AccountRow {
                display_name: "Fresh".to_owned(),
                config_dir: PathBuf::from("/c/fresh"),
                is_current: false,
                unusable: None,
                budget: AccountBudget::Subscription {
                    five_hour_util: Some(0.0),
                    seven_day_util: Some(0.0),
                    resets_at: None,
                },
                experimental: false,
            },
        ];
        crate::app::account_picker::open(&mut app, rows);

        let lines = render_picker(&app, 80, 16);
        let find =
            |name: &str| lines.iter().find(|l| l.contains(name)).expect("row present").clone();

        // Assert the dash is PRESENT, not merely that a percentage is
        // absent: a row rendering some other wrong figure would satisfy
        // the negative and say nothing.
        let unprobed = find("Unprobed");
        assert!(
            unprobed.contains("5h -") && unprobed.contains("7d -"),
            "no snapshot means a dash in both columns: {unprobed}",
        );

        let no_figures = find("NoFigures");
        assert!(
            no_figures.contains("5h -") && no_figures.contains("7d -"),
            "a snapshot carrying no windows shows dashes, not 0%: {no_figures}",
        );

        let half = find("HalfKnown");
        assert!(half.contains("5h 12%"), "the column that has a figure shows it: {half}");
        assert!(half.contains("7d -"), "the column that has none shows a dash, not 0%: {half}");

        let fresh = find("Fresh");
        assert!(
            fresh.contains("5h 0%") && fresh.contains("7d 0%"),
            "a measured zero is still a zero: {fresh}",
        );
    }

    /// The layout property, over every budget variant crossed with the
    /// three tags and both experimental flags: a row never exceeds the
    /// width it is given, and its status tag survives intact with a gap
    /// before it.
    ///
    /// The fixtures span the shapes the data can take: a three-digit
    /// month, a three-digit-hour reset (a 7-day cap four days out), and
    /// the widest tag combination.
    #[test]
    fn no_row_shape_overflows_or_swallows_its_tag() {
        let far = SystemTime::now() + Duration::from_secs(100 * 3600 + 41 * 60);
        let budgets = vec![
            ("unknown windows", AccountBudget::Unknown { spend_billed: false }),
            ("unknown spend", AccountBudget::Unknown { spend_billed: true }),
            (
                "windows with a far reset",
                AccountBudget::Subscription {
                    five_hour_util: Some(100.0),
                    seven_day_util: Some(100.0),
                    resets_at: Some(far),
                },
            ),
            (
                "windows half absent",
                AccountBudget::Subscription {
                    five_hour_util: Some(12.0),
                    seven_day_util: None,
                    resets_at: None,
                },
            ),
            ("spend at zero", AccountBudget::Api { daily: 0.0, weekly: 0.0, monthly: 0.0 }),
            ("spend typical", AccountBudget::Api { daily: 0.56, weekly: 1.25, monthly: 20.30 }),
            (
                "spend three digit",
                AccountBudget::Api { daily: 12.34, weekly: 56.78, monthly: 234.56 },
            ),
            (
                "spend four digit",
                AccountBudget::Api { daily: 20.00, weekly: 100.00, monthly: 4000.00 },
            ),
        ];

        // Painted widths, not the constant: a split pane clamps the
        // overlay, so the row has to hold at whatever it is given.
        for inner_w in [30usize, 44, 54, 60] {
            for (label, budget) in &budgets {
                for unusable in [
                    None,
                    Some(Unusable::Saturated),
                    Some(Unusable::ProbeBlocked),
                    Some(Unusable::Bailed),
                ] {
                    for experimental in [true, false] {
                        let row = AccountRow {
                            display_name: "OpenRouter".to_owned(),
                            config_dir: PathBuf::from("/c/x"),
                            is_current: true,
                            unusable,
                            budget: budget.clone(),
                            experimental,
                        };
                        let line = account_row_line(&row, true, inner_w);
                        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                        let width = Span::raw(text.clone()).width();
                        let tag = match unusable {
                            None => "usable",
                            Some(Unusable::Saturated) => "limit hit",
                            Some(Unusable::ProbeBlocked | Unusable::Bailed) => {
                                "auth failed or expired"
                            }
                        };
                        let case =
                            format!("{label} w={inner_w} unusable={unusable:?} exp={experimental}");

                        assert!(
                            width <= inner_w,
                            "{case} overflows at {width} of {inner_w}: |{text}|",
                        );
                        assert!(text.ends_with(tag), "{case} lost its tag: |{text}|");
                        // What precedes the tag: the dim separator on an
                        // experimental row, otherwise the padding gap -
                        // which is also what an experimental row falls
                        // back to when it is too narrow to carry the
                        // amber prefix beside the tag. The experimental
                        // shape is the one where welding is possible at
                        // all, so it must not be excused.
                        let before = &text[..text.len() - tag.len()];
                        let prefix_fits = experimental
                            && 2 + display_len("experimental") + 3 + tag.len() + TAG_GAP <= inner_w;
                        let expected_lead = if prefix_fits { " \u{00B7} " } else { " " };
                        assert!(
                            before.ends_with(expected_lead),
                            "{case} welded the tag onto the budget: |{text}|",
                        );
                    }
                }
            }
        }
    }

    /// The overlay is clamped to the terminal, so the width a row is
    /// built for has to be the width it is painted into. A vertical
    /// split on a 110-column terminal gives 55.
    #[test]
    fn a_narrow_terminal_still_paints_the_status_tag() {
        for term_w in [40u16, 50, 56, 62, 80] {
            let mut app = App::test_default();
            let rows = vec![AccountRow {
                display_name: "Gateway".to_owned(),
                config_dir: PathBuf::from("/c/gateway"),
                is_current: true,
                unusable: Some(Unusable::Saturated),
                budget: AccountBudget::Subscription {
                    five_hour_util: Some(100.0),
                    seven_day_util: Some(63.0),
                    resets_at: None,
                },
                experimental: false,
            }];
            crate::app::account_picker::open(&mut app, rows);

            let lines = render_picker(&app, term_w, 16);
            let row = lines
                .iter()
                .find(|l| l.contains("Gateway"))
                .unwrap_or_else(|| panic!("row present at {term_w} columns"));
            assert!(
                row.contains("limit hit"),
                "at {term_w} columns the status tag is missing entirely: |{row}|",
            );
        }
    }

    /// `display_len` counts chars; a wide glyph paints two columns. A
    /// name built to fit by count overflows the row it is painted into.
    #[test]
    fn a_wide_glyph_name_does_not_push_the_tag_off_the_row() {
        let mut app = App::test_default();
        let rows = vec![AccountRow {
            // Ten chars, twenty columns.
            display_name:
                "\u{4e2d}\u{6587}\u{4e2d}\u{6587}\u{4e2d}\u{6587}\u{4e2d}\u{6587}\u{4e2d}\u{6587}"
                    .to_owned(),
            config_dir: PathBuf::from("/c/wide"),
            is_current: false,
            unusable: Some(Unusable::Saturated),
            budget: AccountBudget::Subscription {
                five_hour_util: Some(100.0),
                seven_day_util: Some(63.0),
                resets_at: None,
            },
            experimental: false,
        }];
        crate::app::account_picker::open(&mut app, rows);

        let lines = render_picker(&app, 80, 16);
        let row = lines.iter().find(|l| l.contains('\u{4e2d}')).expect("row present");
        assert!(
            row.contains("limit hit"),
            "a wide-glyph name must not push the tag off the row: |{row}|",
        );
    }

    /// An API budget under the widest tag block, `experimental` plus
    /// `auth failed or expired`. Reachable on any spend value, because
    /// a 429 from the key endpoint preserves the snapshot; on a spend
    /// row this is the only unusable shape there is, since spend carries
    /// no window to saturate.
    #[test]
    fn the_widest_api_row_keeps_its_tag_separate_and_uncut() {
        let mut app = App::test_default();
        let rows = vec![AccountRow {
            display_name: "Router".to_owned(),
            config_dir: PathBuf::from("/c/router"),
            is_current: false,
            unusable: Some(Unusable::ProbeBlocked),
            budget: AccountBudget::Api { daily: 0.0, weekly: 0.0, monthly: 0.0 },
            experimental: true,
        }];
        crate::app::account_picker::open(&mut app, rows);

        let lines = render_picker(&app, 80, 16);
        let row = lines.iter().find(|l| l.contains("Router")).expect("row present");

        assert!(
            row.contains("auth failed or expired"),
            "the status tag must not be clipped off the end: {row}",
        );
        assert!(
            !row.contains("mexperimental"),
            "the tag must not weld onto the budget block: {row}",
        );
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
            unusable: None,
            budget: AccountBudget::Subscription {
                five_hour_util: Some(20.0),
                seven_day_util: Some(8.0),
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
                unusable: Some(Unusable::Saturated),
                budget: AccountBudget::Subscription {
                    five_hour_util: Some(100.0),
                    seven_day_util: Some(63.0),
                    resets_at: Some(future),
                },
                experimental: false,
            },
            AccountRow {
                display_name: "Gateway1".to_owned(),
                config_dir: PathBuf::from("/c/gateway1"),
                is_current: false,
                unusable: None,
                budget: AccountBudget::Subscription {
                    five_hour_util: Some(34.0),
                    seven_day_util: Some(22.0),
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
        assert!(joined.contains("limit hit"), "capped account tagged limit hit");
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
                unusable: None,
                budget: AccountBudget::Subscription {
                    five_hour_util: Some(10.0),
                    seven_day_util: Some(5.0),
                    resets_at: None,
                },
                experimental: false,
            },
            AccountRow {
                display_name: "Codex".to_owned(),
                config_dir: PathBuf::from("/c/codex"),
                is_current: false,
                unusable: None,
                budget: AccountBudget::Subscription {
                    five_hour_util: Some(20.0),
                    seven_day_util: Some(8.0),
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
