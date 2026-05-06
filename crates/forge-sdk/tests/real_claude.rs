//! Smoke test against the real `claude` binary.
//!
//! Skipped unless `claude` is on PATH.



#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use forge_sdk::Message;
use forge_sdk::{Client, OptionsBuilder};

fn have_claude() -> bool {
    std::process::Command::new("claude").arg("--version").output().is_ok()
}

#[tokio::test]
#[ignore = "requires `claude` on PATH; run with `cargo nextest run --run-ignored all`"]
async fn real_claude_minimal_turn() {
    if !have_claude() {
        eprintln!("skipping: `claude` not on PATH");
        return;
    }

    let opts =
        OptionsBuilder::new().permission_mode(forge_sdk::PermissionMode::BypassPermissions).build();
    let (client, mut events) = Client::spawn(opts).await.expect("spawn real claude");
    assert!(!client.session_id().is_empty(), "session id should be captured");

    client
        .send_user_message("Reply with exactly the word 'pong' and nothing else.")
        .await
        .expect("send");

    // Drain until we see a Result message.
    let mut saw_assistant = false;
    loop {
        let item = tokio::time::timeout(std::time::Duration::from_secs(60), events.recv())
            .await
            .expect("timeout waiting for event");
        let Some(msg) = item else {
            panic!("stream ended before Result");
        };
        let msg = msg.expect("read event");
        match msg {
            Message::Assistant { .. } => saw_assistant = true,
            Message::Result { .. } => break,
            _ => {}
        }
    }
    assert!(saw_assistant, "expected at least one Assistant before Result");

    client.disconnect().await.expect("disconnect");
}
