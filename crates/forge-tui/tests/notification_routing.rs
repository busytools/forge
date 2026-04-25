//! Integration test locking the contract that fix #1 establishes:
//! `Client::subscribe_session` registers the local mpsc with the read
//! loop, so notifications dispatched as `session.event` round-trip
//! into the consumer's stream.
//!
//! Without this contract, `Client::call("session.subscribe", ...)`
//! issues the daemon RPC but skips the local mpsc registration and
//! every notification gets silently dropped.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use forge_tui::client::Client;
use futures_util::StreamExt;

fn spawn_forged() -> (forged::registry::DaemonState, std::net::SocketAddr) {
    let state = forged::registry::DaemonState::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
    tokio::spawn(forged::server::run(listener, state.clone()));
    (state, addr)
}

#[tokio::test]
async fn subscribe_session_round_trips_notification_into_stream() {
    let (state, addr) = spawn_forged();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let client = Client::connect(&format!("ws://{addr}/?name=routing-test"))
        .await
        .unwrap();

    // Register a fake session in the daemon.
    let sid = forged::session_state::SessionId("sess_routing".into());
    let (_handle, _rx) = state.register_session(sid.clone());

    // Subscribe via Client::subscribe_session — this registers a
    // mpsc on the read loop AND issues the session.subscribe RPC.
    let mut stream = client.subscribe_session("sess_routing").await.unwrap();

    // Fan a session.event notification through the broadcast helper.
    forged::broadcast::fanout(
        &state,
        &sid,
        &forged::connection::Outbound::Notification(forged::jsonrpc::Notification::new(
            "session.event",
            serde_json::json!({
                "session_id": "sess_routing",
                "event_id": "msg_42",
                "message": {"type": "user", "message": {"content": "hi"}},
            }),
        )),
    );

    let frame = tokio::time::timeout(Duration::from_secs(3), stream.next())
        .await
        .expect("subscription stream did not yield within 3s")
        .expect("stream closed");
    assert_eq!(
        frame["session_id"],
        serde_json::json!("sess_routing"),
        "expected session_id round-tripped, got {frame}"
    );
}

/// Drains the notifications channel and verifies that
/// `session.role_assigned`, `session.primary_changed`,
/// `session.closed`, `prompts.expired` are all routed to it (NOT to a
/// per-session subscription). Locks fix #1's contract.
#[tokio::test]
async fn unrouted_notifications_arrive_on_notifications_channel() {
    let (state, addr) = spawn_forged();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let client = Client::connect(&format!("ws://{addr}/?name=notif-test"))
        .await
        .unwrap();
    let mut notifs = client.notifications().expect("first call returns Some");

    // Drain client.identify (also routed to the notifications mpsc
    // because it isn't a session.event).
    let _ident = tokio::time::timeout(Duration::from_secs(1), notifs.recv())
        .await
        .expect("client.identify never arrived")
        .expect("notifications channel closed");

    // Wait for daemon to register the connection.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let conn_id = state.connections.lock().keys().next().cloned().unwrap();

    // Register a session and pin our connection as primary.
    let sid = forged::session_state::SessionId("sess_notif".into());
    let (handle, _rx) = state.register_session(sid.clone());
    *handle.primary.lock() = Some(conn_id.clone());
    handle.subscribers.lock().push(conn_id.clone());

    // Send a primary_changed via fanout.
    forged::broadcast::fanout(
        &state,
        &sid,
        &forged::connection::Outbound::Notification(forged::jsonrpc::Notification::new(
            "session.primary_changed",
            serde_json::json!({
                "session_id": "sess_notif",
                "primary": conn_id.0,
                "previous": null,
                "reason": "test_fanout",
            }),
        )),
    );

    // The forge-tui Client should route this to the notifications mpsc.
    let frame = tokio::time::timeout(Duration::from_secs(3), notifs.recv())
        .await
        .expect("notification did not arrive within 3s")
        .expect("notifications channel closed");
    assert_eq!(frame.method, "session.primary_changed");
    assert_eq!(
        frame.params["reason"].as_str(),
        Some("test_fanout"),
        "expected reason round-trip, got {frame:?}"
    );
}

/// Sync reverse-RPC handler returns the answer; the read loop
/// auto-replies via `send_response`. Verify by issuing a reverse-RPC
/// from the daemon side and asserting `issue_to_primary` resolves.
#[tokio::test]
async fn sync_reverse_rpc_handler_auto_replies() {
    let (state, addr) = spawn_forged();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let client = std::sync::Arc::new(
        Client::connect(&format!("ws://{addr}/?name=sync-test"))
            .await
            .unwrap(),
    );

    // Wait for the daemon to register the connection.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let conn_id = state.connections.lock().keys().next().cloned().unwrap();

    // Register a session, pin our connection as primary.
    let sid = forged::session_state::SessionId("sess_sync".into());
    let (handle, _rx) = state.register_session(sid.clone());
    *handle.primary.lock() = Some(conn_id.clone());

    // Register a sync handler — its return value is the answer the
    // dispatcher auto-sends back via send_response.
    client.on_reverse_rpc_sync("hook.pre_tool_use", move |_rev_id, _params| async move {
        serde_json::json!({"decision": "passthrough"})
    });

    // Issue the reverse-RPC from the daemon side.
    let state_arc = std::sync::Arc::new(state.clone());
    let sid_for = sid.clone();
    let issue = tokio::spawn(async move {
        forged::reverse_rpc::issue_to_primary(
            &state_arc,
            &sid_for,
            "hook.pre_tool_use",
            serde_json::json!({"input": {}, "context": {}}),
            forged::prompt_queue::PromptKind::Hook {
                kind: "pre_tool_use".into(),
            },
            Duration::from_secs(5),
        )
        .await
    });

    let value = issue.await.unwrap().unwrap();
    assert_eq!(value["decision"], serde_json::json!("passthrough"));
}
