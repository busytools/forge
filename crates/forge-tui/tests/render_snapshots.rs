//! M7.4 — golden-file snapshots of the rendered Buffer.
//!
//! Most snapshots assert content-only — `buffer_to_string` extracts the
//! visible text per row and we snapshot that. Cell coordinates and
//! styling are intentionally excluded so harmless layout tweaks (e.g.
//! adjusting a colour) don't burst every snapshot. One canonical
//! styling-aware snapshot (`app_with_permission_modal_styled`) remains
//! to catch colour/modifier regressions.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use forge_tui::app::{App, Focus, PendingPermission};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn render_to_buffer(app: &App) -> ratatui::buffer::Buffer {
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| forge_tui::ui::render(f, app)).unwrap();
    terminal.backend().buffer().clone()
}

/// Concatenate every cell symbol in the buffer row by row, separated by
/// `\n`. Strips trailing whitespace per line so harmless padding changes
/// don't burst snapshots. Drops styling cells entirely — colour and
/// modifier regressions are covered by the styled snapshot below.
fn buffer_to_string(buf: &ratatui::buffer::Buffer) -> String {
    let area = buf.area();
    let mut out = String::with_capacity(usize::from(area.width) * usize::from(area.height));
    for y in 0..area.height {
        let mut line = String::with_capacity(usize::from(area.width));
        for x in 0..area.width {
            line.push_str(buf[(x, y)].symbol());
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

#[test]
fn empty_app_renders() {
    let app = App::default();
    let buffer = render_to_buffer(&app);
    insta::assert_snapshot!(buffer_to_string(&buffer));
}

#[test]
fn app_with_two_messages_renders() {
    let mut app = App::default();
    app.focus = Focus::Conversation;
    app.current_session = Some("sess_demo".into());
    app.messages.push(serde_json::json!({
        "type": "user",
        "message": { "content": [{ "type": "text", "text": "hello" }] }
    }));
    app.messages.push(serde_json::json!({
        "type": "assistant",
        "message": { "content": [{ "type": "text", "text": "hi back" }] }
    }));
    let buffer = render_to_buffer(&app);
    insta::assert_snapshot!(buffer_to_string(&buffer));
}

#[test]
fn app_with_permission_modal_renders() {
    let mut app = App::default();
    app.focus = Focus::PermissionModal;
    app.pending_permission = Some(PendingPermission::new(
        serde_json::json!("rev_test"),
        serde_json::json!({
            "tool_name": "Bash",
            "tool_input": {"command": "ls -la"},
        }),
        None,
    ));
    let buffer = render_to_buffer(&app);
    insta::assert_snapshot!(buffer_to_string(&buffer));
}

/// Styling-aware canonical snapshot — keeps colour/modifier regressions
/// visible. Only kept on this one case so the rest can stay content-only.
///
/// NOTE: this snapshot is keyed on cell coordinates, so any layout
/// shift (modal width/height, padding, border characters) bursts it.
/// Recovery is `cargo insta accept` after manually verifying the new
/// rendering looks right. If layout churn becomes a chronic
/// maintenance burden, refactor to walk cells and assert
/// `(symbol, fg, bg, modifier)` tuples by content rather than by
/// `(x, y)`.
#[test]
fn app_with_permission_modal_styled() {
    let mut app = App::default();
    app.focus = Focus::PermissionModal;
    app.pending_permission = Some(PendingPermission::new(
        serde_json::json!("rev_test"),
        serde_json::json!({
            "tool_name": "Bash",
            "tool_input": {"command": "ls -la"},
        }),
        None,
    ));
    let buffer = render_to_buffer(&app);
    insta::assert_debug_snapshot!(buffer);
}

#[test]
fn app_with_session_list_renders() {
    let mut app = App::default();
    app.session_list = vec![
        serde_json::json!({"session_id": "sess_a", "summary": "First session"}),
        serde_json::json!({"session_id": "sess_b", "summary": "Second session"}),
    ];
    app.session_list_cursor = 1;
    let buffer = render_to_buffer(&app);
    insta::assert_snapshot!(buffer_to_string(&buffer));
}
