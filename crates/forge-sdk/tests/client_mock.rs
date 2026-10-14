//! End-to-end test of `Client` against `mock_claude.sh`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use forge_primitives::Message;
use forge_sdk::{Client, OptionsBuilder};

fn mock_binary_path() -> String {
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mock_claude.sh").into()
}

#[tokio::test]
async fn spawn_captures_session_id() {
    let opts = OptionsBuilder::new().binary(mock_binary_path()).build();
    let (client, _events) = Client::spawn(opts).await.expect("spawn");
    assert_eq!(client.session_id(), "mock-session-001");
    client.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn send_and_receive_full_turn() {
    let opts = OptionsBuilder::new().binary(mock_binary_path()).build();
    let (client, mut events) = Client::spawn(opts).await.expect("spawn");
    client.send_user_message("hi").await.expect("send");

    let msg = events.recv().await.expect("recv").expect("assistant");
    match msg {
        Message::Assistant { message, .. } => {
            assert_eq!(message.content.len(), 1);
        }
        other => panic!("expected Assistant, got: {other:?}"),
    }

    let msg = events.recv().await.expect("recv").expect("result");
    assert!(matches!(msg, Message::Result { .. }));

    client.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn disconnect_after_send_does_not_hang() {
    let opts = OptionsBuilder::new().binary(mock_binary_path()).build();
    let (client, mut events) = Client::spawn(opts).await.expect("spawn");
    client.send_user_message("hi").await.expect("send");
    // Drain.
    let _ = events.recv().await;
    let _ = events.recv().await;

    // Must complete within a reasonable window - regression guard for the
    // anyio-style cancel-scope spin bug seen in a Python equivalent.
    tokio::time::timeout(std::time::Duration::from_secs(2), client.disconnect())
        .await
        .expect("disconnect timed out")
        .expect("disconnect ok");
}

/// A CLI that spawns but never answers the initialize handshake must
/// fail `Client::spawn` at the 60s init budget rather than parking the
/// caller (and leaking the child) forever. `start_paused` auto-advances
/// the virtual clock while the runtime is idle on the wedged read, so
/// the 60s budget elapses without real wall-clock cost - and that the
/// budget ran on the VIRTUAL clock is the thing pinned, because an
/// absolute real-time bound is a statement about the machine: measured
/// 2026-10-14, a loaded gate ran this spawn's real time to 12 s, and a
/// 10 s real bound called that a failure.
#[tokio::test(start_paused = true)]
async fn wedged_init_handshake_times_out() {
    let wedged = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mock_claude_wedged_init.sh");
    let opts = OptionsBuilder::new().binary(wedged).build();

    let real_started = std::time::Instant::now();
    let virtual_started = tokio::time::Instant::now();
    let err = Client::spawn(opts).await.expect_err("wedged init must fail, not hang");
    let forge_sdk::Error::Connection { reason } = err else {
        panic!("expected a connection error, got {err:?}");
    };
    assert!(reason.contains("timed out"), "the reason names the handshake timeout: {reason}");
    assert!(
        virtual_started.elapsed() >= forge_sdk::INIT_HANDSHAKE_TIMEOUT,
        "the budget is spent on the virtual clock, not the machine's: {:?}",
        virtual_started.elapsed(),
    );
    assert!(
        real_started.elapsed() < forge_sdk::INIT_HANDSHAKE_TIMEOUT / 2,
        "and the real wait stays materially under the budget: {:?}",
        real_started.elapsed(),
    );
}
