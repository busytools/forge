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
use std::time::{Duration, SystemTime};

use crossterm::event::KeyCode;
use forge_daemon::prompt_queue::{PendingPrompt, PromptKind};
use forge_tui::app::{App, Focus, PendingPermission};
use forge_tui::client::Client;
use parking_lot::Mutex;
use tokio::sync::mpsc;

#[tokio::test]
async fn fresh_permission_request_round_trips_through_send_response() {
    let state = forge_daemon::registry::DaemonState::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
    tokio::spawn(forge_daemon::server::run(listener, state.clone()));
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
    let sid = forge_daemon::session_state::SessionId("sess_tui_e2e".into());
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

/// Round 4 — fix I1 regression test. Cross-session permission routing.
///
/// Locks the contract that `input::answer_permission` routes
/// `prompts.respond` on the `session_id` carried in the prompt's params,
/// NOT on `app.current_session`. The user can be primary on multiple
/// sessions simultaneously; viewing session A while a permission modal
/// fires for session B must answer B (the originator), not A.
///
/// Prior to the fix, the modal used `app.current_session.clone()` as the
/// `session_id` for `prompts.respond`, so a user viewing session A while
/// a queued prompt fires from session B would fan their answer to A,
/// where the daemon would return `InvalidParams("prompt_id ... not in
/// queue")` and the originating B-side SDK callback would wait the full
/// 1-hour timeout.
#[tokio::test]
async fn answer_permission_routes_to_params_session_not_current_session() {
    let state = forge_daemon::registry::DaemonState::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
    tokio::spawn(forge_daemon::server::run(listener, state.clone()));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let client = Arc::new(
        Client::connect(&format!("ws://{addr}/?name=tui-cross-session"))
            .await
            .unwrap(),
    );

    // Wait for the daemon to register our connection.
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

    // Register two sessions; pin our connection as primary on BOTH.
    // Mimics the production case where one TUI client is primary on
    // multiple concurrent sessions.
    let sid_a = forge_daemon::session_state::SessionId("sess_A".into());
    let sid_b = forge_daemon::session_state::SessionId("sess_B".into());
    let (handle_a, _rx_a) = state.register_session(sid_a.clone());
    let (handle_b, _rx_b) = state.register_session(sid_b.clone());
    *handle_a.primary.lock() = Some(conn_id.clone());
    *handle_b.primary.lock() = Some(conn_id.clone());

    // Issue a `permission.request` on session B. The daemon routes it
    // through the primary (us); the rev path captures the rev_id and
    // would normally let `Client::send_response` resolve it. For this
    // test, we want the queued-prompt code path
    // (`prompt_id` is set in `PendingPermission`), which is what
    // `app.rs`'s `PermissionRequest` handler does when params carry a
    // `prompt_id`.
    //
    // Instead of going through reverse-RPC issuance and forcing the
    // queued path on the daemon side, park a prompt directly on
    // session B's queue and synthesize a `PendingPermission` for it.
    // This is exactly what the TUI sees after a reconnect: the
    // `session.subscribe` response surfaces queued prompts and the app
    // builds a `PendingPermission { rev_id, params, prompt_id: Some }`.
    let prompt_id_b = "prompt_under_test_for_B";
    let rev_id_b = "rev_under_test_for_B";

    // Park the prompt + record the outstanding-reverse entry so
    // `prompts.respond` resolves it. Mirrors `reverse_rpc::park_in_queue`
    // but inlined here so the test owns the responder receiver and can
    // assert which session the answer landed on.
    let (responder_tx, responder_rx) = tokio::sync::oneshot::channel::<serde_json::Value>();
    let (queue_tx, _queue_rx) = tokio::sync::oneshot::channel::<serde_json::Value>();
    handle_b.prompts.enqueue(PendingPrompt {
        prompt_id: prompt_id_b.into(),
        kind: PromptKind::Permission,
        issued_at: SystemTime::now(),
        expires_at: SystemTime::now() + Duration::from_secs(60),
        params: serde_json::json!({"tool_name": "Bash", "tool_input": {"command": "ls"}}),
        responder: queue_tx,
        rev_id: Some(rev_id_b.into()),
    });
    state.outstanding_reverse.lock().insert(
        rev_id_b.into(),
        forge_daemon::registry::OutstandingEntry {
            session_id: sid_b.clone(),
            conn_id: None,
            prompt_id: prompt_id_b.into(),
            responder: responder_tx,
        },
    );

    // Build the App as the user would see it after subscribing to A
    // and then receiving a queued-prompt PermissionRequest for B. The
    // params carry the daemon's envelope: session_id=B, prompt_id=B's
    // prompt id.
    let mut app = App::default();
    app.current_session = Some("sess_A".into()); // user is viewing A
    app.focus = Focus::PermissionModal;
    app.pending_permission = Some(PendingPermission::new(
        serde_json::Value::Null,
        serde_json::json!({
            "session_id": "sess_B",   // daemon-attached envelope: prompt is B's
            "prompt_id": prompt_id_b,
            "tool_name": "Bash",
            "tool_input": {"command": "ls"},
        }),
        Some(prompt_id_b.into()),
    ));

    // Drive the keypress through the public input handler. `event_tx`
    // is unused for the permission-modal path but the signature
    // requires it.
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let quit = forge_tui::input::handle_key(&mut app, KeyCode::Char('a'), &client, &event_tx).await;
    assert_eq!(quit, Some(false), "permission allow must not quit the loop");

    // The daemon's `prompts.respond` handler runs synchronously in the
    // dispatcher; the TUI's `client.call` returns once the daemon
    // sends back the response. By the time `handle_key` returns, the
    // outstanding-reverse entry should have been resolved with our
    // `{"decision": "allow"}` answer.
    let answer = tokio::time::timeout(Duration::from_secs(2), responder_rx)
        .await
        .expect("daemon did not resolve the responder within 2s")
        .expect("responder channel was dropped without an answer");
    assert_eq!(
        answer["decision"],
        serde_json::json!("allow"),
        "session B's responder must have received our 'allow' answer; got {answer}"
    );

    // Session A's queue must be empty — the modal must NOT have
    // misrouted the answer to A. (Pre-fix, `app.current_session` was
    // "sess_A" so `prompts.respond` would have hit A and returned
    // `InvalidParams("prompt_id ... not in queue")`, leaving B's
    // responder forever stuck.)
    assert_eq!(
        handle_a.prompts.snapshot().len(),
        0,
        "session A had no parked prompt and must remain so"
    );
    assert_eq!(
        handle_b.prompts.snapshot().len(),
        0,
        "session B's prompt must have been consumed by prompts.respond"
    );

    // Status message must NOT show a routing error.
    assert!(
        app.status_msg.is_empty(),
        "answer_permission must not surface an error; got: {}",
        app.status_msg
    );
}
