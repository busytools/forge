//! Integration test: the CLI emits two `system/transcript_mirror` frames
//! while `--session-mirror` is active. The SDK should swallow them (they
//! must not surface via `next_event`) and append the decoded entries to
//! the attached [`MemorySessionStore`].

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use forge_sdk::Message;
use forge_sdk::session::store::{MemorySessionStore, SessionKey, SessionStore};
use forge_sdk::{Client, OptionsBuilder};

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

#[tokio::test]
async fn memory_session_store_receives_mirrored_entries_via_client() {
    let store = Arc::new(MemorySessionStore::new());
    let store_handle: Arc<dyn SessionStore> = store.clone();

    let opts = OptionsBuilder::new()
        .binary(fixture("mock_claude_transcript_mirror.sh"))
        .session_store_arc(store_handle)
        .projects_dir("/tmp/forge-mirror-root")
        .build();

    let mut client = Client::spawn(opts).await.expect("spawn");
    client.send_user_message("mirror me").await.expect("send");

    // Drain until we see the Result — the transcript_mirror frames must
    // never surface here (Client swallows them).
    let mut saw_assistant = false;
    let mut saw_result = false;
    while let Some(msg) = client.next_event().await.expect("next") {
        match msg {
            Message::Assistant { .. } => saw_assistant = true,
            Message::Result { .. } => {
                saw_result = true;
                break;
            }
            Message::System { subtype, .. } => {
                panic!(
                    "system/{subtype} frame unexpectedly surfaced (transcript_mirror must be swallowed)"
                );
            }
            Message::User { .. } => panic!("unexpected user event in mirror test"),
            Message::RateLimitEvent { .. } => {
                panic!("unexpected rate-limit event in mirror test")
            }
            Message::TaskStarted { .. }
            | Message::TaskProgress { .. }
            | Message::TaskNotification { .. } => {
                panic!("unexpected task lifecycle frame in mirror test")
            }
            Message::MirrorError { .. } => {
                panic!("unexpected mirror_error frame in mirror test")
            }
            Message::StreamEvent { .. } => panic!("unexpected stream_event in mirror test"),
            Message::Error { .. } => panic!("unexpected error frame in mirror test"),
        }
    }
    assert!(saw_assistant, "expected an assistant turn before result");
    assert!(saw_result, "expected a result frame before stream end");

    client.disconnect().await.expect("disconnect");

    let key = SessionKey {
        project_key: "proj_mock".into(),
        session_id: "mock-mirror-001".into(),
        subpath: None,
    };
    let entries = store
        .load(&key)
        .await
        .expect("load")
        .expect("entries present");

    assert_eq!(
        entries.len(),
        2,
        "expected two mirrored entries, got {entries:?}"
    );
    assert_eq!(entries[0].ty, "user", "first mirrored entry type mismatch");
    assert_eq!(
        entries[1].ty, "assistant",
        "second mirrored entry type mismatch"
    );
}
