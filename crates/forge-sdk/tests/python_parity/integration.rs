//! Mirrors `tests/test_integration.py` from `claude-agent-sdk-python`
//! v0.1.64.
//!
//! Port of all 5 upstream cases from `TestIntegration`. Python uses
//! `unittest.mock` to patch `SubprocessCLITransport` and inject a
//! canned message stream; forge-sdk exercises the same end-to-end
//! shapes either via the shipped `mock_claude.sh` binary or by
//! driving the codec / options directly.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forge_sdk::transport::codec::decode_line;
use forge_sdk::{Client, ContentBlock, Error, Message, OptionsBuilder, query};

fn mock_binary_path() -> String {
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mock_claude.sh").into()
}

/// Ported from `test_simple_query_response`. `query()` yields at
/// least one assistant message followed by a result frame.
#[tokio::test]
async fn simple_query_response() {
    let opts = OptionsBuilder::new().binary(mock_binary_path()).build();
    let messages = query("What is 2 + 2?", Some(opts)).await.expect("query");
    // Must see an Assistant followed by a Result (in order).
    let assistant_idx = messages
        .iter()
        .position(|m| matches!(m, Message::Assistant { .. }))
        .expect("Assistant message present");
    let result_idx = messages
        .iter()
        .rposition(|m| matches!(m, Message::Result { .. }))
        .expect("Result message present");
    assert!(assistant_idx < result_idx);
    let Message::Result {
        total_cost_usd,
        session_id,
        ..
    } = &messages[result_idx]
    else {
        unreachable!()
    };
    assert!(total_cost_usd.is_some());
    assert!(!session_id.is_empty());
}

/// Ported from `test_query_with_tool_use`. An assistant turn with
/// mixed text + `tool_use` content parses both blocks.
#[test]
fn query_with_tool_use() {
    let wire = r#"{"type":"assistant","session_id":"test-session-2","message":{"id":"msg_01","role":"assistant","model":"claude-opus-4-1-20250805","content":[{"type":"text","text":"Let me read that file for you."},{"type":"tool_use","id":"tool-123","name":"Read","input":{"file_path":"/test.txt"}}]}}"#;
    let msg = decode_line(wire, 1).expect("parse");
    let Message::Assistant { message, .. } = msg else {
        panic!("expected Assistant");
    };
    assert_eq!(message.content.len(), 2);
    let ContentBlock::Text { text } = &message.content[0] else {
        panic!("expected text block");
    };
    assert_eq!(text, "Let me read that file for you.");
    let ContentBlock::ToolUse { name, input, .. } = &message.content[1] else {
        panic!("expected tool_use block");
    };
    assert_eq!(name, "Read");
    assert_eq!(
        input.get("file_path").and_then(serde_json::Value::as_str),
        Some("/test.txt")
    );
}

/// Ported from `test_cli_not_found`. Pointing `binary` at a path
/// that doesn't exist surfaces `Error::CliNotFound`, mirroring
/// Python's `CLINotFoundError`.
#[tokio::test]
async fn cli_not_found() {
    let opts = OptionsBuilder::new()
        .binary("/nonexistent/path/to/claude")
        .build();
    let err = Client::spawn(opts).await.expect_err("must reject");
    match err {
        Error::CliNotFound { binary } => {
            assert_eq!(binary, "/nonexistent/path/to/claude");
        }
        Error::Connection { .. } => {
            // minimum_cli_version probe path — also acceptable; the
            // `--version` probe surfaces a Connection error before the
            // spawn proper would fire CliNotFound.
        }
        other => panic!("expected CliNotFound or Connection, got {other:?}"),
    }
}

/// Ported from `test_continuation_option`. `continue_conversation =
/// true` is preserved on the built `Options` and flows into argv.
/// The argv-level assertion is owned by
/// `argv_composition.rs::continue_conversation_flag_lands`; here we
/// verify the builder + field round-trip.
#[test]
fn continuation_option() {
    let opts = OptionsBuilder::new().continue_conversation(true).build();
    assert!(opts.continue_conversation);
}

/// Ported from `test_max_budget_usd_option`. Two assertions:
/// (a) the `max_budget_usd` option survives the builder, and
/// (b) `error_max_budget_usd` parses as a `Result` variant with
/// `is_error: false` (the budget is a graceful stop, not an error).
#[test]
fn max_budget_usd_option() {
    let opts = OptionsBuilder::new().max_budget_usd(0.0001).build();
    assert_eq!(opts.max_budget_usd, Some(0.0001));

    let wire = r#"{"type":"result","subtype":"error_max_budget_usd","duration_ms":500,"duration_api_ms":400,"is_error":false,"num_turns":1,"session_id":"test-session-budget","total_cost_usd":0.0002,"usage":{"input_tokens":100,"output_tokens":50}}"#;
    let msg = decode_line(wire, 1).expect("parse");
    let Message::Result {
        subtype,
        is_error,
        total_cost_usd,
        ..
    } = msg
    else {
        panic!("expected Result");
    };
    assert_eq!(subtype, "error_max_budget_usd");
    assert!(!is_error);
    assert_eq!(total_cost_usd, Some(0.0002));
}
