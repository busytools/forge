//! Integration: mock emits a `PreToolUse` hook request, callback replaces input.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use forge_primitives::Message;
use forge_sdk::{Client, HookContext, HookDecision, HooksBuilder, OptionsBuilder, PreToolUseInput};
use serde_json::json;

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

#[tokio::test]
async fn pre_tool_use_replaces_input() {
    let hooks = HooksBuilder::new()
        .pre_tool_use("Bash", |_input: PreToolUseInput, _ctx: HookContext| async move {
            HookDecision::replace_input(json!({
                "tool_input": {"command": "echo replaced"}
            }))
        })
        .build();

    let opts = OptionsBuilder::new().binary(fixture("mock_claude_hooks.sh")).hooks(hooks).build();

    let (client, mut events) = Client::spawn(opts).await.expect("spawn");
    client.send_user_message("run bash").await.expect("send");

    let msg = events.recv().await.expect("recv").expect("assistant");
    match msg {
        Message::Assistant { message, .. } => {
            assert!(
                message.content.iter().any(
                    |b| matches!(b, forge_primitives::ContentBlock::Text { text } if text.contains("echo replaced"))
                ),
                "expected replaced command in reply, got: {:?}",
                message.content
            );
        }
        other => panic!("unexpected: {other:?}"),
    }
    let _ = events.recv().await;
    client.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn pre_tool_use_deny_propagates() {
    let hooks = HooksBuilder::new()
        .pre_tool_use("Bash", |_input: PreToolUseInput, _ctx: HookContext| async move {
            HookDecision::deny("no bash today")
        })
        .build();

    let opts = OptionsBuilder::new().binary(fixture("mock_claude_hooks.sh")).hooks(hooks).build();

    let (client, mut events) = Client::spawn(opts).await.expect("spawn");
    client.send_user_message("run bash").await.expect("send");

    let msg = events.recv().await.expect("recv").expect("assistant");
    match msg {
        Message::Assistant { message, .. } => {
            assert!(
                message.content.iter().any(
                    |b| matches!(b, forge_primitives::ContentBlock::Text { text } if text.contains("hook denied"))
                ),
                "expected deny text, got: {:?}",
                message.content
            );
        }
        other => panic!("unexpected: {other:?}"),
    }
    let _ = events.recv().await;
    client.disconnect().await.expect("disconnect");
}
