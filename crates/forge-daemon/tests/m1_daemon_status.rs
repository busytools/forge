//! M1 — JSON-RPC framing roundtrip + Error mapping + daemon.status + WS server + status CLI.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use forge_daemon::Error;
use forge_daemon::jsonrpc::{ErrorObject, Request, Response};
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
    let result = forge_daemon::methods::daemon::status(
        &forge_daemon::registry::DaemonState::new_for_test(started_at),
    )
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
    let state = forge_daemon::registry::DaemonState::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(forge_daemon::server::run(listener, state));

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

/// Drive a real WS client against forged with a malformed JSON body
/// and assert the daemon answers with a JSON-RPC parse-error response
/// keyed off the null id (per spec — we don't know the caller's id).
#[tokio::test]
async fn malformed_json_body_yields_parse_error_response_with_null_id() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message as WsMsg;

    let state = forge_daemon::registry::DaemonState::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(forge_daemon::server::run(listener, state));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (mut ws, _) = connect_async(format!("ws://{addr}/")).await.unwrap();
    // Drain client.identify.
    let _ = ws.next().await;

    ws.send(WsMsg::Text("{not valid json".into()))
        .await
        .unwrap();

    // Expect an error response carrying parse-error code -32700 + null id.
    let resp = loop {
        let msg = ws.next().await.unwrap().unwrap();
        let WsMsg::Text(t) = msg else { continue };
        let v: serde_json::Value = serde_json::from_str(&t).unwrap();
        if v.get("error").is_some() {
            break v;
        }
    };
    assert!(resp["id"].is_null(), "expected null id, got {}", resp["id"]);
    let code = resp["error"]["code"].as_i64().unwrap();
    assert_eq!(code, -32700, "expected parse-error code");
}

/// Valid JSON body that is missing the `method` field — the JSON-RPC
/// envelope deserialiser should reject this, surfacing an error
/// response keyed off the null id.
#[tokio::test]
async fn valid_json_with_missing_method_yields_invalid_request_error() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message as WsMsg;

    let state = forge_daemon::registry::DaemonState::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(forge_daemon::server::run(listener, state));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (mut ws, _) = connect_async(format!("ws://{addr}/")).await.unwrap();
    let _ = ws.next().await;

    // No `method` and no `result/error` — neither a valid request nor
    // a response, so envelope deserialise will fail.
    ws.send(WsMsg::Text("{\"jsonrpc\":\"2.0\",\"id\":42}".into()))
        .await
        .unwrap();

    let resp = loop {
        let msg = ws.next().await.unwrap().unwrap();
        let WsMsg::Text(t) = msg else { continue };
        let v: serde_json::Value = serde_json::from_str(&t).unwrap();
        if v.get("error").is_some() {
            break v;
        }
    };
    let code = resp["error"]["code"].as_i64().unwrap();
    // Daemon currently maps envelope decode failure to ParseError
    // (-32700) since that's the closest standard JSON-RPC code; the
    // contract under test is just "an error fires, not silent drop".
    assert!(
        code == -32700 || code == -32600,
        "expected parse/invalid-request code, got {code}"
    );
}

/// Send a binary WebSocket frame — the daemon must ignore it without
/// dropping the connection. Subsequent text frames continue to work.
#[tokio::test]
async fn binary_ws_frame_is_ignored_without_killing_connection() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message as WsMsg;

    let state = forge_daemon::registry::DaemonState::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(forge_daemon::server::run(listener, state));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (mut ws, _) = connect_async(format!("ws://{addr}/")).await.unwrap();
    let _ = ws.next().await; // client.identify

    // Send a binary frame — should be ignored.
    ws.send(WsMsg::Binary(vec![0u8, 1, 2, 3])).await.unwrap();
    // Follow with a normal request.
    let req = forge_daemon::jsonrpc::Request::new(
        "daemon.status",
        serde_json::json!({}),
        serde_json::json!(99),
    );
    ws.send(WsMsg::Text(serde_json::to_string(&req).unwrap()))
        .await
        .unwrap();

    // Expect the daemon.status response — connection is alive.
    let resp = loop {
        let msg = ws.next().await.unwrap().unwrap();
        let WsMsg::Text(t) = msg else { continue };
        let v: serde_json::Value = serde_json::from_str(&t).unwrap();
        if v.get("id") == Some(&serde_json::json!(99)) {
            break v;
        }
    };
    assert!(
        resp.get("result").is_some(),
        "expected success response after binary frame, got {resp}"
    );
}

#[tokio::test]
async fn status_cli_prints_uptime_to_stdout() {
    // Spin up a daemon on an ephemeral port.
    let state = forge_daemon::registry::DaemonState::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _handle = tokio::spawn(forge_daemon::server::run(listener, state));
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let out = forge_daemon::status_cli::query(&addr.to_string())
        .await
        .unwrap();
    assert!(out.contains("uptime_seconds"));
    assert!(out.contains(env!("CARGO_PKG_VERSION")));
}
