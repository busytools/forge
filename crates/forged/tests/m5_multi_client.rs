//! M5 — broadcast-with-named-primary multi-client semantics.
//!
//! Per D11: connect = auto-primary, second-subscribe auto-takes,
//! `session.claim_primary` always grants, `session.peers` enumerates
//! subscribers with role + name. Tests cover:
//!
//! - `?name=` query param recorded on the [`Connection`].
//! - Auto-takeover round trip: A subscribes (primary, `initial`), B
//!   subscribes (primary, `auto_takeover_on_connect`), A demoted
//!   (`viewer`, `demoted`), `session.primary_changed` broadcast.
//! - `session.claim_primary` reclaim flow.
//! - `session.peers` shape — role + name + `connected_at`.
//! - Regression: a queued `permission.request` parked while A was
//!   primary surfaces in B's subscribe response after takeover; B's
//!   `prompts.respond` resolves the SDK-side oneshot.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, SystemTime};

use forged::registry::DaemonState;
use forged::session_state::SessionId;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMsg;

const MOCK_CLAUDE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../forge-sdk/tests/fixtures/mock_claude.sh"
);

/// Bind a listener on an ephemeral port, spawn the daemon, and wait
/// briefly so the accept loop is ready.
async fn spawn_daemon() -> (DaemonState, std::net::SocketAddr) {
    let state = DaemonState::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(forged::server::run(listener, state.clone()));
    tokio::time::sleep(Duration::from_millis(50)).await;
    (state, addr)
}

/// Drain WS frames until a JSON-RPC response carrying `target_id`
/// arrives. Frames that don't match are pushed into `parked` so the
/// caller can later look for notifications interleaved with the
/// response — this is critical for M5 where `subscribe` emits
/// `session.role_assigned` and `session.primary_changed` BEFORE the
/// response and `drain_for_response` would otherwise eat them.
async fn drain_for_response<S>(
    ws: &mut S,
    target_id: &serde_json::Value,
    parked: &mut Vec<serde_json::Value>,
) -> forged::jsonrpc::Response
where
    S: futures_util::Stream<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let WsMsg::Text(t) = ws.next().await.unwrap().unwrap() else {
            continue;
        };
        let v: serde_json::Value = serde_json::from_str(&t).unwrap();
        if v.get("id") == Some(target_id) {
            return serde_json::from_value(v).unwrap();
        }
        parked.push(v);
    }
}

/// Pull the first frame matching `method` from `parked`, falling back
/// to draining new frames from `ws` (also parking unrelated frames so
/// they remain available for later assertions).
async fn drain_for_method<S>(
    ws: &mut S,
    method: &str,
    parked: &mut Vec<serde_json::Value>,
) -> serde_json::Value
where
    S: futures_util::Stream<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    if let Some(idx) = parked
        .iter()
        .position(|v| v.get("method").and_then(|m| m.as_str()) == Some(method))
    {
        return parked.remove(idx);
    }
    loop {
        let WsMsg::Text(t) = ws.next().await.unwrap().unwrap() else {
            continue;
        };
        let v: serde_json::Value = serde_json::from_str(&t).unwrap();
        if v.get("method").and_then(|m| m.as_str()) == Some(method) {
            return v;
        }
        parked.push(v);
    }
}

// =============================================================================
// M5.1 — Connection name query param
// =============================================================================

#[tokio::test]
async fn name_query_param_is_recorded_on_connection() {
    let (state, addr) = spawn_daemon().await;

    let url = format!("ws://{addr}/?name=studio-terminal");
    let (mut ws, _) = connect_async(&url).await.unwrap();

    // Drain client.identify so the connection registration is settled.
    let mut parked = Vec::new();
    let _ = drain_for_method(&mut ws, "client.identify", &mut parked).await;

    // The connection should be in the registry with the parsed name.
    let conns = state.connections.lock().clone();
    assert_eq!(conns.len(), 1);
    let conn = conns.values().next().unwrap();
    assert_eq!(conn.name.as_deref(), Some("studio-terminal"));
    assert!(
        conn.connected_at_iso.contains('T') && conn.connected_at_iso.ends_with('Z'),
        "expected ISO-8601 'YYYY-...Z', got {}",
        conn.connected_at_iso
    );
}

#[tokio::test]
async fn missing_name_query_param_leaves_name_unset() {
    let (state, addr) = spawn_daemon().await;
    let url = format!("ws://{addr}/");
    let (mut ws, _) = connect_async(&url).await.unwrap();
    let mut parked = Vec::new();
    let _ = drain_for_method(&mut ws, "client.identify", &mut parked).await;

    let conns = state.connections.lock().clone();
    let conn = conns.values().next().unwrap();
    assert!(conn.name.is_none(), "expected no name, got {:?}", conn.name);
    assert!(!conn.connected_at_iso.is_empty());
}

#[tokio::test]
async fn name_query_param_decodes_percent_escapes() {
    let (state, addr) = spawn_daemon().await;
    // "studio terminal" with a percent-encoded space.
    let url = format!("ws://{addr}/?name=studio%20terminal");
    let (mut ws, _) = connect_async(&url).await.unwrap();
    let mut parked = Vec::new();
    let _ = drain_for_method(&mut ws, "client.identify", &mut parked).await;

    let conns = state.connections.lock().clone();
    let conn = conns.values().next().unwrap();
    assert_eq!(conn.name.as_deref(), Some("studio terminal"));
}

// =============================================================================
// M5.2 — Auto-takeover on subscribe
// =============================================================================

/// Minimal A-only subscribe sanity check — confirms the new
/// notification flow emits `role_assigned` + `primary_changed` and
/// the frame-buffering test helper picks them up correctly even
/// though they arrive before the response.
#[tokio::test]
async fn single_subscribe_emits_initial_role_and_primary_changed() {
    let (_state, addr) = spawn_daemon().await;
    let url = format!("ws://{addr}/?name=A");
    let (mut ws, _) = connect_async(&url).await.unwrap();
    let mut parked = Vec::new();
    let _ = drain_for_method(&mut ws, "client.identify", &mut parked).await;

    let spawn_req = forged::jsonrpc::Request::new(
        "session.spawn",
        serde_json::json!({"options": {"binary": MOCK_CLAUDE}}),
        serde_json::json!(1),
    );
    ws.send(WsMsg::Text(serde_json::to_string(&spawn_req).unwrap()))
        .await
        .unwrap();
    let spawn_resp = drain_for_response(&mut ws, &serde_json::json!(1), &mut parked).await;
    let session_id = spawn_resp
        .result
        .unwrap()
        .get("session_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();

    let sub = forged::jsonrpc::Request::new(
        "session.subscribe",
        serde_json::json!({"session_id": session_id}),
        serde_json::json!(2),
    );
    ws.send(WsMsg::Text(serde_json::to_string(&sub).unwrap()))
        .await
        .unwrap();
    let _ = drain_for_response(&mut ws, &serde_json::json!(2), &mut parked).await;
    let role = drain_for_method(&mut ws, "session.role_assigned", &mut parked).await;
    assert_eq!(role["params"]["reason"].as_str(), Some("initial"));
    let pc = drain_for_method(&mut ws, "session.primary_changed", &mut parked).await;
    assert_eq!(pc["params"]["reason"].as_str(), Some("initial"));
}

/// End-to-end auto-takeover roundtrip:
///
/// 1. Two clients connect. A spawns a session and subscribes — becomes
///    primary with `reason: "initial"`.
/// 2. B subscribes — becomes primary with `reason: "auto_takeover_on_connect"`.
/// 3. A receives `session.role_assigned { role: "viewer", reason: "demoted" }`.
/// 4. Both A and B receive a `session.primary_changed` broadcast.
#[tokio::test]
async fn second_subscribe_auto_takes_primary_demoting_first() {
    let (_state, addr) = spawn_daemon().await;

    let url_a = format!("ws://{addr}/?name=A");
    let url_b = format!("ws://{addr}/?name=B");
    let (mut ws_a, _) = connect_async(&url_a).await.unwrap();
    let (mut ws_b, _) = connect_async(&url_b).await.unwrap();

    let mut parked_a = Vec::new();
    let mut parked_b = Vec::new();

    // Drain client.identify on both so subsequent waits aren't confused.
    let _ = drain_for_method(&mut ws_a, "client.identify", &mut parked_a).await;
    let _ = drain_for_method(&mut ws_b, "client.identify", &mut parked_b).await;

    // A spawns a session.
    let spawn_req = forged::jsonrpc::Request::new(
        "session.spawn",
        serde_json::json!({"options": {"binary": MOCK_CLAUDE}}),
        serde_json::json!(1),
    );
    ws_a.send(WsMsg::Text(serde_json::to_string(&spawn_req).unwrap()))
        .await
        .unwrap();
    let spawn_resp = drain_for_response(&mut ws_a, &serde_json::json!(1), &mut parked_a).await;
    let session_id = spawn_resp
        .result
        .unwrap()
        .get("session_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();

    // A subscribes — becomes initial primary.
    let sub_a = forged::jsonrpc::Request::new(
        "session.subscribe",
        serde_json::json!({"session_id": session_id}),
        serde_json::json!(2),
    );
    ws_a.send(WsMsg::Text(serde_json::to_string(&sub_a).unwrap()))
        .await
        .unwrap();
    let resp_a = drain_for_response(&mut ws_a, &serde_json::json!(2), &mut parked_a).await;
    assert!(resp_a.error.is_none(), "subscribe A failed: {resp_a:?}");

    // A receives role_assigned (initial).
    let role_a = drain_for_method(&mut ws_a, "session.role_assigned", &mut parked_a).await;
    assert_eq!(role_a["params"]["role"].as_str(), Some("primary"));
    assert_eq!(role_a["params"]["reason"].as_str(), Some("initial"));
    // A receives primary_changed (initial).
    let pc_a_first = drain_for_method(&mut ws_a, "session.primary_changed", &mut parked_a).await;
    assert_eq!(pc_a_first["params"]["reason"].as_str(), Some("initial"));
    assert!(
        pc_a_first["params"]["previous"].is_null(),
        "expected previous=null on initial, got {:?}",
        pc_a_first["params"]["previous"]
    );

    // B subscribes — auto-takes; A demoted.
    let sub_b = forged::jsonrpc::Request::new(
        "session.subscribe",
        serde_json::json!({"session_id": session_id}),
        serde_json::json!(3),
    );
    ws_b.send(WsMsg::Text(serde_json::to_string(&sub_b).unwrap()))
        .await
        .unwrap();
    let resp_b = drain_for_response(&mut ws_b, &serde_json::json!(3), &mut parked_b).await;
    assert!(resp_b.error.is_none(), "subscribe B failed: {resp_b:?}");

    // B receives role_assigned primary, reason=auto_takeover_on_connect.
    let role_b = drain_for_method(&mut ws_b, "session.role_assigned", &mut parked_b).await;
    assert_eq!(role_b["params"]["role"].as_str(), Some("primary"));
    assert_eq!(
        role_b["params"]["reason"].as_str(),
        Some("auto_takeover_on_connect")
    );

    // A receives role_assigned viewer, reason=demoted.
    let demoted = drain_for_method(&mut ws_a, "session.role_assigned", &mut parked_a).await;
    assert_eq!(demoted["params"]["role"].as_str(), Some("viewer"));
    assert_eq!(demoted["params"]["reason"].as_str(), Some("demoted"));

    // Both A and B should see a primary_changed broadcast for the takeover.
    let pc_a = drain_for_method(&mut ws_a, "session.primary_changed", &mut parked_a).await;
    assert_eq!(
        pc_a["params"]["reason"].as_str(),
        Some("auto_takeover_on_connect")
    );
    let pc_b = drain_for_method(&mut ws_b, "session.primary_changed", &mut parked_b).await;
    assert_eq!(
        pc_b["params"]["reason"].as_str(),
        Some("auto_takeover_on_connect")
    );
}

// =============================================================================
// M5.3 — `session.claim_primary` + `session.peers`
// =============================================================================

/// `session.claim_primary` from a viewer demotes the existing primary
/// and promotes the caller. Wire flow:
///
/// 1. Caller receives `session.role_assigned { role: "primary",
///    reason: "claim" }`.
/// 2. Old primary receives `session.role_assigned { role: "viewer",
///    reason: "demoted" }`.
/// 3. Both receive `session.primary_changed { reason: "claimed" }`.
#[tokio::test]
async fn claim_primary_demotes_existing_and_promotes_caller() {
    use forged::connection::{Connection, ConnectionId};
    use tokio::sync::mpsc;

    let state = DaemonState::new();
    let sid = SessionId("sess_claim".into());
    let (handle, _rx) = state.register_session(sid.clone());

    let (a_tx, mut a_rx) = mpsc::channel(forged::connection::OUTBOUND_CHANNEL_CAPACITY);
    let (b_tx, mut b_rx) = mpsc::channel(forged::connection::OUTBOUND_CHANNEL_CAPACITY);
    let conn_a = Connection::with_metadata(
        ConnectionId("conn_A".into()),
        Some("A".into()),
        SystemTime::now(),
        a_tx,
    );
    let conn_b = Connection::with_metadata(
        ConnectionId("conn_B".into()),
        Some("B".into()),
        SystemTime::now(),
        b_tx,
    );
    state.register_connection(conn_a.clone());
    state.register_connection(conn_b.clone());

    // Seed: A is primary, B is viewer (both subscribed).
    {
        let mut subs = handle.subscribers.lock();
        subs.push(conn_a.id.clone());
        subs.push(conn_b.id.clone());
    }
    *handle.primary.lock() = Some(conn_a.id.clone());

    // B claims primary.
    forged::methods::multi_client::claim_primary(&state, &conn_b.id, &sid).unwrap();

    // Primary slot should now point at B.
    assert_eq!(*handle.primary.lock(), Some(conn_b.id.clone()));

    // B receives role_assigned primary, reason=claim, plus the
    // primary_changed broadcast.
    let mut b_saw_role = false;
    let mut b_saw_pc = false;
    while let Ok(frame) = b_rx.try_recv() {
        if let forged::connection::Outbound::Notification(n) = frame {
            match n.method.as_str() {
                "session.role_assigned" => {
                    let p = n.params.unwrap();
                    assert_eq!(p["role"].as_str(), Some("primary"));
                    assert_eq!(p["reason"].as_str(), Some("claim"));
                    assert_eq!(p["primary"].as_str(), Some("conn_B"));
                    b_saw_role = true;
                }
                "session.primary_changed" => {
                    let p = n.params.unwrap();
                    assert_eq!(p["reason"].as_str(), Some("claimed"));
                    assert_eq!(p["primary"].as_str(), Some("conn_B"));
                    assert_eq!(p["previous"].as_str(), Some("conn_A"));
                    b_saw_pc = true;
                }
                _ => {}
            }
        }
    }
    assert!(b_saw_role, "B did not receive role_assigned");
    assert!(b_saw_pc, "B did not receive primary_changed");

    // A receives role_assigned viewer, reason=demoted, plus
    // primary_changed.
    let mut a_saw_demoted = false;
    let mut a_saw_pc = false;
    while let Ok(frame) = a_rx.try_recv() {
        if let forged::connection::Outbound::Notification(n) = frame {
            match n.method.as_str() {
                "session.role_assigned" => {
                    let p = n.params.unwrap();
                    assert_eq!(p["role"].as_str(), Some("viewer"));
                    assert_eq!(p["reason"].as_str(), Some("demoted"));
                    a_saw_demoted = true;
                }
                "session.primary_changed" => {
                    let p = n.params.unwrap();
                    assert_eq!(p["reason"].as_str(), Some("claimed"));
                    a_saw_pc = true;
                }
                _ => {}
            }
        }
    }
    assert!(a_saw_demoted, "A did not receive role_assigned demoted");
    assert!(a_saw_pc, "A did not receive primary_changed");
}

/// Calling `claim_primary` while already primary is idempotent for
/// the role state but still emits the role + `primary_changed`
/// notifications — clients can rely on a frame after every successful
/// claim (per the contract documented on `multi_client::claim_primary`).
#[tokio::test]
async fn claim_primary_self_claim_is_idempotent_but_still_notifies() {
    use forged::connection::{Connection, ConnectionId};
    use tokio::sync::mpsc;

    let state = DaemonState::new();
    let sid = SessionId("sess_self_claim".into());
    let (handle, _rx) = state.register_session(sid.clone());

    let (a_tx, mut a_rx) = mpsc::channel(forged::connection::OUTBOUND_CHANNEL_CAPACITY);
    let conn_a = Connection::with_metadata(
        ConnectionId("conn_A".into()),
        Some("A".into()),
        SystemTime::now(),
        a_tx,
    );
    state.register_connection(conn_a.clone());
    handle.subscribers.lock().push(conn_a.id.clone());
    *handle.primary.lock() = Some(conn_a.id.clone());

    // Self-claim — A is already primary.
    forged::methods::multi_client::claim_primary(&state, &conn_a.id, &sid).unwrap();

    // Primary slot still points at A.
    assert_eq!(*handle.primary.lock(), Some(conn_a.id.clone()));

    // A should have received both role_assigned (reason=claim) and
    // primary_changed (reason=claimed). previous == primary == A
    // because no other primary was displaced.
    let mut saw_role = false;
    let mut saw_pc = false;
    while let Ok(frame) = a_rx.try_recv() {
        if let forged::connection::Outbound::Notification(n) = frame {
            match n.method.as_str() {
                "session.role_assigned" => {
                    let p = n.params.unwrap();
                    assert_eq!(p["role"].as_str(), Some("primary"));
                    assert_eq!(p["reason"].as_str(), Some("claim"));
                    assert_eq!(p["primary"].as_str(), Some("conn_A"));
                    saw_role = true;
                }
                "session.primary_changed" => {
                    let p = n.params.unwrap();
                    assert_eq!(p["primary"].as_str(), Some("conn_A"));
                    assert_eq!(p["previous"].as_str(), Some("conn_A"));
                    assert_eq!(p["reason"].as_str(), Some("claimed"));
                    saw_pc = true;
                }
                _ => {}
            }
        }
    }
    assert!(saw_role, "self-claim must still emit role_assigned");
    assert!(saw_pc, "self-claim must still emit primary_changed");
}

#[test]
fn claim_primary_unknown_session_returns_session_not_found() {
    use forged::connection::ConnectionId;
    let state = DaemonState::new();
    let unknown = SessionId("sess_does_not_exist".into());
    let err = forged::methods::multi_client::claim_primary(
        &state,
        &ConnectionId("conn_X".into()),
        &unknown,
    )
    .unwrap_err();
    assert!(
        matches!(err, forged::Error::SessionNotFound(_)),
        "expected SessionNotFound, got {err:?}"
    );
}

#[test]
fn peers_returns_role_and_name_per_subscriber() {
    use forged::connection::{Connection, ConnectionId};
    use tokio::sync::mpsc;

    let state = DaemonState::new();
    let sid = SessionId("sess_peers".into());
    let (handle, _rx) = state.register_session(sid.clone());

    let (a_tx, _a_rx) = mpsc::channel(forged::connection::OUTBOUND_CHANNEL_CAPACITY);
    let (b_tx, _b_rx) = mpsc::channel(forged::connection::OUTBOUND_CHANNEL_CAPACITY);
    let conn_a = Connection::with_metadata(
        ConnectionId("conn_A".into()),
        Some("studio".into()),
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        a_tx,
    );
    let conn_b = Connection::with_metadata(
        ConnectionId("conn_B".into()),
        None,
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_001),
        b_tx,
    );
    state.register_connection(conn_a.clone());
    state.register_connection(conn_b.clone());

    {
        let mut subs = handle.subscribers.lock();
        subs.push(conn_a.id.clone());
        subs.push(conn_b.id.clone());
    }
    *handle.primary.lock() = Some(conn_a.id.clone());

    let result = forged::methods::multi_client::peers(&state, &sid).unwrap();
    assert_eq!(result.peers.len(), 2);

    let a_entry = result
        .peers
        .iter()
        .find(|p| p.connection_id == "conn_A")
        .unwrap();
    assert_eq!(a_entry.role, "primary");
    assert_eq!(a_entry.name.as_deref(), Some("studio"));
    assert!(a_entry.connected_at.starts_with("20"));

    let b_entry = result
        .peers
        .iter()
        .find(|p| p.connection_id == "conn_B")
        .unwrap();
    assert_eq!(b_entry.role, "viewer");
    assert!(b_entry.name.is_none());
}

#[tokio::test]
async fn ws_session_peers_returns_subscribers_with_roles() {
    let (_state, addr) = spawn_daemon().await;

    let url_a = format!("ws://{addr}/?name=A");
    let url_b = format!("ws://{addr}/?name=B");
    let (mut ws_a, _) = connect_async(&url_a).await.unwrap();
    let (mut ws_b, _) = connect_async(&url_b).await.unwrap();

    let mut parked_a = Vec::new();
    let mut parked_b = Vec::new();
    let _ = drain_for_method(&mut ws_a, "client.identify", &mut parked_a).await;
    let _ = drain_for_method(&mut ws_b, "client.identify", &mut parked_b).await;

    // A spawns + subscribes.
    let spawn_req = forged::jsonrpc::Request::new(
        "session.spawn",
        serde_json::json!({"options": {"binary": MOCK_CLAUDE}}),
        serde_json::json!(1),
    );
    ws_a.send(WsMsg::Text(serde_json::to_string(&spawn_req).unwrap()))
        .await
        .unwrap();
    let spawn_resp = drain_for_response(&mut ws_a, &serde_json::json!(1), &mut parked_a).await;
    let session_id = spawn_resp
        .result
        .unwrap()
        .get("session_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();

    let sub_a = forged::jsonrpc::Request::new(
        "session.subscribe",
        serde_json::json!({"session_id": session_id}),
        serde_json::json!(2),
    );
    ws_a.send(WsMsg::Text(serde_json::to_string(&sub_a).unwrap()))
        .await
        .unwrap();
    let _ = drain_for_response(&mut ws_a, &serde_json::json!(2), &mut parked_a).await;

    // B subscribes (auto-takeover; B is now primary, A is viewer).
    let sub_b = forged::jsonrpc::Request::new(
        "session.subscribe",
        serde_json::json!({"session_id": session_id}),
        serde_json::json!(3),
    );
    ws_b.send(WsMsg::Text(serde_json::to_string(&sub_b).unwrap()))
        .await
        .unwrap();
    let _ = drain_for_response(&mut ws_b, &serde_json::json!(3), &mut parked_b).await;

    // Ask peers from B.
    let peers = forged::jsonrpc::Request::new(
        "session.peers",
        serde_json::json!({"session_id": session_id}),
        serde_json::json!(4),
    );
    ws_b.send(WsMsg::Text(serde_json::to_string(&peers).unwrap()))
        .await
        .unwrap();
    let resp = drain_for_response(&mut ws_b, &serde_json::json!(4), &mut parked_b).await;
    assert!(resp.error.is_none(), "peers error: {resp:?}");

    let result = resp.result.unwrap();
    let arr = result["peers"].as_array().unwrap();
    assert_eq!(arr.len(), 2);

    // The A entry should be viewer with name "A"; the B entry primary with name "B".
    let a_entry = arr
        .iter()
        .find(|p| p["name"].as_str() == Some("A"))
        .unwrap();
    let b_entry = arr
        .iter()
        .find(|p| p["name"].as_str() == Some("B"))
        .unwrap();
    assert_eq!(a_entry["role"].as_str(), Some("viewer"));
    assert_eq!(b_entry["role"].as_str(), Some("primary"));
    assert!(a_entry["connected_at"].as_str().unwrap().contains('T'));
    assert!(
        b_entry["connection_id"]
            .as_str()
            .unwrap()
            .starts_with("conn_")
    );
}

// =============================================================================
// Fix #3 regression — disconnect must purge from subscribers + primary
// =============================================================================

/// When a primary connection disconnects, the daemon must walk every
/// session's subscribers list, remove the dead conn, and clear the
/// primary slot. The next `session.peers` call must NOT report the
/// disconnected conn.
#[tokio::test]
async fn disconnect_purges_dead_conn_from_subscribers_and_clears_primary() {
    let (_state, addr) = spawn_daemon().await;

    let url_a = format!("ws://{addr}/?name=A");
    let (mut ws_a, _) = connect_async(&url_a).await.unwrap();
    let mut parked_a = Vec::new();
    let _ = drain_for_method(&mut ws_a, "client.identify", &mut parked_a).await;

    // A spawns + subscribes — A is initial primary.
    let spawn_req = forged::jsonrpc::Request::new(
        "session.spawn",
        serde_json::json!({"options": {"binary": MOCK_CLAUDE}}),
        serde_json::json!(1),
    );
    ws_a.send(WsMsg::Text(serde_json::to_string(&spawn_req).unwrap()))
        .await
        .unwrap();
    let spawn_resp = drain_for_response(&mut ws_a, &serde_json::json!(1), &mut parked_a).await;
    let session_id = spawn_resp
        .result
        .unwrap()
        .get("session_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();

    let sub_a = forged::jsonrpc::Request::new(
        "session.subscribe",
        serde_json::json!({"session_id": session_id}),
        serde_json::json!(2),
    );
    ws_a.send(WsMsg::Text(serde_json::to_string(&sub_a).unwrap()))
        .await
        .unwrap();
    let _ = drain_for_response(&mut ws_a, &serde_json::json!(2), &mut parked_a).await;

    // Drop A's WS handle — the daemon should observe the close, run
    // `unregister_connection`, walk sessions, drop A from subscribers
    // and clear the primary slot.
    drop(ws_a);

    // Give the daemon a moment to observe the close.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Connect B and call session.peers.
    let url_b = format!("ws://{addr}/?name=B");
    let (mut ws_b, _) = connect_async(&url_b).await.unwrap();
    let mut parked_b = Vec::new();
    let _ = drain_for_method(&mut ws_b, "client.identify", &mut parked_b).await;

    let peers = forged::jsonrpc::Request::new(
        "session.peers",
        serde_json::json!({"session_id": session_id}),
        serde_json::json!(99),
    );
    ws_b.send(WsMsg::Text(serde_json::to_string(&peers).unwrap()))
        .await
        .unwrap();
    let resp = drain_for_response(&mut ws_b, &serde_json::json!(99), &mut parked_b).await;
    let result = resp.result.unwrap();
    let arr = result["peers"].as_array().unwrap();
    // A's conn should not appear; only B (which hasn't subscribed
    // yet) — so the list might be empty or contain only B.
    for entry in arr {
        let name = entry["name"].as_str().unwrap_or("");
        assert_ne!(
            name, "A",
            "expected A's stale entry to be purged after disconnect"
        );
    }
}

/// When the primary disconnects, all subscribers should receive
/// `session.primary_changed { primary: null, reason: "disconnected" }`.
#[tokio::test]
async fn disconnect_broadcasts_primary_changed_with_disconnected_reason() {
    let (_state, addr) = spawn_daemon().await;

    // A spawns + subscribes (primary).
    let url_a = format!("ws://{addr}/?name=A");
    let (mut ws_a, _) = connect_async(&url_a).await.unwrap();
    let mut parked_a = Vec::new();
    let _ = drain_for_method(&mut ws_a, "client.identify", &mut parked_a).await;

    let spawn_req = forged::jsonrpc::Request::new(
        "session.spawn",
        serde_json::json!({"options": {"binary": MOCK_CLAUDE}}),
        serde_json::json!(1),
    );
    ws_a.send(WsMsg::Text(serde_json::to_string(&spawn_req).unwrap()))
        .await
        .unwrap();
    let spawn_resp = drain_for_response(&mut ws_a, &serde_json::json!(1), &mut parked_a).await;
    let session_id = spawn_resp
        .result
        .unwrap()
        .get("session_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();

    let sub_a = forged::jsonrpc::Request::new(
        "session.subscribe",
        serde_json::json!({"session_id": session_id}),
        serde_json::json!(2),
    );
    ws_a.send(WsMsg::Text(serde_json::to_string(&sub_a).unwrap()))
        .await
        .unwrap();
    let _ = drain_for_response(&mut ws_a, &serde_json::json!(2), &mut parked_a).await;

    // B subscribes (auto-takeover; B is primary, A is viewer now).
    let url_b = format!("ws://{addr}/?name=B");
    let (mut ws_b, _) = connect_async(&url_b).await.unwrap();
    let mut parked_b = Vec::new();
    let _ = drain_for_method(&mut ws_b, "client.identify", &mut parked_b).await;

    let sub_b = forged::jsonrpc::Request::new(
        "session.subscribe",
        serde_json::json!({"session_id": session_id}),
        serde_json::json!(3),
    );
    ws_b.send(WsMsg::Text(serde_json::to_string(&sub_b).unwrap()))
        .await
        .unwrap();
    let _ = drain_for_response(&mut ws_b, &serde_json::json!(3), &mut parked_b).await;

    // Drain whatever auto-takeover broadcasts A has buffered so we
    // can match strictly on the disconnect-emitted frame next.
    let _ = drain_for_method(&mut ws_a, "session.primary_changed", &mut parked_a).await;
    parked_a
        .retain(|v| v.get("method").and_then(|m| m.as_str()) != Some("session.primary_changed"));
    parked_b.clear();

    // Drop B (the current primary). A should observe a
    // primary_changed { reason: "disconnected" }.
    drop(ws_b);

    // Drain frames until we see one with reason=disconnected.
    let mut saw_disconnected = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        let Ok(frame) = tokio::time::timeout(
            Duration::from_millis(500),
            drain_for_method(&mut ws_a, "session.primary_changed", &mut parked_a),
        )
        .await
        else {
            continue;
        };
        let reason = frame["params"]["reason"].as_str().unwrap_or("");
        if reason == "disconnected" {
            assert!(
                frame["params"]["primary"].is_null(),
                "expected primary=null, got {}",
                frame["params"]["primary"]
            );
            saw_disconnected = true;
            break;
        }
        // Otherwise it's a stale takeover frame; keep draining.
    }
    assert!(
        saw_disconnected,
        "primary_changed {{ reason: disconnected }} not seen within 3s after B disconnect"
    );
}

// =============================================================================
// M5.4 — Regression: queued permission.request hands off via takeover
// =============================================================================

/// Simulates the takeover hand-off path locked-in by D11: an answering
/// client (A) is primary when a `permission.request` reverse-RPC parks
/// in the prompt queue; client B subscribes, auto-takes primary, and
/// resolves the parked prompt via `prompts.respond`.
///
/// The plan's M5.4 description allows simulation by parking a prompt
/// directly in the queue (matching the M4 pattern in
/// `prompts_respond_resolves_a_queued_prompt`); this avoids the
/// fiddliness of triggering the SDK's `can_use_tool` end to end while
/// still locking the contract that auto-takeover + queue +
/// `prompts.respond` compose correctly.
#[tokio::test]
async fn permission_request_hands_off_via_queue_on_takeover() {
    use forged::prompt_queue::{PendingPrompt, PromptKind};

    let (state, addr) = spawn_daemon().await;

    // Two clients.
    let url_a = format!("ws://{addr}/?name=A");
    let url_b = format!("ws://{addr}/?name=B");
    let (mut ws_a, _) = connect_async(&url_a).await.unwrap();
    let (mut ws_b, _) = connect_async(&url_b).await.unwrap();
    let mut parked_a = Vec::new();
    let mut parked_b = Vec::new();
    let _ = drain_for_method(&mut ws_a, "client.identify", &mut parked_a).await;
    let _ = drain_for_method(&mut ws_b, "client.identify", &mut parked_b).await;

    // Manually register a session (no actor) — gives us a queue we can
    // park a prompt into without spawning a full subprocess.
    let sid = SessionId("sess_handoff".into());
    let (handle, _rx) = state.register_session(sid.clone());

    // A subscribes — becomes initial primary.
    let sub_a = forged::jsonrpc::Request::new(
        "session.subscribe",
        serde_json::json!({"session_id": sid.0}),
        serde_json::json!(1),
    );
    ws_a.send(WsMsg::Text(serde_json::to_string(&sub_a).unwrap()))
        .await
        .unwrap();
    let _ = drain_for_response(&mut ws_a, &serde_json::json!(1), &mut parked_a).await;

    // Park a permission.request prompt in the queue (as if the SDK
    // bridge issued one and the client never answered). Per the M5
    // plan and the M4 patterns this matches: the regression test
    // simulates by parking directly to lock the contract that
    // takeover + queue + prompts.respond compose correctly.
    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel::<serde_json::Value>();
    handle.prompts.enqueue(PendingPrompt {
        prompt_id: "prompt_handoff".into(),
        kind: PromptKind::Permission,
        issued_at: SystemTime::now(),
        expires_at: SystemTime::now() + Duration::from_secs(3600),
        params: serde_json::json!({"tool_name": "Bash", "tool_input": {}}),
        responder: resp_tx,
        rev_id: None,
    });

    // B subscribes — auto-takeover; B's subscribe response should
    // include the parked prompt.
    let sub_b = forged::jsonrpc::Request::new(
        "session.subscribe",
        serde_json::json!({"session_id": sid.0}),
        serde_json::json!(2),
    );
    ws_b.send(WsMsg::Text(serde_json::to_string(&sub_b).unwrap()))
        .await
        .unwrap();
    let resp_b = drain_for_response(&mut ws_b, &serde_json::json!(2), &mut parked_b).await;
    assert!(resp_b.error.is_none(), "subscribe B failed: {resp_b:?}");

    let result = resp_b.result.unwrap();
    let pending = result["pending_prompts"].as_array().unwrap();
    assert_eq!(pending.len(), 1, "expected one parked prompt");
    let prompt_id = pending[0]["prompt_id"].as_str().unwrap();
    assert_eq!(prompt_id, "prompt_handoff");
    assert_eq!(pending[0]["kind"].as_str(), Some("permission.request"));

    // Confirm B is now primary.
    let b_conn_id = state
        .connections
        .lock()
        .iter()
        .find(|(_, c)| c.name.as_deref() == Some("B"))
        .map(|(cid, _)| cid.clone())
        .unwrap();
    assert_eq!(*handle.primary.lock(), Some(b_conn_id));

    // B answers via prompts.respond.
    let respond = forged::jsonrpc::Request::new(
        "prompts.respond",
        serde_json::json!({
            "session_id": sid.0,
            "prompt_id": "prompt_handoff",
            "result": {"decision": "allow"},
        }),
        serde_json::json!(3),
    );
    ws_b.send(WsMsg::Text(serde_json::to_string(&respond).unwrap()))
        .await
        .unwrap();
    let resp = drain_for_response(&mut ws_b, &serde_json::json!(3), &mut parked_b).await;
    assert!(resp.error.is_none(), "respond error: {resp:?}");

    // The SDK-side oneshot should resolve with the answer B sent.
    let answer = tokio::time::timeout(Duration::from_secs(2), resp_rx)
        .await
        .expect("respond timed out")
        .expect("oneshot dropped without value");
    assert_eq!(answer["decision"], serde_json::json!("allow"));
    assert_eq!(
        handle.prompts.snapshot().len(),
        0,
        "queue should be drained"
    );
}
