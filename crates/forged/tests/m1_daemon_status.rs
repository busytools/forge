//! M1 — JSON-RPC framing roundtrip + Error mapping + daemon.status + WS server + status CLI.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use forged::Error;
use forged::jsonrpc::{ErrorObject, Request, Response};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMsg;

#[test]
fn request_serializes_with_jsonrpc_marker() {
    let req = Request::new("daemon.status", serde_json::json!({}), serde_json::json!(1));
    let s = serde_json::to_string(&req).unwrap();
    assert!(s.contains("\"jsonrpc\":\"2.0\""));
    assert!(s.contains("\"method\":\"daemon.status\""));
    assert!(s.contains("\"id\":1"));
}

#[test]
fn response_success_has_result_no_error() {
    let resp = Response::success(serde_json::json!(1), serde_json::json!({"ok": true}));
    let s = serde_json::to_string(&resp).unwrap();
    assert!(s.contains("\"result\":{\"ok\":true}"));
    assert!(!s.contains("\"error\""));
}

#[test]
fn response_error_has_error_no_result() {
    let resp = Response::error(
        serde_json::json!(1),
        ErrorObject {
            code: -32601,
            message: "Method not found".into(),
            data: None,
        },
    );
    let s = serde_json::to_string(&resp).unwrap();
    assert!(s.contains("\"error\":"));
    assert!(s.contains("-32601"));
    assert!(!s.contains("\"result\""));
}

#[test]
fn error_session_not_found_maps_to_minus_32002() {
    let err = Error::SessionNotFound("sess_abc".into());
    let obj = err.to_jsonrpc();
    assert_eq!(obj.code, -32002);
    assert!(obj.message.contains("sess_abc"));
}

#[test]
fn error_method_not_found_maps_to_minus_32601() {
    let err = Error::MethodNotFound("foo.bar".into());
    let obj = err.to_jsonrpc();
    assert_eq!(obj.code, -32601);
}

#[tokio::test]
async fn daemon_status_returns_uptime_and_version() {
    let started_at = std::time::Instant::now()
        .checked_sub(std::time::Duration::from_secs(10))
        .unwrap();
    let result =
        forged::methods::daemon::status(&forged::registry::DaemonState::new_for_test(started_at))
            .await;
    let v = result.unwrap();
    assert!(v.uptime_seconds >= 10);
    assert_eq!(v.version, env!("CARGO_PKG_VERSION"));
    assert!(v.active_sessions == 0);
    assert!(v.connected_clients == 0);
}

#[tokio::test]
async fn server_accepts_ws_and_answers_daemon_status() {
    // Bind to ephemeral port on loopback.
    let state = forged::registry::DaemonState::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(forged::server::run(listener, state));

    // Give it a moment to start accepting.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Connect as a WS client.
    let url = format!("ws://{addr}/");
    let (mut ws, _resp) = connect_async(&url).await.unwrap();

    // Send daemon.status request.
    let req = Request::new("daemon.status", serde_json::json!({}), serde_json::json!(1));
    ws.send(WsMsg::Text(serde_json::to_string(&req).unwrap()))
        .await
        .unwrap();

    // Expect a response (skip notifications like client.identify).
    let resp: Response = loop {
        let msg = ws.next().await.unwrap().unwrap();
        let WsMsg::Text(text) = msg else { continue };
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        if v.get("id").is_some() {
            break serde_json::from_value(v).unwrap();
        }
    };
    assert_eq!(resp.id, serde_json::json!(1));
    let result = resp.result.expect("expected success");
    assert!(result.get("uptime_seconds").is_some());
    assert!(result.get("version").is_some());

    handle.abort();
}

#[tokio::test]
async fn status_cli_prints_uptime_to_stdout() {
    // Spin up a daemon on an ephemeral port.
    let state = forged::registry::DaemonState::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _handle = tokio::spawn(forged::server::run(listener, state));
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let out = forged::status_cli::query(&addr.to_string()).await.unwrap();
    assert!(out.contains("uptime_seconds"));
    assert!(out.contains(env!("CARGO_PKG_VERSION")));
}
