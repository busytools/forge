//! Integration tests for the 9 outbound `control_request` subtypes on
//! [`Client`]. Each test spawns the minimal `mock_claude_control.sh`
//! fixture, invokes the corresponding method, and asserts the round-trip
//! either returned `Ok(())` or the expected decoded payload.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forge_sdk::{Client, OptionsBuilder, PermissionMode};

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

async fn spawn_client() -> Client {
    let opts = OptionsBuilder::new()
        .binary(fixture("mock_claude_control.sh"))
        .build();
    Client::spawn(opts).await.expect("spawn")
}

#[tokio::test]
async fn interrupt_round_trip() {
    let mut client = spawn_client().await;
    client.interrupt().await.expect("interrupt");
    client.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn set_permission_mode_round_trip() {
    let mut client = spawn_client().await;
    client
        .set_permission_mode(PermissionMode::AcceptEdits)
        .await
        .expect("set_permission_mode");
    client.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn rewind_files_round_trip() {
    let mut client = spawn_client().await;
    client
        .rewind_files("msg_user_01")
        .await
        .expect("rewind_files");
    client.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn mcp_reconnect_round_trip() {
    let mut client = spawn_client().await;
    client
        .mcp_reconnect("my-server")
        .await
        .expect("mcp_reconnect");
    client.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn mcp_toggle_round_trip() {
    let mut client = spawn_client().await;
    client
        .mcp_toggle("my-server", false)
        .await
        .expect("mcp_toggle");
    client.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn stop_task_round_trip() {
    let mut client = spawn_client().await;
    client.stop_task("task_abc").await.expect("stop_task");
    client.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn mcp_status_returns_canned_payload() {
    let mut client = spawn_client().await;
    let resp = client.mcp_status().await.expect("mcp_status");
    assert_eq!(
        resp,
        serde_json::json!({"servers": []}),
        "mcp_status payload mismatch"
    );
    client.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn get_context_usage_returns_canned_payload() {
    let mut client = spawn_client().await;
    let resp = client.get_context_usage().await.expect("get_context_usage");
    assert_eq!(
        resp,
        serde_json::json!({"used": 0, "budget": 200_000}),
        "get_context_usage payload mismatch"
    );
    client.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn fork_session_returns_new_session_id() {
    let mut client = spawn_client().await;
    let new_session = client
        .fork_session(Some("toolu_split_01"))
        .await
        .expect("fork_session");
    assert_eq!(new_session, "forked-123", "fork_session id mismatch");
    client.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn fork_session_without_tool_use_id_returns_new_session_id() {
    let mut client = spawn_client().await;
    let new_session = client.fork_session(None).await.expect("fork_session");
    assert_eq!(
        new_session, "forked-123",
        "fork_session (no tool_use_id) id mismatch"
    );
    client.disconnect().await.expect("disconnect");
}
