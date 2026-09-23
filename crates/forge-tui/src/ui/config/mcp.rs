use super::overlay::{
    OverlayChrome, OverlayLayoutSpec, overlay_line_style, render_overlay_separator,
    render_overlay_shell,
};
use super::theme;
use crate::app::App;
use crate::app::config::available_mcp_actions;
use crate::app::extensions::skills::{mcp_status_label, transport_label};
use forge_primitives::{McpServerConnectionStatus, McpServerStatus};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Wrap};
use serde_json::Value;

pub(super) fn render_details_overlay(frame: &mut Frame, area: Rect, app: &App) {
    let Some(overlay) = app.config.mcp_details_overlay() else {
        return;
    };

    let server = app
        .mcp()
        .and_then(|mcp| mcp.servers.iter().find(|server| server.name == overlay.server_name));
    let action_lines = server.map_or_else(Vec::new, |server| mcp_action_lines(server, overlay));
    let rendered = render_overlay_shell(
        frame,
        area,
        OverlayLayoutSpec {
            min_width: 72,
            min_height: 12,
            width_percent: 78,
            height_percent: 82,
            preferred_height: 24,
            fullscreen_below: Some((80, 18)),
            inner_margin: Margin { vertical: 1, horizontal: 2 },
        },
        OverlayChrome {
            title: overlay.server_name.as_str(),
            subtitle: None,
            help: Some("Up/Down select | Enter run | Esc cancel"),
        },
    );

    if action_lines.is_empty() {
        let body = server.map_or_else(server_missing_lines, server_detail_lines);
        frame.render_widget(Paragraph::new(body).wrap(Wrap { trim: false }), rendered.body_area);
        return;
    }

    let action_height = wrapped_height(Text::from(action_lines.clone()), rendered.body_area.width);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1), Constraint::Length(action_height)])
        .split(rendered.body_area);

    let body = server.map_or_else(server_missing_lines, server_detail_lines);
    frame.render_widget(Paragraph::new(body).wrap(Wrap { trim: false }), sections[0]);
    render_overlay_separator(frame, sections[1]);
    frame.render_widget(Paragraph::new(action_lines).wrap(Wrap { trim: false }), sections[2]);
}

fn server_detail_lines(server: &McpServerStatus) -> Vec<Line<'static>> {
    let mut lines = vec![
        section_heading("Status"),
        detail_kv("Status", mcp_status_label(server.status), status_color(server.status)),
        detail_kv(
            "Enabled",
            if matches!(server.status, McpServerConnectionStatus::Disabled) { "No" } else { "Yes" },
            Color::White,
        ),
        detail_kv("Scope", server.scope.as_deref().unwrap_or("session"), Color::White),
        detail_kv("Transport", transport_label(server.config.as_ref()), Color::White),
        detail_kv(
            "Tools",
            &crate::ui::format::tool_summary(server.tools.as_deref().map_or(0, <[_]>::len)),
            Color::White,
        ),
    ];

    if let Some(info) = server.server_info.as_ref() {
        lines.push(detail_kv("Server name", &info.name, Color::White));
        lines.push(detail_kv("Version", &info.version, Color::White));
    }

    if let Some(config) = server.config.as_ref() {
        lines.push(Line::default());
        lines.push(section_heading("Configuration"));
        lines.extend(config_lines(config));
    }

    if let Some(error) = server.error.as_deref() {
        lines.push(Line::default());
        lines.push(section_heading("Error"));
        lines.push(detail_value(error, theme::STATUS_ERROR));
    }

    lines
}

fn server_missing_lines() -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled(
            "The selected server is no longer present in the latest MCP snapshot.",
            Style::default().fg(theme::DIM),
        )),
        Line::default(),
        Line::from(Span::styled(
            "Close this overlay and refresh the MCP list.",
            Style::default().fg(theme::DIM),
        )),
    ]
}

fn mcp_action_lines(
    server: &McpServerStatus,
    overlay: &crate::app::config::McpDetailsOverlayState,
) -> Vec<Line<'static>> {
    let actions = available_mcp_actions(server);
    if actions.is_empty() {
        return vec![detail_value("No actions available.", theme::DIM)];
    }

    let mut lines = vec![section_heading("Actions"), Line::default()];
    for (index, action) in actions.into_iter().enumerate() {
        let selected = index == overlay.selected_index;
        let spans = vec![Span::styled(
            format!("{} {}", if selected { ">" } else { " " }, action.label()),
            overlay_line_style(selected, true),
        )];
        lines.push(Line::from(spans));
    }
    lines
}

/// Render the configuration block for a single MCP server. The
/// SDK exposes the config as `Option<serde_json::Value>` because
/// the CLI accepts variants forge-sdk doesn't model
/// (e.g. `claudeai-proxy`); we extract fields by `type` discriminator
/// inline rather than carrying a typed mirror enum across the
/// bridge.
fn config_lines(config: &Value) -> Vec<Line<'static>> {
    let kind = config.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "stdio" => {
            let command = config.get("command").and_then(Value::as_str).unwrap_or("(missing)");
            let args = config
                .get("args")
                .and_then(Value::as_array)
                .map(|arr| arr.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "(none)".to_owned());
            let env_count =
                config.get("env").and_then(Value::as_object).map_or(0, serde_json::Map::len);
            vec![
                detail_kv("Command", command, Color::White),
                detail_kv("Args", &args, Color::White),
                detail_kv("Env", &format!("{env_count} variable(s)"), Color::White),
            ]
        }
        "sse" | "http" => {
            let url = config.get("url").and_then(Value::as_str).unwrap_or("(missing)");
            let header_count =
                config.get("headers").and_then(Value::as_object).map_or(0, serde_json::Map::len);
            vec![
                detail_kv("URL", url, Color::White),
                detail_kv("Headers", &format!("{header_count} configured"), Color::White),
            ]
        }
        "sdk" => {
            let name = config.get("name").and_then(Value::as_str).unwrap_or("(missing)");
            vec![detail_kv("SDK server", name, Color::White)]
        }
        "claudeai-proxy" => {
            let url = config.get("url").and_then(Value::as_str).unwrap_or("(missing)");
            let id = config.get("id").and_then(Value::as_str).unwrap_or("(missing)");
            vec![detail_kv("Proxy URL", url, Color::White), detail_kv("Proxy ID", id, Color::White)]
        }
        _ => Vec::new(),
    }
}

fn detail_kv(key: &str, value: &str, value_color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key}: "), Style::default().fg(theme::DIM)),
        Span::styled(value.to_owned(), Style::default().fg(value_color)),
    ])
}

fn detail_value(value: &str, color: Color) -> Line<'static> {
    Line::from(Span::styled(value.to_owned(), Style::default().fg(color)))
}

fn section_heading(title: &str) -> Line<'static> {
    Line::from(Span::styled(
        title.to_owned(),
        Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD),
    ))
}

fn wrapped_height(text: Text<'static>, width: u16) -> u16 {
    u16::try_from(Paragraph::new(text).wrap(Wrap { trim: false }).line_count(width))
        .unwrap_or(u16::MAX)
        .max(1)
}

fn status_color(status: McpServerConnectionStatus) -> Color {
    match status {
        McpServerConnectionStatus::Connected => theme::RUST_ORANGE,
        McpServerConnectionStatus::NeedsAuth => theme::STATUS_WARNING,
        McpServerConnectionStatus::Pending => Color::Cyan,
        McpServerConnectionStatus::Disabled => Color::DarkGray,
        McpServerConnectionStatus::Failed => theme::STATUS_ERROR,
    }
}
