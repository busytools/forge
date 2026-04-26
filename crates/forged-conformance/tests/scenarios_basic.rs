//! Live-capture scenarios for the forged↔client wire.
//!
//! These tests are `#[ignore]` by default — they spin up a real forged on
//! an ephemeral loopback port, drive a WS client through the scenario,
//! capture the bidirectional trace as JSONL under
//! `target/forged-wire-traces/`, and exit.
//!
//! Opt in with `FORGED_WIRE_CAPTURE=1` and `--run-ignored only`:
//!
//! ```bash
//! FORGED_WIRE_CAPTURE=1 cargo nextest run -p forged-conformance \
//!   --no-capture --run-ignored only capture_m1_status
//! ```
//!
//! Promote a captured trace into the committed baseline:
//!
//! ```bash
//! cp target/forged-wire-traces/m1_status-*.jsonl \
//!    crates/forged-conformance/baselines/0.1.64/m1_status.jsonl
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMsg;

use forged_conformance::{PINNED_FORGED_VERSION, TraceEntry};

#[tokio::test]
#[ignore = "live capture; opt-in via FORGED_WIRE_CAPTURE=1"]
async fn capture_m1_status() {
    if std::env::var("FORGED_WIRE_CAPTURE").is_err() {
        eprintln!("FORGED_WIRE_CAPTURE not set; skipping");
        return;
    }

    // 1. Bind forged on ephemeral port.
    let state = forge_daemon::registry::DaemonState::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _h = tokio::spawn(forge_daemon::server::run(listener, state));
    tokio::time::sleep(Duration::from_millis(50)).await;

    // 2. Trace sink.
    let trace = Arc::new(Mutex::new(Vec::<TraceEntry>::new()));

    // 3. Open WS, capture client.identify (out from daemon's POV → "out"),
    //    send daemon.status (in from daemon's POV → "in"), capture response
    //    (out → "out").
    let url = format!("ws://{addr}/");
    let (mut ws, _) = connect_async(&url).await.unwrap();

    let WsMsg::Text(t) = ws.next().await.unwrap().unwrap() else {
        panic!("expected first frame to be Text (client.identify)");
    };
    trace.lock().push(TraceEntry {
        dir: "out".into(),
        line: t,
    });

    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "daemon.status",
        "params": {}
    });
    let body = serde_json::to_string(&req).unwrap();
    trace.lock().push(TraceEntry {
        dir: "in".into(),
        line: body.clone(),
    });
    ws.send(WsMsg::Text(body)).await.unwrap();

    let WsMsg::Text(t) = ws.next().await.unwrap().unwrap() else {
        panic!("expected response frame to be Text");
    };
    trace.lock().push(TraceEntry {
        dir: "out".into(),
        line: t,
    });

    // 4. Dump to disk.
    let target = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/forged-wire-traces");
    std::fs::create_dir_all(&target).unwrap();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let path = target.join(format!("m1_status-{ts}.jsonl"));
    let mut body = String::new();
    for e in trace.lock().iter() {
        use std::fmt::Write;
        let line = serde_json::to_string(e).expect("serialize trace entry");
        writeln!(body, "{line}").expect("write trace line");
    }
    std::fs::write(&path, body).unwrap();
    eprintln!("captured trace: {}", path.display());
    eprintln!(
        "promote with: cp {} crates/forged-conformance/baselines/{}/m1_status.jsonl",
        path.display(),
        PINNED_FORGED_VERSION
    );
}

/// Helper: dump a captured trace to `target/forged-wire-traces/`. The
/// caller hands us the trace + scenario name; we name the file
/// `<scenario>-<unix-ts>.jsonl` so multiple captures don't collide.
fn dump_trace(scenario: &str, trace: &Mutex<Vec<TraceEntry>>) -> std::path::PathBuf {
    let target = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/forged-wire-traces");
    std::fs::create_dir_all(&target).unwrap();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let path = target.join(format!("{scenario}-{ts}.jsonl"));
    let mut body = String::new();
    for e in trace.lock().iter() {
        use std::fmt::Write;
        let line = serde_json::to_string(e).expect("serialize trace entry");
        writeln!(body, "{line}").expect("write trace line");
    }
    std::fs::write(&path, body).unwrap();
    eprintln!("captured trace: {}", path.display());
    eprintln!(
        "promote with: cp {} crates/forged-conformance/baselines/{}/{scenario}.jsonl",
        path.display(),
        PINNED_FORGED_VERSION
    );
    path
}

/// Drive a `session.subscribe` round trip against the daemon and dump
/// the trace. The session is registered manually so the capture
/// doesn't require a real subprocess.
#[tokio::test]
#[ignore = "live capture; opt-in via FORGED_WIRE_CAPTURE=1"]
async fn capture_session_subscribe_basic() {
    if std::env::var("FORGED_WIRE_CAPTURE").is_err() {
        eprintln!("FORGED_WIRE_CAPTURE not set; skipping");
        return;
    }

    let state = forge_daemon::registry::DaemonState::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _h = tokio::spawn(forge_daemon::server::run(listener, state.clone()));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let trace = Arc::new(Mutex::new(Vec::<TraceEntry>::new()));
    let url = format!("ws://{addr}/");
    let (mut ws, _) = connect_async(&url).await.unwrap();

    // Drain client.identify.
    let WsMsg::Text(t) = ws.next().await.unwrap().unwrap() else {
        panic!("expected text frame")
    };
    trace.lock().push(TraceEntry {
        dir: "out".into(),
        line: t,
    });

    // Manually register a fake session — capture the subscribe round
    // trip without needing a real subprocess.
    let sid = forge_daemon::session_state::SessionId("sess_demo".into());
    let _registered = state.register_session(sid.clone());

    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "req_1",
        "method": "session.subscribe",
        "params": {"session_id": "sess_demo"},
    });
    let body = serde_json::to_string(&req).unwrap();
    trace.lock().push(TraceEntry {
        dir: "in".into(),
        line: body.clone(),
    });
    ws.send(WsMsg::Text(body)).await.unwrap();

    // Drain the role_assigned + primary_changed notifications and the
    // subscribe response.
    for _ in 0..3 {
        let WsMsg::Text(t) = ws.next().await.unwrap().unwrap() else {
            continue;
        };
        trace.lock().push(TraceEntry {
            dir: "out".into(),
            line: t,
        });
    }

    let _ = dump_trace("session_subscribe_basic", &trace);
}

/// Drive a permission.request round trip against the daemon and dump
/// the trace. The captured shape mirrors the hand-authored
/// `permission_request_round_trip.jsonl` baseline.
#[tokio::test]
#[ignore = "live capture; opt-in via FORGED_WIRE_CAPTURE=1"]
async fn capture_permission_request_round_trip() {
    if std::env::var("FORGED_WIRE_CAPTURE").is_err() {
        eprintln!("FORGED_WIRE_CAPTURE not set; skipping");
        return;
    }

    let state = forge_daemon::registry::DaemonState::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _h = tokio::spawn(forge_daemon::server::run(listener, state.clone()));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let trace = Arc::new(Mutex::new(Vec::<TraceEntry>::new()));
    let url = format!("ws://{addr}/");
    let (mut ws, _) = connect_async(&url).await.unwrap();

    let WsMsg::Text(t) = ws.next().await.unwrap().unwrap() else {
        panic!("expected text frame")
    };
    trace.lock().push(TraceEntry {
        dir: "out".into(),
        line: t,
    });

    // Wait for the daemon to register the connection so we can pin
    // the connection as primary on a fake session.
    let conn_id = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(id) = state.connections.lock().keys().next().cloned() {
                break id;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("connection_id discovery timed out");

    let sid = forge_daemon::session_state::SessionId("sess_demo".into());
    let (handle, _rx) = state.register_session(sid.clone());
    *handle.primary.lock() = Some(conn_id.clone());

    // Daemon issues the reverse-RPC; capture the outbound frame.
    let state_arc = std::sync::Arc::new(state.clone());
    let sid_for = sid.clone();
    let issue = tokio::spawn(async move {
        forge_daemon::reverse_rpc::issue_to_primary(
            &state_arc,
            &sid_for,
            "permission.request",
            serde_json::json!({"tool_name": "Bash", "tool_input": {"command": "ls"}}),
            forge_daemon::prompt_queue::PromptKind::Permission,
            Duration::from_secs(5),
        )
        .await
    });

    let WsMsg::Text(t) = ws.next().await.unwrap().unwrap() else {
        panic!("expected text frame")
    };
    let v: serde_json::Value = serde_json::from_str(&t).unwrap();
    let rev_id = v["id"].as_str().unwrap().to_string();
    trace.lock().push(TraceEntry {
        dir: "out".into(),
        line: t,
    });

    // Client answers — capture inbound.
    let resp = serde_json::json!({
        "jsonrpc": "2.0",
        "id": rev_id,
        "result": {"decision": "allow"},
    });
    let resp_body = serde_json::to_string(&resp).unwrap();
    trace.lock().push(TraceEntry {
        dir: "in".into(),
        line: resp_body.clone(),
    });
    ws.send(WsMsg::Text(resp_body)).await.unwrap();
    let _ = issue.await.unwrap().unwrap();

    let _ = dump_trace("permission_request_round_trip", &trace);
}

/// Drive an `?name=studio-terminal` handshake plus a `daemon.status`
/// round trip. Round 3 — fix M12. The `client.identify` wire payload
/// does NOT carry the name (the name is server-side metadata
/// surfaced via `session.peers`), so the captured trace is shape-
/// equivalent to `m1_status` — but the name flows through the URL
/// query to the server. The baseline locks that contract.
#[tokio::test]
#[ignore = "live capture; opt-in via FORGED_WIRE_CAPTURE=1"]
async fn capture_client_identify_with_name() {
    if std::env::var("FORGED_WIRE_CAPTURE").is_err() {
        eprintln!("FORGED_WIRE_CAPTURE not set; skipping");
        return;
    }

    let state = forge_daemon::registry::DaemonState::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _h = tokio::spawn(forge_daemon::server::run(listener, state));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let trace = Arc::new(Mutex::new(Vec::<TraceEntry>::new()));

    // Connect WITH a name parameter — exercises the server's
    // `?name=` parsing path.
    let url = format!("ws://{addr}/?name=studio-terminal");
    let (mut ws, _) = connect_async(&url).await.unwrap();

    let WsMsg::Text(t) = ws.next().await.unwrap().unwrap() else {
        panic!("expected first frame to be Text (client.identify)");
    };
    trace.lock().push(TraceEntry {
        dir: "out".into(),
        line: t,
    });

    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "daemon.status",
        "params": {}
    });
    let body = serde_json::to_string(&req).unwrap();
    trace.lock().push(TraceEntry {
        dir: "in".into(),
        line: body.clone(),
    });
    ws.send(WsMsg::Text(body)).await.unwrap();

    let WsMsg::Text(t) = ws.next().await.unwrap().unwrap() else {
        panic!("expected response frame to be Text");
    };
    trace.lock().push(TraceEntry {
        dir: "out".into(),
        line: t,
    });

    let _ = dump_trace("client_identify_with_name", &trace);
}

/// Drive a `permission.request` round trip where the client returns
/// a JSON-RPC error response (`{"error": {...}}`) instead of a
/// `{"result": ...}`. The daemon's `resolve_error` path wraps the
/// error in the `_jsonrpc_error` sentinel; the bridge then maps it
/// to a typed deny. Round 3 — fix M12. Mirrors the hand-authored
/// `permission_request_jsonrpc_error.jsonl` baseline.
#[tokio::test]
#[ignore = "live capture; opt-in via FORGED_WIRE_CAPTURE=1"]
async fn capture_permission_request_jsonrpc_error() {
    if std::env::var("FORGED_WIRE_CAPTURE").is_err() {
        eprintln!("FORGED_WIRE_CAPTURE not set; skipping");
        return;
    }

    let state = forge_daemon::registry::DaemonState::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _h = tokio::spawn(forge_daemon::server::run(listener, state.clone()));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let trace = Arc::new(Mutex::new(Vec::<TraceEntry>::new()));
    let url = format!("ws://{addr}/");
    let (mut ws, _) = connect_async(&url).await.unwrap();

    let WsMsg::Text(t) = ws.next().await.unwrap().unwrap() else {
        panic!("expected text frame")
    };
    trace.lock().push(TraceEntry {
        dir: "out".into(),
        line: t,
    });

    // Wait for the daemon to register the connection so we can pin
    // the connection as primary on a fake session.
    let conn_id = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(id) = state.connections.lock().keys().next().cloned() {
                break id;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("connection_id discovery timed out");

    let sid = forge_daemon::session_state::SessionId("sess_demo".into());
    let (handle, _rx) = state.register_session(sid.clone());
    *handle.primary.lock() = Some(conn_id.clone());

    // Daemon issues the reverse-RPC; capture the outbound frame.
    let state_arc = std::sync::Arc::new(state.clone());
    let sid_for = sid.clone();
    let issue = tokio::spawn(async move {
        forge_daemon::reverse_rpc::issue_to_primary(
            &state_arc,
            &sid_for,
            "permission.request",
            serde_json::json!({"tool_name": "Bash", "tool_input": {"command": "rm -rf /"}}),
            forge_daemon::prompt_queue::PromptKind::Permission,
            Duration::from_secs(5),
        )
        .await
    });

    let WsMsg::Text(t) = ws.next().await.unwrap().unwrap() else {
        panic!("expected text frame")
    };
    let v: serde_json::Value = serde_json::from_str(&t).unwrap();
    let rev_id = v["id"].as_str().unwrap().to_string();
    trace.lock().push(TraceEntry {
        dir: "out".into(),
        line: t,
    });

    // Client answers with a JSON-RPC error — capture inbound.
    let resp = serde_json::json!({
        "jsonrpc": "2.0",
        "id": rev_id,
        "error": {"code": -32601, "message": "client refused: dangerous command"},
    });
    let resp_body = serde_json::to_string(&resp).unwrap();
    trace.lock().push(TraceEntry {
        dir: "in".into(),
        line: resp_body.clone(),
    });
    ws.send(WsMsg::Text(resp_body)).await.unwrap();
    let _ = issue.await.unwrap();

    let _ = dump_trace("permission_request_jsonrpc_error", &trace);
}

/// Drive an auto-takeover round trip with two clients, capture the
/// trace from the second client's perspective. Mirrors the hand-
/// authored `multi_client_takeover.jsonl` baseline.
#[tokio::test]
#[ignore = "live capture; opt-in via FORGED_WIRE_CAPTURE=1"]
async fn capture_multi_client_takeover() {
    if std::env::var("FORGED_WIRE_CAPTURE").is_err() {
        eprintln!("FORGED_WIRE_CAPTURE not set; skipping");
        return;
    }

    let state = forge_daemon::registry::DaemonState::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _h = tokio::spawn(forge_daemon::server::run(listener, state.clone()));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let url_a = format!("ws://{addr}/?name=A");
    let url_b = format!("ws://{addr}/?name=B");
    let (mut ws_a, _) = connect_async(&url_a).await.unwrap();
    let (mut ws_b, _) = connect_async(&url_b).await.unwrap();

    // Drain client.identify on A.
    let _ = ws_a.next().await;

    // Manually register a fake session.
    let sid = forge_daemon::session_state::SessionId("sess_demo".into());
    let _kept = state.register_session(sid.clone());

    // A subscribes (becomes initial primary). Drain everything for A.
    let sub_a = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "req_1",
        "method": "session.subscribe",
        "params": {"session_id": "sess_demo"},
    });
    ws_a.send(WsMsg::Text(serde_json::to_string(&sub_a).unwrap()))
        .await
        .unwrap();

    // Wait for A's subscribe to settle by draining role_assigned +
    // primary_changed + response. Tolerant timeout.
    let drain_a = async {
        for _ in 0..5 {
            let _ = tokio::time::timeout(Duration::from_millis(500), ws_a.next()).await;
        }
    };
    drain_a.await;

    // Now start capturing B's perspective.
    let trace = Arc::new(Mutex::new(Vec::<TraceEntry>::new()));
    // Capture B's client.identify first.
    let WsMsg::Text(t) = ws_b.next().await.unwrap().unwrap() else {
        panic!("expected text frame")
    };
    trace.lock().push(TraceEntry {
        dir: "out".into(),
        line: t,
    });

    // B subscribes — auto-takeover.
    let sub_b = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "req_2",
        "method": "session.subscribe",
        "params": {"session_id": "sess_demo"},
    });
    let body = serde_json::to_string(&sub_b).unwrap();
    trace.lock().push(TraceEntry {
        dir: "in".into(),
        line: body.clone(),
    });
    ws_b.send(WsMsg::Text(body)).await.unwrap();

    // Drain role_assigned + primary_changed + response on B.
    for _ in 0..3 {
        let WsMsg::Text(t) = ws_b.next().await.unwrap().unwrap() else {
            continue;
        };
        trace.lock().push(TraceEntry {
            dir: "out".into(),
            line: t,
        });
    }

    // B asks for peers.
    let peers = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "req_3",
        "method": "session.peers",
        "params": {"session_id": "sess_demo"},
    });
    let body = serde_json::to_string(&peers).unwrap();
    trace.lock().push(TraceEntry {
        dir: "in".into(),
        line: body.clone(),
    });
    ws_b.send(WsMsg::Text(body)).await.unwrap();
    let WsMsg::Text(t) = ws_b.next().await.unwrap().unwrap() else {
        panic!("expected text frame")
    };
    trace.lock().push(TraceEntry {
        dir: "out".into(),
        line: t,
    });

    let _ = dump_trace("multi_client_takeover", &trace);
}
