//! `/gateway` overlay render.
//!
//! A centered, read-only modal showing what the gateway holds: one
//! block per org with its primary and fallback pins, then one line per
//! account naming its provider, what it has left, and whether it is
//! pickable. State and key handling live in
//! [`crate::app::gateway_view`]; nothing here switches, rebinds or
//! respawns anything.

use forge_primitives::account::Provider;
use forge_primitives::usage::AccountBudget;
use forge_workspace::{AccountRow, GatewayOrgView, LoadingState, Unusable};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::App;
use crate::ui::theme;

const WIDTH: u16 = 64;
const INDENT: &str = "  ";
/// The name column's cap. Longer names truncate rather than pushing the
/// state tag out of the overlay.
const NAME_W: usize = 14;
/// Columns kept between a row's detail and its tag.
const TAG_GAP: usize = 1;
/// The key hint plus the read-only statement, kept inside the inner
/// width so the paragraph never cuts it.
const FOOTER: &str = "esc close   read-only: the spawn pick decides every account";

pub(crate) fn render(frame: &mut Frame, area: Rect, app: &App) {
    let Some(state) = app.gateway_view.as_ref() else {
        return;
    };
    // The content, a blank, then the footer is the body; the border adds
    // a row above and below, so the overlay is the body plus two. The
    // row count is width-independent, which is what lets the height be
    // settled before the rows are built to fit their width.
    let body_lines = gateway_line_count(&state.orgs) + 2;
    let height = u16::try_from(body_lines).unwrap_or(u16::MAX).saturating_add(2);
    let overlay = centered(area, WIDTH, height);

    frame.render_widget(Clear, overlay);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::RUST_ORANGE));
    let inner = block.inner(overlay);
    frame.render_widget(block, overlay);

    let mut body = gateway_lines(&state.orgs, usize::from(inner.width));
    body.push(Line::default());
    body.push(Line::from(Span::styled(FOOTER, Style::default().fg(theme::DIM))));
    frame.render_widget(Paragraph::new(body), inner);
}

/// Lines the content occupies: the title, then per org a blank, the org
/// header, its two pin lines and its account rows.
fn gateway_line_count(orgs: &[GatewayOrgView]) -> usize {
    1 + orgs.iter().map(|org| 4 + org.rows.len()).sum::<usize>()
}

/// The overlay's body: a title, one block per org (its pins, then its
/// accounts), in the order the snapshot reports. Rows are built to fit
/// `inner_w` because the paragraph does not wrap: an overrun is cut, and
/// the state tag is the last thing that may be.
fn gateway_lines(orgs: &[GatewayOrgView], inner_w: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let title = format!("Gateway \u{00B7} {} orgs", orgs.len());
    lines.push(Line::from(Span::styled(
        title,
        Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD),
    )));

    for org in orgs {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            format!("{INDENT}{}", org.org),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(Span::styled(
            truncate_pad(&format!("{INDENT}primary   {}", list(&org.accounts)), inner_w),
            Style::default().fg(theme::DIM),
        )));
        lines.push(Line::from(Span::styled(
            truncate_pad(&format!("{INDENT}fallback  {}", list(&org.fallback_accounts)), inner_w),
            Style::default().fg(theme::DIM),
        )));
        for row in &org.rows {
            lines.push(account_line(row, inner_w));
        }
    }
    lines
}

/// One account line: name, provider, what it has left, and its state.
///
/// Built against `inner_w` so the tag always survives: the name column
/// shrinks to make room for it, then the provider/budget detail degrades
/// to the provider alone and then to nothing. The paragraph does not
/// wrap, so whatever is left over would be cut without a mark - and the
/// state tag is the one column this view exists to show.
fn account_line(row: &AccountRow, inner_w: usize) -> Line<'static> {
    let (tag, tag_color) = match row.unusable {
        None if row.loading == LoadingState::Loading => ("loading", theme::DIM),
        None => ("usable", Color::Green),
        Some(Unusable::Saturated) => ("limit hit", theme::STATUS_ERROR),
        Some(Unusable::ProbeBlocked | Unusable::Bailed) => {
            ("auth failed or expired", theme::STATUS_ERROR)
        }
    };
    let tag_w = display_len(tag);
    let mut used = 0usize;

    let mut spans = vec![Span::raw(format!("{INDENT}  "))];
    used += display_len(&format!("{INDENT}  "));

    // The name column gives way first, and stops shrinking while the tag
    // and a gap still fit.
    let name_w = NAME_W.min(inner_w.saturating_sub(used + tag_w + TAG_GAP));
    let name = truncate_pad(&row.display_name, name_w);
    used += display_len(&name);
    spans.push(Span::styled(name, Style::default().add_modifier(Modifier::BOLD)));

    if row.fallback {
        // The suffix gives way before the tag ever would: the pin lines
        // above already say which list the account came from.
        let fallback_w = display_len(" fallback");
        if used + fallback_w + tag_w + TAG_GAP <= inner_w {
            spans.push(Span::styled(" fallback", Style::default().fg(theme::DIM)));
            used += fallback_w;
        }
    }

    // The detail degrades rather than vanishing whole: the row takes the
    // longest budget form that fits, then the provider alone, then
    // nothing.
    let room = inner_w.saturating_sub(used + tag_w + TAG_GAP);
    let provider = provider_label(row.provider);
    let provider_only = format!("  {provider}");
    let detail = budget_forms(&row.budget)
        .into_iter()
        .map(|figures| format!("  {provider}  {figures}"))
        .find(|text| display_len(text) <= room)
        .or_else(|| (display_len(&provider_only) <= room).then_some(provider_only))
        .unwrap_or_default();
    used += display_len(&detail);
    spans.push(Span::raw(detail));

    // Right-align the tag: it is never the part that is cut.
    let pad = inner_w.saturating_sub(used + tag_w);
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    }
    spans.push(Span::styled(tag.to_owned(), Style::default().fg(tag_color)));
    Line::from(spans)
}

/// Terminal columns `s` occupies, not chars: a CJK glyph is one char and
/// two columns, so counting chars builds a row that fits on paper and
/// overruns the rect it is painted into.
fn display_len(s: &str) -> usize {
    Span::raw(s).width()
}

/// Pad or truncate `s` to exactly `width` terminal columns. Truncation
/// walks columns rather than chars, and stops short by one when the next
/// glyph is double-width, so the result never overshoots.
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

/// A pin render: its names, or `-` when the list is empty (an org with
/// no fallbacks is the normal shape, not a missing value).
fn list(names: &[String]) -> String {
    if names.is_empty() { "-".to_owned() } else { names.join(", ") }
}

fn provider_label(provider: Provider) -> &'static str {
    match provider {
        Provider::Anthropic => "anthropic",
        Provider::Openrouter => "openrouter",
        Provider::Zai => "zai",
        Provider::Codex => "codex",
    }
}

/// What the account has left, in its backend's own terms, longest form
/// first: the row takes the first form that fits, so a squeezed row
/// loses the reset ETA and then the period words rather than the figures
/// themselves. A column the snapshot did not carry reads `-`: a real
/// absence, never a zero.
fn budget_forms(budget: &AccountBudget) -> Vec<String> {
    match budget {
        AccountBudget::Unknown { .. } => vec!["-".to_owned()],
        AccountBudget::Subscription { five_hour_util, seven_day_util, resets_at } => {
            let figures = window_figures(*five_hour_util, *seven_day_util);
            let mut forms = vec![figures.clone()];
            if let Some(at) = resets_at {
                forms.insert(0, format!("{figures}{}", reset_eta(*at)));
            }
            forms
        }
        AccountBudget::Api { daily, weekly, monthly } => vec![
            format!("day ${daily:.2}  week ${weekly:.2}  month ${monthly:.2}"),
            format!("${daily:.2} d  ${weekly:.2} w  ${monthly:.2} m"),
            format!("${monthly:.2} m"),
        ],
    }
}

/// `resets <when>` for a capped account, from its reset moment.
fn reset_eta(resets_at: std::time::SystemTime) -> String {
    let remaining = resets_at.duration_since(std::time::SystemTime::now()).unwrap_or_default();
    let mins = remaining.as_secs() / 60;
    let (hours, minutes) = (mins / 60, mins % 60);
    if hours > 0 { format!("  resets {hours}h {minutes}m") } else { format!("  resets {minutes}m") }
}

/// The two plan windows as percentages; a window the snapshot did not
/// carry reads `-` rather than a zero.
fn window_figures(five_hour_util: Option<f64>, seven_day_util: Option<f64>) -> String {
    let pct = |value: Option<f64>| match value {
        Some(pct) => format!("{pct:.0}%"),
        None => "-".to_owned(),
    };
    format!("5h {}  7d {}", pct(five_hour_util), pct(seven_day_util))
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
    use std::time::SystemTime;

    fn row(
        name: &str,
        provider: Provider,
        unusable: Option<Unusable>,
        budget: AccountBudget,
        fallback: bool,
    ) -> AccountRow {
        AccountRow {
            display_name: name.to_owned(),
            unusable,
            budget,
            fallback,
            provider,
            loading: LoadingState::Ready,
        }
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|span| span.content.as_ref()).collect()
    }

    fn render_lines(orgs: &[GatewayOrgView], inner_w: usize) -> Vec<String> {
        gateway_lines(orgs, inner_w).iter().map(line_text).collect()
    }

    /// Three accounts: healthy, capped, and a fallback whose probe is
    /// blocked - one of each state tag the view renders.
    fn mixed_org() -> GatewayOrgView {
        GatewayOrgView {
            org: "Busytools".to_owned(),
            accounts: vec!["Ready".to_owned(), "Capped".to_owned()],
            fallback_accounts: vec!["Spare".to_owned()],
            rows: vec![
                row(
                    "Ready",
                    Provider::Anthropic,
                    None,
                    AccountBudget::Subscription {
                        five_hour_util: Some(34.0),
                        seven_day_util: Some(22.0),
                        resets_at: None,
                    },
                    false,
                ),
                row(
                    "Capped",
                    Provider::Zai,
                    Some(Unusable::Saturated),
                    AccountBudget::Unknown { spend_billed: false },
                    false,
                ),
                row(
                    "Spare",
                    Provider::Openrouter,
                    Some(Unusable::ProbeBlocked),
                    AccountBudget::Api { daily: 1.5, weekly: 9.25, monthly: 40.0 },
                    true,
                ),
            ],
        }
    }

    /// The block reads the org, then its pins in order, then one row per
    /// account in snapshot order, each ending in its state tag.
    #[test]
    fn the_view_lists_each_org_with_its_pins_and_account_state() {
        let lines = render_lines(&[mixed_org()], 200);

        assert_eq!(lines[0], "Gateway \u{00B7} 1 orgs");
        assert_eq!(lines[1], "");
        assert_eq!(lines[2], "  Busytools");
        assert!(
            lines[3].starts_with("  primary   Ready, Capped"),
            "the primary pin reads in order; got: {}",
            lines[3],
        );
        assert!(
            lines[4].starts_with("  fallback  Spare"),
            "the fallback pin reads next; got: {}",
            lines[4],
        );
        assert!(
            lines[5].starts_with("    Ready") && lines[5].ends_with("usable"),
            "the healthy account names its state; got: {}",
            lines[5],
        );
        assert!(
            lines[6].starts_with("    Capped") && lines[6].ends_with("limit hit"),
            "the capped account reads limit hit; got: {}",
            lines[6],
        );
        assert!(
            lines[7].starts_with("    Spare")
                && lines[7].contains("fallback")
                && lines[7].ends_with("auth failed or expired"),
            "the fallback row carries both its group suffix and its state; got: {}",
            lines[7],
        );
    }

    /// Every account row fits the width it is painted into, and the
    /// state tag survives at the end, across widths that force the
    /// detail and the name column to give way first. The paragraph does
    /// not wrap, so an overrun would be cut without a mark.
    #[test]
    fn every_account_row_fits_its_width_and_keeps_its_tag() {
        let org = GatewayOrgView {
            org: "Busytools".to_owned(),
            accounts: vec!["OpenRouter-TM".to_owned()],
            fallback_accounts: Vec::new(),
            rows: vec![row(
                "OpenRouter-TM",
                Provider::Openrouter,
                Some(Unusable::Saturated),
                AccountBudget::Api { daily: 1.5, weekly: 9.25, monthly: 40.0 },
                true,
            )],
        };

        for inner_w in [62usize, 48, 32, 24] {
            let lines = render_lines(std::slice::from_ref(&org), inner_w);
            let account = lines.last().expect("the account row");
            assert!(
                display_len(account) == inner_w && account.ends_with("limit hit"),
                "the tag is right-aligned on a row of exactly {inner_w} columns; got {account:?}",
            );
            assert!(
                account.starts_with("    OpenRouter"),
                "the name column leads the row at {inner_w} columns; got {account:?}",
            );
        }
    }

    /// The rendered row count is what the height arithmetic counts: the
    /// two must not drift, or the overlay clips its last line again.
    #[test]
    fn the_built_lines_match_the_counted_height() {
        let orgs = vec![mixed_org()];
        assert_eq!(
            gateway_lines(&orgs, 62).len(),
            gateway_line_count(&orgs),
            "the height arithmetic counts exactly what the builder emits",
        );
    }

    /// The footer - the only key hint and the read-only statement - is
    /// painted inside the overlay, which the height arithmetic has to
    /// leave room for.
    #[test]
    fn the_render_paints_the_footer() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = App::test_default();
        crate::app::gateway_view::open(&mut app, vec![mixed_org()]);

        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| render(frame, frame.area(), &app)).expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let rows: Vec<String> = (0..30)
            .map(|y| {
                (0..80)
                    .map(|x| {
                        buffer
                            .cell((x, y))
                            .map_or(' ', |c| c.symbol().chars().next().unwrap_or(' '))
                    })
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect();

        assert!(
            rows.iter().any(|row| row.contains("esc close")),
            "the footer must be painted; got: {rows:?}",
        );
        assert!(
            rows.iter().any(|row| row.contains("read-only")),
            "the footer carries the read-only statement; got: {rows:?}",
        );
        assert!(
            display_len(FOOTER) <= usize::from(WIDTH) - 2,
            "the footer must fit the inner width; it is {} columns",
            display_len(FOOTER),
        );
    }

    /// An account whose boot probe has not settled reads `loading`, not
    /// `usable`: nothing has verified it yet, so the row must not claim
    /// it is pickable.
    #[test]
    fn an_account_still_loading_is_not_tagged_usable() {
        let org = GatewayOrgView {
            org: "Default".to_owned(),
            accounts: vec!["Pending".to_owned()],
            fallback_accounts: Vec::new(),
            rows: vec![AccountRow {
                loading: LoadingState::Loading,
                ..row(
                    "Pending",
                    Provider::Anthropic,
                    None,
                    AccountBudget::Unknown { spend_billed: false },
                    false,
                )
            }],
        };
        let lines = render_lines(&[org], 62);
        assert!(
            lines[5].ends_with("loading"),
            "an unsettled account reads loading; got: {}",
            lines[5],
        );
    }

    /// An org with no fallbacks reads `-`, not a blank: the empty list
    /// is the normal shape rather than a missing value.
    #[test]
    fn an_org_with_no_fallbacks_reads_a_dash() {
        let org = GatewayOrgView {
            org: "Subspace".to_owned(),
            accounts: vec!["Solo".to_owned()],
            fallback_accounts: Vec::new(),
            rows: vec![row(
                "Solo",
                Provider::Anthropic,
                None,
                AccountBudget::Unknown { spend_billed: false },
                false,
            )],
        };
        let lines = render_lines(&[org], 62);
        assert!(
            lines[4].starts_with("  fallback  -"),
            "no fallbacks is a dash, not an empty cell; got: {}",
            lines[4],
        );
        assert!(lines[5].starts_with("    Solo"), "the account row follows; got: {}", lines[5]);
    }

    /// A capped subscription carries its reset ETA, which is the only
    /// place the view tells the reader when an exhausted account frees.
    #[test]
    fn a_capped_subscription_shows_its_reset() {
        // Half a minute of slack: the ETA is computed against the wall
        // clock at render time, so a bare boundary reading flips a
        // minute under load.
        let resets_at = SystemTime::now() + std::time::Duration::from_secs(2 * 3600 + 330);
        let org = GatewayOrgView {
            org: "Default".to_owned(),
            accounts: vec!["Capped".to_owned()],
            fallback_accounts: Vec::new(),
            rows: vec![row(
                "Capped",
                Provider::Anthropic,
                Some(Unusable::Saturated),
                AccountBudget::Subscription {
                    five_hour_util: Some(100.0),
                    seven_day_util: Some(63.0),
                    resets_at: Some(resets_at),
                },
                false,
            )],
        };
        // At the overlay's own width the window figures survive and the
        // ETA is what gives way.
        let lines = render_lines(std::slice::from_ref(&org), 62);
        assert!(
            lines[5].contains("5h 100%")
                && lines[5].contains("7d 63%")
                && lines[5].ends_with("limit hit"),
            "the figures and the tag survive at the overlay's width; got: {}",
            lines[5],
        );

        // With room to spare the ETA is shown too: it is the only place
        // the view says when an exhausted account frees.
        let wide = render_lines(&[org], 200);
        assert!(
            wide[5].contains("resets 2h 5m"),
            "a capped account names when it frees; got: {}",
            wide[5],
        );
    }
}
