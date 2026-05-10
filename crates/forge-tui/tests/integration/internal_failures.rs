// =====
// TESTS: 3
// =====
//
// Internal-failure integration tests.
// Validate client event processing + final UI render output for failed tool calls.

use forge_tui::agent::model;
use forge_tui::app::MessageBlock;
use pretty_assertions::assert_eq;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use crate::helpers::test_app;
use crate::message_helpers::{
    assistant_message, send_msg, tool_result_error_block, tool_use_block, user_message,
};

#[tokio::test]
async fn failed_tool_call_with_xml_internal_error_renders_internal_banner_and_summary() {
    let mut app = test_app();
    let tool_id = "tc-xml-internal";
    let xml_payload =
        "<error><code>-32603</code><message>Adapter process crashed</message></error>";

    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            tool_id,
            "Read",
            serde_json::json!({"file_path": "src/lib.rs"}),
        )]),
    );

    send_msg(
        &mut app,
        user_message(vec![tool_result_error_block(tool_id, serde_json::json!(xml_payload))]),
    );

    assert_eq!(tool_call_text_payload(&app, tool_id).as_deref(), Some(xml_payload));

    let frame = render_frame_to_string(&mut app, 120, 36);
    assert!(frame.contains("Internal Agent SDK error"));
    assert!(frame.contains("Adapter process crashed"));
}

#[tokio::test]
async fn failed_tool_call_with_jsonrpc_internal_error_renders_extracted_message() {
    let mut app = test_app();
    let tool_id = "tc-jsonrpc-internal";
    let json_payload =
        r#"{"jsonrpc":"2.0","error":{"code":-32603,"message":"internal rpc fault"}}"#;

    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            tool_id,
            "Read",
            serde_json::json!({"file_path": "src/lib.rs"}),
        )]),
    );

    send_msg(
        &mut app,
        user_message(vec![tool_result_error_block(tool_id, serde_json::json!(json_payload))]),
    );

    let frame = render_frame_to_string(&mut app, 120, 36);
    assert!(frame.contains("Internal Agent SDK error"));
    assert!(frame.contains("internal rpc fault"));
}

#[tokio::test]
async fn failed_tool_call_with_plain_command_error_keeps_normal_rendering() {
    let mut app = test_app();
    let tool_id = "tc-plain-failure";
    let plain_payload = "bash: definitely_not_a_command: command not found";

    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            tool_id,
            "Read",
            serde_json::json!({"file_path": "src/lib.rs"}),
        )]),
    );

    send_msg(
        &mut app,
        user_message(vec![tool_result_error_block(tool_id, serde_json::json!(plain_payload))]),
    );

    let frame = render_frame_to_string(&mut app, 120, 36);
    assert!(!frame.contains("Internal Agent SDK error"));
    assert!(frame.contains("command not found"));
}

fn tool_call_text_payload(app: &forge_tui::app::App, tool_id: &str) -> Option<String> {
    let (mi, bi) = app.tool_call_index.get(tool_id).copied()?;
    let MessageBlock::ToolCall(tc) = &app.messages().get(mi)?.blocks.get(bi)? else {
        return None;
    };
    tc.content.iter().find_map(|content| match content {
        model::ToolCallContent::Content(c) => match &c.content {
            model::ContentBlock::Text(t) => Some(t.text.clone()),
            model::ContentBlock::Image(_) => None,
        },
        _ => None,
    })
}

fn render_frame_to_string(app: &mut forge_tui::app::App, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("create test terminal");
    terminal.draw(|f| forge_tui::ui::render(f, app)).expect("draw frame");

    let mut out = String::new();
    let buffer = terminal.backend().buffer();
    for y in 0..height {
        for x in 0..width {
            out.push_str(buffer[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}
