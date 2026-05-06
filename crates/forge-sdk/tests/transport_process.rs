//! Integration test for `transport::process::Subprocess` against the mock binary.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use forge_sdk::Options;
use forge_sdk::transport::process::Subprocess;

fn mock_binary_path() -> String {
    // Use the raw mock that doesn't expect an initialize handshake —
    // transport_process tests exercise Subprocess directly, below
    // Client's initialize round-trip.
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mock_claude_raw.sh").into()
}

#[tokio::test]
async fn spawn_reads_init_line() {
    let mut opts = Options::default();
    opts.binary = mock_binary_path();

    let mut sub = Subprocess::spawn(&opts).await.expect("spawn");
    let line = sub.read_line().await.expect("read").expect("init line present");
    assert!(line.contains("\"type\":\"system\""), "expected init system line, got: {line}");
    sub.close().await.expect("close");
}

#[tokio::test]
async fn send_and_read_roundtrip() {
    let mut opts = Options::default();
    opts.binary = mock_binary_path();

    let mut sub = Subprocess::spawn(&opts).await.expect("spawn");
    // Drop the init line.
    let _init = sub.read_line().await.expect("read").expect("init");

    sub.write_line("any input\n").await.expect("write");

    let assistant = sub.read_line().await.expect("read").expect("assistant");
    assert!(assistant.contains("\"type\":\"assistant\""));

    let result = sub.read_line().await.expect("read").expect("result");
    assert!(result.contains("\"type\":\"result\""));

    sub.close().await.expect("close");
}

#[tokio::test]
async fn spawn_rejects_missing_binary() {
    let mut opts = Options::default();
    opts.binary = "/definitely/does/not/exist/claude".into();
    let err = Subprocess::spawn(&opts).await.expect_err("should fail");
    let rendered = format!("{err}");
    assert!(
        rendered.contains("not found") || rendered.contains("No such file"),
        "expected missing-binary error, got: {rendered}"
    );
}
