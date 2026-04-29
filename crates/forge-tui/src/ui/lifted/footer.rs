#![allow(
    dead_code,
    missing_docs,
    clippy::pedantic,
    clippy::disallowed_methods,
    clippy::while_let_loop,
    clippy::collapsible_if,
    reason = "lifted upstream from claude-code-rust"
)]

use crate::state::app::App;
use crate::state::messages::{MessageBlock, MessageRole};
use crate::state::model;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::ui::theme;

const FOOTER_PAD: u16 = 2;
const FOOTER_COLUMN_GAP: u16 = 1;
const PRIMARY_ROW_LEFT_MIN_WIDTH: u16 = 24;
const SECONDARY_ROW_LEFT_MIN_WIDTH: u16 = 28;
const MIN_CONTEXT_LOCATION_WIDTH: usize = 10;
const MIN_CONTEXT_BRANCH_WIDTH: usize = 4;
type FooterItem = Option<(String, Color)>;
const FOOTER_CONTEXT_VALUE: Color = Color::Gray;

pub fn render(frame: &mut Frame, area: Rect, app: &mut App) {
    if area.height == 0 {
        return;
    }

    let padded = Rect {
        x: area.x.saturating_add(FOOTER_PAD),
        y: area.y,
        width: area.width.saturating_sub(FOOTER_PAD * 2),
        height: area.height,
    };

    let [first_row, second_row] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(padded);

    let first_line = build_primary_line(app);
    render_footer_row(
        frame,
        first_row,
        first_line,
        footer_primary_hint(app),
        PRIMARY_ROW_LEFT_MIN_WIDTH,
    );

    let second_hint = footer_secondary_hint(app);
    let (second_left, second_right) = split_footer_columns_hint(
        second_row,
        second_hint.as_ref().map(|(text, _)| text.as_str()),
        SECONDARY_ROW_LEFT_MIN_WIDTH,
    );
    frame.render_widget(
        Paragraph::new(build_context_line(app, usize::from(second_left.width))),
        second_left,
    );
    if let Some((hint_text, hint_color)) = second_hint {
        render_footer_right_info(frame, second_right, &hint_text, hint_color);
    }
}

fn footer_primary_hint(app: &App) -> FooterItem {
    let permission_count = pending_permission_request_count(app);
    if permission_count > 0 {
        return Some((format!("{permission_count} PEND. PERM."), Color::Yellow));
    }
    None
}

fn footer_mcp_auth_hint(app: &App) -> FooterItem {
    let needs_auth_count = mcp_needs_auth_count(app);
    (needs_auth_count > 0 && should_show_startup_mcp_hint(app))
        .then(|| (format!("{needs_auth_count} MCP NEEDS AUTH"), Color::Yellow))
}

fn footer_context_usage_hint(app: &App) -> FooterItem {
    app.session_usage.context_usage_percent.map(|percentage| {
        let remaining = 100_u8.saturating_sub(percentage);
        (format!("{remaining}%"), FOOTER_CONTEXT_VALUE)
    })
}

fn footer_secondary_hint(app: &App) -> FooterItem {
    footer_mcp_auth_hint(app).or_else(|| footer_context_usage_hint(app))
}

fn render_footer_row(
    frame: &mut Frame,
    area: Rect,
    left_line: Line<'static>,
    right_hint: FooterItem,
    left_min_width: u16,
) {
    let (left_area, right_area) = split_footer_columns_hint(
        area,
        right_hint.as_ref().map(|(text, _)| text.as_str()),
        left_min_width,
    );
    frame.render_widget(Paragraph::new(left_line), left_area);
    if let Some((hint_text, hint_color)) = right_hint {
        render_footer_right_info(frame, right_area, &hint_text, hint_color);
    }
}

fn split_footer_columns_hint(
    area: Rect,
    right_text: Option<&str>,
    left_min_width: u16,
) -> (Rect, Rect) {
    if area.width == 0 {
        return (area, zero_width_rect(area));
    }

    let Some(right_text) = right_text else {
        return (area, zero_width_rect(area));
    };

    let left_min_width = left_min_width.min(area.width);
    let available_right =
        area.width.saturating_sub(left_min_width).saturating_sub(FOOTER_COLUMN_GAP);
    if available_right == 0 {
        return (area, zero_width_rect(area));
    }

    let natural_right_width = u16::try_from(UnicodeWidthStr::width(right_text)).unwrap_or(u16::MAX);
    let right_width = natural_right_width.min(available_right);
    if right_width == 0 {
        return (area, zero_width_rect(area));
    }

    let left_width = area.width.saturating_sub(right_width).saturating_sub(FOOTER_COLUMN_GAP);
    let left = Rect { width: left_width, ..area };
    let right = Rect {
        x: left.x.saturating_add(left_width).saturating_add(FOOTER_COLUMN_GAP),
        width: right_width,
        ..area
    };
    (left, right)
}

fn zero_width_rect(area: Rect) -> Rect {
    Rect { x: area.x.saturating_add(area.width), width: 0, ..area }
}

fn build_primary_line(app: &App) -> Line<'static> {
    if let Some(ref mode) = app.mode {
        let color = mode_color(&mode.current_mode_id);
        let (fast_mode_text, fast_mode_color) = fast_mode_badge(app.fast_mode_state);
        let mut spans = Vec::new();
        push_badge(&mut spans, mode.current_mode_name.clone(), color);
        if let Some(model_badge) = footer_model_badge(app) {
            spans.push(Span::raw("  "));
            push_badge(&mut spans, model_badge, FOOTER_CONTEXT_VALUE);
        }
        spans.push(Span::raw("  "));
        push_badge(&mut spans, fast_mode_text.to_owned(), fast_mode_color);
        spans.push(Span::raw("  "));
        spans.push(Span::styled("?", Style::default().fg(Color::White)));
        spans.push(Span::styled(" : Help", Style::default().fg(theme::DIM)));
        Line::from(spans)
    } else {
        Line::from(vec![
            Span::styled("?", Style::default().fg(Color::White)),
            Span::styled(" : Help", Style::default().fg(theme::DIM)),
        ])
    }
}

fn push_badge(spans: &mut Vec<Span<'static>>, text: String, color: Color) {
    spans.push(Span::styled("[", Style::default().fg(color)));
    spans.push(Span::styled(text, Style::default().fg(color)));
    spans.push(Span::styled("]", Style::default().fg(color)));
}

fn footer_model_badge(app: &App) -> Option<String> {
    let current_model = app.current_model.as_ref()?;
    let mut badge = current_model.display_name_short.clone();
    if current_model.supports_effort {
        badge.push('/');
        badge.push_str(footer_effort_label(app.thinking_effort_effective()));
    }
    Some(badge)
}

const fn footer_effort_label(effort: model::EffortLevel) -> &'static str {
    match effort {
        model::EffortLevel::Low => "Low",
        model::EffortLevel::Medium => "Med",
        model::EffortLevel::High => "High",
    }
}

fn fit_footer_right_text(text: &str, max_width: usize) -> Option<String> {
    if max_width == 0 || text.trim().is_empty() {
        return None;
    }

    if UnicodeWidthStr::width(text) <= max_width {
        return Some(text.to_owned());
    }

    if max_width <= 3 {
        return Some(".".repeat(max_width));
    }

    let mut fitted = String::new();
    let mut width: usize = 0;
    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width.saturating_add(ch_width).saturating_add(3) > max_width {
            break;
        }
        fitted.push(ch);
        width = width.saturating_add(ch_width);
    }

    if fitted.is_empty() {
        return Some("...".to_owned());
    }
    fitted.push_str("...");
    Some(fitted)
}

fn render_footer_right_info(frame: &mut Frame, area: Rect, right_text: &str, right_color: Color) {
    if area.width == 0 {
        return;
    }
    let Some(fitted) = fit_footer_right_text(right_text, usize::from(area.width)) else {
        return;
    };

    let line = Line::from(Span::styled(fitted, Style::default().fg(right_color)));
    frame.render_widget(Paragraph::new(line).alignment(Alignment::Right), area);
}

fn build_context_line(app: &App, max_width: usize) -> Line<'static> {
    let Some((location_value, branch_value)) = context_values(app, max_width) else {
        return Line::default();
    };

    let mut spans = vec![
        Span::styled("Loc: ", Style::default().fg(theme::DIM)),
        Span::styled(location_value, Style::default().fg(FOOTER_CONTEXT_VALUE)),
    ];

    if let Some(branch_value) = branch_value {
        spans.push(Span::styled(" (", Style::default().fg(theme::DIM)));
        spans.push(Span::styled(branch_value, Style::default().fg(FOOTER_CONTEXT_VALUE)));
        spans.push(Span::styled(")", Style::default().fg(theme::DIM)));
    }

    Line::from(spans)
}

fn context_values(app: &App, max_width: usize) -> Option<(String, Option<String>)> {
    const LOCATION_LABEL_WIDTH: usize = 5;
    const BRANCH_WRAP_WIDTH: usize = 3;

    let location_only_width = max_width.saturating_sub(LOCATION_LABEL_WIDTH);
    let branch = app.git_branch().filter(|branch| !branch.is_empty());

    if let Some(branch) = branch {
        let fixed_width = LOCATION_LABEL_WIDTH + BRANCH_WRAP_WIDTH;
        let available_values = max_width.saturating_sub(fixed_width);
        if available_values >= MIN_CONTEXT_LOCATION_WIDTH + MIN_CONTEXT_BRANCH_WIDTH {
            let branch_width = UnicodeWidthStr::width(branch)
                .min(available_values.saturating_sub(MIN_CONTEXT_LOCATION_WIDTH));
            let branch_value = fit_footer_right_text(branch, branch_width);
            let branch_display_width =
                branch_value.as_ref().map_or(0, |value| UnicodeWidthStr::width(value.as_str()));
            let location_width = available_values.saturating_sub(branch_display_width);
            if let Some(location_value) = fit_location_value(&app.cwd, location_width) {
                return Some((location_value, branch_value));
            }
        }
    }

    fit_location_value(&app.cwd, location_only_width).map(|location_value| (location_value, None))
}

fn fit_location_value(cwd: &str, max_width: usize) -> Option<String> {
    if max_width == 0 {
        return None;
    }

    for candidate in location_candidates(cwd) {
        if UnicodeWidthStr::width(candidate.as_str()) <= max_width {
            return Some(candidate);
        }
    }

    fit_footer_suffix_text(cwd, max_width)
}

fn location_candidates(cwd: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    push_unique(&mut candidates, Some(cwd.to_owned()));
    push_unique(&mut candidates, trailing_path_components(cwd, 2));
    push_unique(&mut candidates, trailing_path_components(cwd, 1));
    candidates
}

fn trailing_path_components(path: &str, count: usize) -> Option<String> {
    let separator = if path.contains('\\') { "\\" } else { "/" };
    let components: Vec<&str> = path
        .split(['/', '\\'])
        .filter(|component| !component.is_empty() && *component != "~")
        .collect();
    if components.is_empty() {
        return None;
    }
    let start = components.len().saturating_sub(count);
    Some(components[start..].join(separator))
}

fn push_unique(candidates: &mut Vec<String>, candidate: Option<String>) {
    let Some(candidate) = candidate else {
        return;
    };
    if !candidate.is_empty() && !candidates.iter().any(|existing| existing == &candidate) {
        candidates.push(candidate);
    }
}

fn fit_footer_suffix_text(text: &str, max_width: usize) -> Option<String> {
    if max_width == 0 || text.trim().is_empty() {
        return None;
    }

    if UnicodeWidthStr::width(text) <= max_width {
        return Some(text.to_owned());
    }

    if max_width <= 3 {
        return Some(".".repeat(max_width));
    }

    let mut fitted = String::new();
    let mut width = 0usize;
    for ch in text.chars().rev() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width.saturating_add(ch_width).saturating_add(3) > max_width {
            break;
        }
        fitted.insert(0, ch);
        width = width.saturating_add(ch_width);
    }

    if fitted.is_empty() {
        return Some("...".to_owned());
    }

    Some(format!("...{fitted}"))
}

fn pending_permission_request_count(app: &App) -> usize {
    app.pending_interaction_ids
        .iter()
        .filter(|tool_id| {
            let Some((mi, bi)) = app.lookup_tool_call(tool_id) else {
                return false;
            };
            matches!(
                app.messages.get(mi).and_then(|msg| msg.blocks.get(bi)),
                Some(MessageBlock::ToolCall(tc)) if tc.pending_permission.is_some()
            )
        })
        .count()
}

fn mcp_needs_auth_count(app: &App) -> usize {
    app.mcp
        .servers
        .iter()
        .filter(|server| {
            matches!(server.status, crate::state::agent_types::McpServerConnectionStatus::NeedsAuth)
        })
        .count()
}

fn should_show_startup_mcp_hint(app: &App) -> bool {
    !app.messages
        .iter()
        .any(|message| matches!(message.role, MessageRole::User | MessageRole::Assistant))
}

fn mode_color(mode_id: &str) -> Color {
    match mode_id {
        "default" => theme::DIM,
        "auto" | "acceptEdits" => Color::Yellow,
        "plan" => Color::Blue,
        "bypassPermissions" | "dontAsk" => Color::Red,
        _ => Color::Magenta,
    }
}

fn fast_mode_badge(state: model::FastModeState) -> (&'static str, Color) {
    match state {
        model::FastModeState::Off => ("FAST:OFF", theme::DIM),
        model::FastModeState::Cooldown => ("FAST:CD", Color::Yellow),
        model::FastModeState::On => ("FAST:ON", theme::RUST_ORANGE),
    }
}

