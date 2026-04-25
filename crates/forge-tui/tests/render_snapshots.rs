//! M7.4 — golden-file snapshots of the rendered Buffer.

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

#[test]
fn empty_app_renders() {
    let app = App::default();
    let buffer = render_to_buffer(&app);
    insta::assert_debug_snapshot!(buffer);
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
    insta::assert_debug_snapshot!(buffer);
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
    insta::assert_debug_snapshot!(buffer);
}
