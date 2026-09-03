//! Integration tests for the 9 outbound `control_request` subtypes on
//! [`Client`]. Each test spawns the minimal `mock_claude_control.sh`
//! fixture, invokes the corresponding method, and asserts the round-trip
//! either returned `Ok(())` or the expected decoded payload - and that
//! the SUBTYPE the CLI actually observed matches what the wire shape
//! promises, via the mock's `FORGED_MOCK_ECHO_SUBTYPE` hook.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use forge_sdk::{Client, OptionsBuilder, PermissionMode};

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

/// Spawn the control mock with the subtype echo pointed at `echo`.
async fn spawn_client(echo: &std::path::Path) -> Client {
    let opts = OptionsBuilder::new()
        .binary(fixture("mock_claude_control.sh"))
        .env("FORGED_MOCK_ECHO_SUBTYPE", echo.to_string_lossy().as_ref())
        .build();
    // This test exercises the writer-side control-request round-trips
    // and never reads events; drop the events receiver immediately.
    let (client, _events) = Client::spawn(opts).await.expect("spawn");
    client
}

/// The subtypes the mock observed, in order. The initialize handshake
/// is always the first entry.
fn observed_subtypes(echo: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(echo)
        .expect("the mock echoes observed subtypes to the file")
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Assert the round-trip's LAST observed subtype is `expected` (the
/// echo also records the init handshake's `initialize`).
fn assert_last_subtype(echo: &std::path::Path, expected: &str) {
    let observed = observed_subtypes(echo);
    assert_eq!(
        observed.last().map(String::as_str),
        Some(expected),
        "the CLI observed subtype {observed:?} - the wire subtype must match"
    );
}

#[tokio::test]
async fn interrupt_round_trip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let echo = dir.path().join("echo");
    let client = spawn_client(&echo).await;
    client.interrupt().await.expect("interrupt");
    assert_last_subtype(&echo, "interrupt");
    client.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn set_permission_mode_round_trip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let echo = dir.path().join("echo");
    let client = spawn_client(&echo).await;
    client.set_permission_mode(PermissionMode::AcceptEdits).await.expect("set_permission_mode");
    assert_last_subtype(&echo, "set_permission_mode");
    client.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn mcp_reconnect_round_trip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let echo = dir.path().join("echo");
    let client = spawn_client(&echo).await;
    client.mcp_reconnect("my-server").await.expect("mcp_reconnect");
    assert_last_subtype(&echo, "mcp_reconnect");
    client.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn mcp_toggle_round_trip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let echo = dir.path().join("echo");
    let client = spawn_client(&echo).await;
    client.mcp_toggle("my-server", false).await.expect("mcp_toggle");
    assert_last_subtype(&echo, "mcp_toggle");
    client.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn stop_task_round_trip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let echo = dir.path().join("echo");
    let client = spawn_client(&echo).await;
    client.stop_task("task_abc").await.expect("stop_task");
    assert_last_subtype(&echo, "stop_task");
    client.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn mcp_status_raw_returns_canned_payload() {
    let dir = tempfile::tempdir().expect("tempdir");
    let echo = dir.path().join("echo");
    let client = spawn_client(&echo).await;
    let resp = client.mcp_status_raw().await.expect("mcp_status");
    assert_eq!(resp, serde_json::json!({"servers": []}), "mcp_status payload mismatch");
    assert_last_subtype(&echo, "mcp_status");
    client.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn get_context_usage_raw_returns_canned_payload() {
    let dir = tempfile::tempdir().expect("tempdir");
    let echo = dir.path().join("echo");
    let client = spawn_client(&echo).await;
    let resp = client.get_context_usage_raw().await.expect("get_context_usage");
    assert_eq!(
        resp,
        serde_json::json!({"used": 0, "budget": 200_000}),
        "get_context_usage payload mismatch"
    );
    assert_last_subtype(&echo, "get_context_usage");
    client.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn set_model_round_trip_with_model() {
    let dir = tempfile::tempdir().expect("tempdir");
    let echo = dir.path().join("echo");
    let client = spawn_client(&echo).await;
    client.set_model(Some("claude-sonnet-4-6")).await.expect("set_model");
    assert_last_subtype(&echo, "set_model");
    client.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn set_model_round_trip_with_none_reverts_to_default() {
    let dir = tempfile::tempdir().expect("tempdir");
    let echo = dir.path().join("echo");
    let client = spawn_client(&echo).await;
    // Python accepts `model=None` to revert to CLI default; forge-sdk
    // passes Option<&str>, so None serialises to JSON null.
    client.set_model(None).await.expect("set_model");
    assert_last_subtype(&echo, "set_model");
    client.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn get_server_info_returns_cached_initialize_payload() {
    // The mock replies to `initialize` with a canned
    // `{"commands": [...], "outputStyle": "default"}` body. Client::spawn
    // stores it so get_server_info() surfaces the payload later
    // without re-issuing the handshake (mirrors Python
    // ClaudeSDKClient.get_server_info - client.py:541-564).
    let dir = tempfile::tempdir().expect("tempdir");
    let echo = dir.path().join("echo");
    let client = spawn_client(&echo).await;
    let info = client.get_server_info().expect("initialize payload cached");
    assert_eq!(info["outputStyle"], "default");
    assert!(info["commands"].is_array(), "expected commands array");
    client.disconnect().await.expect("disconnect");
}
