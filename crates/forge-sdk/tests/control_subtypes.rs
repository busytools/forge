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
    // This test exercises the writer-side control-request round-trips
    // and never reads events; drop the events receiver immediately.
    let (client, _events) = Client::spawn(opts).await.expect("spawn");
    client
}

#[tokio::test]
async fn interrupt_round_trip() {
    let client = spawn_client().await;
    client.interrupt().await.expect("interrupt");
    client.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn set_permission_mode_round_trip() {
    let client = spawn_client().await;
    client
        .set_permission_mode(PermissionMode::AcceptEdits)
        .await
        .expect("set_permission_mode");
    client.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn rewind_files_round_trip() {
    let client = spawn_client().await;
    client
        .rewind_files("msg_user_01")
        .await
        .expect("rewind_files");
    client.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn mcp_reconnect_round_trip() {
    let client = spawn_client().await;
    client
        .mcp_reconnect("my-server")
        .await
        .expect("mcp_reconnect");
    client.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn mcp_toggle_round_trip() {
    let client = spawn_client().await;
    client
        .mcp_toggle("my-server", false)
        .await
        .expect("mcp_toggle");
    client.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn stop_task_round_trip() {
    let client = spawn_client().await;
    client.stop_task("task_abc").await.expect("stop_task");
    client.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn mcp_status_raw_returns_canned_payload() {
    let client = spawn_client().await;
    let resp = client.mcp_status_raw().await.expect("mcp_status");
    assert_eq!(
        resp,
        serde_json::json!({"servers": []}),
        "mcp_status payload mismatch"
    );
    client.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn get_context_usage_raw_returns_canned_payload() {
    let client = spawn_client().await;
    let resp = client
        .get_context_usage_raw()
        .await
        .expect("get_context_usage");
    assert_eq!(
        resp,
        serde_json::json!({"used": 0, "budget": 200_000}),
        "get_context_usage payload mismatch"
    );
    client.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn set_model_round_trip_with_model() {
    let client = spawn_client().await;
    client
        .set_model(Some("claude-sonnet-4-6"))
        .await
        .expect("set_model");
    client.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn set_model_round_trip_with_none_reverts_to_default() {
    let client = spawn_client().await;
    // Python accepts `model=None` to revert to CLI default; forge-sdk
    // passes Option<&str>, so None serialises to JSON null.
    client.set_model(None).await.expect("set_model");
    client.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn get_server_info_returns_cached_initialize_payload() {
    // The mock replies to `initialize` with a canned
    // `{"commands": [...], "outputStyle": "default"}` body. Client::spawn
    // stores it so get_server_info() surfaces the payload later
    // without re-issuing the handshake (mirrors Python
    // ClaudeSDKClient.get_server_info — client.py:541-564).
    let client = spawn_client().await;
    let info = client.get_server_info().expect("initialize payload cached");
    assert_eq!(info["outputStyle"], "default");
    assert!(info["commands"].is_array(), "expected commands array");
    client.disconnect().await.expect("disconnect");
}
