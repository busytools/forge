//! M7.6 — end-to-end reverse-RPC permission flow through the forge-tui
//! `Client`.
//!
//! 1. Daemon issues `permission.request` to the registered primary client.
//! 2. The `Client::on_reverse_rpc_deferred` handler captures the `rev_id`
//!    and forwards it through a oneshot channel (mimicking what `main.rs`
//!    does, except the answer comes from the test rather than a keypress).
//! 3. `Client::send_response(rev_id, ...)` ships the answer back.
//! 4. The daemon's `issue_to_primary` future resolves with the answer.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use forge_tui::client::Client;
use parking_lot::Mutex;

#[tokio::test]
async fn fresh_permission_request_round_trips_through_send_response() {
    let state = forged::registry::DaemonState::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
    tokio::spawn(forged::server::run(listener, state.clone()));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let client = Arc::new(
        Client::connect(&format!("ws://{addr}/?name=tui-e2e"))
            .await
            .unwrap(),
    );

    // Wait until the daemon has registered the connection. Replaces a
    // previous timing-dependent `sleep` with a deterministic poll loop
    // so the test is stable under load.
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

    // Register a session and pin our connection as the primary.
    let sid = forged::session_state::SessionId("sess_tui_e2e".into());
    let (handle, _rx) = state.register_session(sid.clone());
    *handle.primary.lock() = Some(conn_id.clone());

    // Slot for the captured rev_id, so the test (acting as the keypress
    // handler) can later answer with `Client::send_response`.
    let captured: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
    {
        let captured = captured.clone();
        client.on_reverse_rpc_deferred("permission.request", move |rev_id, _params| {
            let captured = captured.clone();
            async move {
                *captured.lock() = Some(rev_id);
                // The answer flows back via send_response below — the
                // _deferred handler intentionally returns no value.
            }
        });
    }

    // Issue the reverse-RPC from the daemon side.
    let state_arc = Arc::new(state.clone());
    let sid_for = sid.clone();
    let issue = tokio::spawn(async move {
        forged::reverse_rpc::issue_to_primary(
            &state_arc,
            &sid_for,
            "permission.request",
            serde_json::json!({"tool_name": "Bash", "tool_input": {"command": "ls"}}),
            forged::prompt_queue::PromptKind::Permission,
            Duration::from_secs(5),
        )
        .await
    });

    // Wait for the handler to capture the rev_id.
    let rev_id = {
        let mut tries = 0;
        loop {
            if let Some(v) = captured.lock().clone() {
                break v;
            }
            tries += 1;
            assert!(
                tries <= 50,
                "forge-tui Client never received the reverse-RPC"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };

    // Answer via send_response (the codepath input::answer_permission uses
    // for fresh reverse-RPCs).
    client
        .send_response(rev_id, serde_json::json!({"decision": "allow"}))
        .unwrap();

    let value = issue.await.unwrap().unwrap();
    assert_eq!(value["decision"], serde_json::json!("allow"));
}
