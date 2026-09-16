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

pub(crate) fn render(frame: &mut Frame, area: Rect, app: &App) {
    let Some(state) = app.gateway_view.as_ref() else {
        return;
    };
    let lines = gateway_lines(&state.orgs);
    // Body plus a blank line above the footer, inside a 1-cell border.
    let height = u16::try_from(lines.len()).unwrap_or(u16::MAX).saturating_add(3);
    let overlay = centered(area, WIDTH, height);

    frame.render_widget(Clear, overlay);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::RUST_ORANGE));
    let inner = block.inner(overlay);
    frame.render_widget(block, overlay);

    let mut body = lines;
    body.push(Line::default());
    body.push(Line::from(Span::styled(
        "esc close   read-only: the spawn pick decides every session's account",
        Style::default().fg(theme::DIM),
    )));
    frame.render_widget(Paragraph::new(body), inner);
}

/// The overlay's body: a title, one block per org (its pins, then its
/// accounts), in the order the snapshot reports.
fn gateway_lines(orgs: &[GatewayOrgView]) -> Vec<Line<'static>> {
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
            format!("{INDENT}primary   {}", list(&org.accounts)),
            Style::default().fg(theme::DIM),
        )));
        lines.push(Line::from(Span::styled(
            format!("{INDENT}fallback  {}", list(&org.fallback_accounts)),
            Style::default().fg(theme::DIM),
        )));
        for row in &org.rows {
            lines.push(account_line(row));
        }
    }
    lines
}

/// One account line: name, provider, what it has left, and its state.
fn account_line(row: &AccountRow) -> Line<'static> {
    let (tag, tag_color) = match row.unusable {
        None if row.loading == LoadingState::Loading => ("loading", theme::DIM),
        None => ("usable", Color::Green),
        Some(Unusable::Saturated) => ("limit hit", theme::STATUS_ERROR),
        Some(Unusable::ProbeBlocked | Unusable::Bailed) => {
            ("auth failed or expired", theme::STATUS_ERROR)
        }
    };
    let mut spans = vec![
        Span::raw(format!("{INDENT}  ")),
        Span::styled(row.display_name.clone(), Style::default().add_modifier(Modifier::BOLD)),
    ];
    if row.fallback {
        spans.push(Span::styled(" fallback", Style::default().fg(theme::DIM)));
    }
    spans.push(Span::raw(format!(
        "  {}  {}",
        provider_label(row.provider),
        budget_summary(&row.budget)
    )));
    spans.push(Span::styled(format!("  {tag}"), Style::default().fg(tag_color)));
    Line::from(spans)
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

/// What the account has left, in its backend's own terms. A column the
/// snapshot did not carry reads `-`: a real absence, never a zero.
fn budget_summary(budget: &AccountBudget) -> String {
    match budget {
        AccountBudget::Unknown { .. } => "-".to_owned(),
        AccountBudget::Subscription { five_hour_util, seven_day_util, resets_at } => {
            let pct = |value: Option<f64>| match value {
                Some(pct) => format!("{pct:.0}%"),
                None => "-".to_owned(),
            };
            let mut text = format!("5h {}  7d {}", pct(*five_hour_util), pct(*seven_day_util));
            if let Some(at) = resets_at {
                let remaining = at.duration_since(std::time::SystemTime::now()).unwrap_or_default();
                let mins = remaining.as_secs() / 60;
                let (hours, minutes) = (mins / 60, mins % 60);
                let eta = if hours > 0 {
                    format!("  resets {hours}h {minutes}m")
                } else {
                    format!("  resets {minutes}m")
                };
                text.push_str(eta.as_str());
            }
            text
        }
        AccountBudget::Api { daily, weekly, monthly } => {
            format!("day ${daily:.2}  week ${weekly:.2}  month ${monthly:.2}")
        }
    }
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
            config_dir: std::path::PathBuf::from("/cfg"),
            is_current: false,
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

    /// Exact lines: the org's pins, then its accounts with their state,
    /// in the order the snapshot reports them.
    #[test]
    fn the_view_lists_each_org_with_its_pins_and_account_state() {
        let orgs = vec![GatewayOrgView {
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
        }];

        let lines: Vec<String> = gateway_lines(&orgs).iter().map(line_text).collect();
        assert_eq!(
            lines,
            vec![
                "Gateway \u{00B7} 1 orgs",
                "",
                "  Busytools",
                "  primary   Ready, Capped",
                "  fallback  Spare",
                "    Ready  anthropic  5h 34%  7d 22%  usable",
                "    Capped  zai  -  limit hit",
                "    Spare fallback  openrouter  day $1.50  week $9.25  month $40.00  auth failed or expired",
            ],
            "the block reads pins first, then accounts in snapshot order",
        );
    }

    /// An account whose boot probe has not settled reads `loading`, not
    /// `usable`: nothing has verified it yet, so the row must not claim
    /// it is pickable.
    #[test]
    fn an_account_still_loading_is_not_tagged_usable() {
        let orgs = vec![GatewayOrgView {
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
        }];
        let lines: Vec<String> = gateway_lines(&orgs).iter().map(line_text).collect();
        assert_eq!(lines[5], "    Pending  anthropic  -  loading");
    }

    /// An org with no fallbacks reads `-`, not a blank: the empty list
    /// is the normal shape rather than a missing value.
    #[test]
    fn an_org_with_no_fallbacks_reads_a_dash() {
        let orgs = vec![GatewayOrgView {
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
        }];
        let lines: Vec<String> = gateway_lines(&orgs).iter().map(line_text).collect();
        assert_eq!(lines[4], "  fallback  -", "no fallbacks is a dash, not an empty cell");
        assert_eq!(lines[5], "    Solo  anthropic  -  usable");
    }

    /// A capped subscription carries its reset ETA, which is the only
    /// place the view tells the reader when an exhausted account frees.
    #[test]
    fn a_capped_subscription_shows_its_reset() {
        // Half a minute of slack: the ETA is computed against the wall
        // clock at render time, so a bare boundary reading flips a
        // minute under load.
        let resets_at = SystemTime::now() + std::time::Duration::from_secs(2 * 3600 + 330);
        let orgs = vec![GatewayOrgView {
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
        }];
        let lines: Vec<String> = gateway_lines(&orgs).iter().map(line_text).collect();
        assert_eq!(
            lines[5], "    Capped  anthropic  5h 100%  7d 63%  resets 2h 5m  limit hit",
            "a capped account names when it frees",
        );
    }
}
