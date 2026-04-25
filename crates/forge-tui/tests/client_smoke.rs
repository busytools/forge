//! M7.1 — forge-tui WS client smoke tests against a real forged.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use forge_tui::client::Client;

fn spawn_forged() -> std::net::SocketAddr {
    let state = forged::registry::DaemonState::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
    tokio::spawn(forged::server::run(listener, state));
    addr
}

#[tokio::test]
async fn client_connects_and_calls_daemon_status() {
    let addr = spawn_forged();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let client = Client::connect(&format!("ws://{addr}/?name=tui-test"))
        .await
        .unwrap();

    let status: serde_json::Value = client
        .call("daemon.status", serde_json::json!({}))
        .await
        .unwrap();
    assert!(status.get("uptime_seconds").is_some());
    assert!(status.get("version").is_some());
}

#[tokio::test]
async fn client_send_response_does_not_panic_on_unknown_rev_id() {
    let addr = spawn_forged();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let client = Client::connect(&format!("ws://{addr}/?name=tui-test"))
        .await
        .unwrap();

    // Even when the daemon never asked for it, `send_response` shouldn't
    // tear down the channel — the daemon will simply log/drop an
    // unexpected response.
    client
        .send_response(
            serde_json::json!("rev_does_not_exist"),
            serde_json::json!({"decision": "deny"}),
        )
        .unwrap();

    // After send_response, normal calls still work.
    let status: serde_json::Value = client
        .call("daemon.status", serde_json::json!({}))
        .await
        .unwrap();
    assert!(status.get("uptime_seconds").is_some());
}

#[tokio::test]
async fn client_subscribe_yields_session_events_via_stream() {
    use futures_util::StreamExt;

    let state = forged::registry::DaemonState::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
    tokio::spawn(forged::server::run(listener, state.clone()));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let client = Client::connect(&format!("ws://{addr}/?name=tui-test"))
        .await
        .unwrap();

    // Register a fake session in the daemon (no real subprocess) so the
    // subscribe call has somewhere to land.
    let sid = forged::session_state::SessionId("sess_sub_test".into());
    let (_handle, _rx) = state.register_session(sid.clone());

    let mut events = client.subscribe_session("sess_sub_test").await.unwrap();

    // Fan out a notification through the daemon's broadcast helper.
    forged::broadcast::fanout(
        &state,
        &sid,
        &forged::connection::Outbound::Notification(forged::jsonrpc::Notification::new(
            "session.event",
            serde_json::json!({
                "session_id": "sess_sub_test",
                "event_id": 1,
                "message": {"type": "user", "message": {"content": [{"type": "text", "text": "hi"}]}},
            }),
        )),
    );

    let evt = tokio::time::timeout(Duration::from_secs(3), events.next())
        .await
        .expect("timed out waiting for session.event")
        .expect("subscribe stream closed unexpectedly");
    assert!(evt.get("message").is_some());
    assert_eq!(evt["session_id"], serde_json::json!("sess_sub_test"));
}

#[tokio::test]
async fn client_method_not_found_surfaces_as_daemon_error() {
    let addr = spawn_forged();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let client = Client::connect(&format!("ws://{addr}/?name=tui-test"))
        .await
        .unwrap();

    let err = client
        .call::<_, serde_json::Value>("does.not.exist", serde_json::json!({}))
        .await
        .unwrap_err();
    match err {
        forge_tui::client::ClientError::Daemon { code, .. } => {
            assert_eq!(code, -32601, "method-not-found");
        }
        other => panic!("expected Daemon error, got {other:?}"),
    }
}
