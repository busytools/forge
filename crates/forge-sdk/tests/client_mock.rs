//! End-to-end test of `Client` against `mock_claude.sh`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forge_sdk::Message;
use forge_sdk::{Client, OptionsBuilder};

fn mock_binary_path() -> String {
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mock_claude.sh").into()
}

#[tokio::test]
async fn spawn_captures_session_id() {
    let opts = OptionsBuilder::new().binary(mock_binary_path()).build();
    let client = Client::spawn(opts).await.expect("spawn");
    assert_eq!(client.session_id(), "mock-session-001");
    client.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn send_and_receive_full_turn() {
    let opts = OptionsBuilder::new().binary(mock_binary_path()).build();
    let mut client = Client::spawn(opts).await.expect("spawn");
    client.send_user_message("hi").await.expect("send");

    let msg = client.next_event().await.expect("next").expect("assistant");
    match msg {
        Message::Assistant { message, .. } => {
            assert_eq!(message.content.len(), 1);
        }
        other => panic!("expected Assistant, got: {other:?}"),
    }

    let msg = client.next_event().await.expect("next").expect("result");
    assert!(matches!(msg, Message::Result { .. }));

    client.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn disconnect_after_send_does_not_hang() {
    let opts = OptionsBuilder::new().binary(mock_binary_path()).build();
    let mut client = Client::spawn(opts).await.expect("spawn");
    client.send_user_message("hi").await.expect("send");
    // Drain.
    let _ = client.next_event().await;
    let _ = client.next_event().await;

    // Must complete within a reasonable window — regression guard for the
    // anyio-style cancel-scope spin bug Python architect hit.
    tokio::time::timeout(std::time::Duration::from_secs(2), client.disconnect())
        .await
        .expect("disconnect timed out")
        .expect("disconnect ok");
}
