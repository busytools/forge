//! End-to-end tests of the `can_use_tool` callback flow.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use forge_sdk::messages::Message;
use forge_sdk::{Client, OptionsBuilder, PermissionDecision, ToolPermissionContext};
use serde_json::json;

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

#[tokio::test]
async fn allow_path_completes_turn() {
    let call_count = Arc::new(AtomicUsize::new(0));
    let cc = call_count.clone();
    let opts = OptionsBuilder::new()
        .binary(fixture("mock_claude_permission.sh"))
        .can_use_tool(move |_ctx: ToolPermissionContext| {
            let cc = cc.clone();
            async move {
                cc.fetch_add(1, Ordering::SeqCst);
                PermissionDecision::allow()
            }
        })
        .build();
    let mut client = Client::spawn(opts).await.expect("spawn");
    client.send_user_message("edit please").await.expect("send");

    // First visible event: the assistant turn AFTER the control round-trip.
    let msg = client.next_event().await.expect("next").expect("assistant");
    match msg {
        Message::Assistant { message, .. } => {
            assert!(
                message.content.iter().any(
                    |b| matches!(b, forge_sdk::content::ContentBlock::Text { text } if text.contains("edited"))
                ),
                "expected 'edited' text, got: {:?}",
                message.content
            );
        }
        other => panic!("expected Assistant, got: {other:?}"),
    }

    let msg = client.next_event().await.expect("next").expect("result");
    assert!(matches!(msg, Message::Result { .. }));

    client.disconnect().await.expect("disconnect");
    assert_eq!(
        call_count.load(Ordering::SeqCst),
        1,
        "callback should have fired exactly once"
    );
}

#[tokio::test]
async fn deny_path_completes_turn_with_denial_text() {
    let opts = OptionsBuilder::new()
        .binary(fixture("mock_claude_permission.sh"))
        .can_use_tool(|ctx: ToolPermissionContext| async move {
            PermissionDecision::deny(format!("cannot touch {}", ctx.tool_input["file_path"]))
        })
        .build();
    let mut client = Client::spawn(opts).await.expect("spawn");
    client.send_user_message("edit please").await.expect("send");

    let msg = client.next_event().await.expect("next").expect("assistant");
    match msg {
        Message::Assistant { message, .. } => {
            assert!(
                message.content.iter().any(
                    |b| matches!(b, forge_sdk::content::ContentBlock::Text { text } if text.contains("denied"))
                ),
                "expected 'denied' text when callback denies, got: {:?}",
                message.content
            );
        }
        other => panic!("expected Assistant, got: {other:?}"),
    }
    let _ = client.next_event().await.expect("next");
    client.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn allow_with_updated_input_propagates() {
    let opts = OptionsBuilder::new()
        .binary(fixture("mock_claude_permission.sh"))
        .can_use_tool(|_ctx: ToolPermissionContext| async move {
            PermissionDecision::allow_with_input(json!({"file_path": "/tmp/redirected.txt"}))
        })
        .build();
    let mut client = Client::spawn(opts).await.expect("spawn");
    client.send_user_message("edit please").await.expect("send");

    let msg = client.next_event().await.expect("next").expect("assistant");
    match msg {
        Message::Assistant { message, .. } => {
            assert!(
                message.content.iter().any(
                    |b| matches!(b, forge_sdk::content::ContentBlock::Text { text } if text.contains("redirected"))
                ),
                "expected redirected path in reply, got: {:?}",
                message.content
            );
        }
        other => panic!("expected Assistant, got: {other:?}"),
    }
    let _ = client.next_event().await.expect("next");
    client.disconnect().await.expect("disconnect");
}
