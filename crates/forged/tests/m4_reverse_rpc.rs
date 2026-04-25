//! M4 — reverse-RPC + pending prompts.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use forged::prompt_queue::{PendingPrompt, PromptKind, PromptQueue};

// ============================================================================
// M4.1 — PromptQueue data structure
// ============================================================================

#[test]
fn queue_starts_empty() {
    let q = PromptQueue::new();
    assert_eq!(q.snapshot().len(), 0);
}

#[test]
fn enqueue_then_take_by_id_returns_the_prompt_and_removes_it() {
    let q = PromptQueue::new();
    let (tx, _rx) = tokio::sync::oneshot::channel();
    let prompt = PendingPrompt {
        prompt_id: "prompt_1".into(),
        kind: PromptKind::Permission,
        issued_at: SystemTime::now(),
        expires_at: SystemTime::now() + Duration::from_secs(3600),
        params: serde_json::json!({}),
        responder: tx,
    };
    q.enqueue(prompt);
    assert_eq!(q.snapshot().len(), 1);
    let taken = q.take("prompt_1").unwrap();
    assert_eq!(taken.prompt_id, "prompt_1");
    assert_eq!(q.snapshot().len(), 0);
}

#[test]
fn snapshot_is_jsonable_with_iso8601_timestamps() {
    let q = PromptQueue::new();
    let (tx, _rx) = tokio::sync::oneshot::channel();
    let issued = SystemTime::now();
    let prompt = PendingPrompt {
        prompt_id: "prompt_iso".into(),
        kind: PromptKind::Hook {
            kind: "pre_tool_use".into(),
        },
        issued_at: issued,
        expires_at: issued + Duration::from_secs(3600),
        params: serde_json::json!({"foo": "bar"}),
        responder: tx,
    };
    q.enqueue(prompt);

    let json = serde_json::to_value(q.snapshot_for_wire()).unwrap();
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    let entry = &arr[0];
    let issued_str = entry["issued_at"].as_str().unwrap();
    assert!(
        issued_str.contains('T') && issued_str.contains('Z'),
        "expected ISO8601 with Zulu suffix, got {issued_str}"
    );
    assert_eq!(entry["kind"].as_str().unwrap(), "hook.pre_tool_use");
    assert_eq!(entry["params"]["foo"], serde_json::json!("bar"));
}

#[tokio::test]
async fn responder_oneshot_resolves_when_take_then_send() {
    let q = PromptQueue::new();
    let (tx, rx) = tokio::sync::oneshot::channel();
    q.enqueue(PendingPrompt {
        prompt_id: "prompt_resp".into(),
        kind: PromptKind::Permission,
        issued_at: SystemTime::now(),
        expires_at: SystemTime::now() + Duration::from_secs(3600),
        params: serde_json::json!({}),
        responder: tx,
    });
    let taken = q.take("prompt_resp").unwrap();
    taken
        .responder
        .send(serde_json::json!({"decision": "allow"}))
        .unwrap();

    let answer = rx.await.unwrap();
    assert_eq!(answer["decision"], serde_json::json!("allow"));
}

#[test]
fn take_unknown_id_returns_none() {
    let q = PromptQueue::new();
    assert!(q.take("does_not_exist").is_none());
}

#[test]
fn permission_kind_renders_as_permission_request_on_wire() {
    assert_eq!(PromptKind::Permission.as_wire(), "permission.request");
}

#[test]
fn hook_kind_renders_with_hook_dot_prefix_on_wire() {
    assert_eq!(
        PromptKind::Hook {
            kind: "post_tool_use".into()
        }
        .as_wire(),
        "hook.post_tool_use"
    );
}

// ============================================================================
// M4.2 — Reverse-RPC issuer + outstanding-id table
// ============================================================================

#[tokio::test]
async fn issue_to_primary_sends_request_and_resolves_on_response() {
    let state = Arc::new(forged::registry::DaemonState::new());

    // Register a fake connection with a captured outbound channel.
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel();
    let conn = forged::connection::Connection::new(
        forged::connection::ConnectionId("conn_test".into()),
        out_tx,
    );
    state.register_connection(conn.clone());

    // Register a session with conn as primary.
    let sid = forged::session_state::SessionId("sess_rev".into());
    let (handle, _rx) = state.register_session(sid.clone());
    *handle.primary.lock() = Some(conn.id.clone());

    // Spawn the issue call concurrently.
    let st_for_issue = state.clone();
    let sid_for_issue = sid.clone();
    let issue = tokio::spawn(async move {
        forged::reverse_rpc::issue_to_primary(
            &st_for_issue,
            &sid_for_issue,
            "permission.request",
            serde_json::json!({"foo": "bar"}),
            forged::prompt_queue::PromptKind::Permission,
            Duration::from_secs(3600),
        )
        .await
    });

    // Wait for the daemon to send the reverse-RPC request out.
    let outbound_frame = out_rx.recv().await.unwrap();
    let req = match outbound_frame {
        forged::connection::Outbound::Request(r) => r,
        other => panic!("expected Outbound::Request, got {other:?}"),
    };
    assert_eq!(req.method, "permission.request");
    let rev_id = req.id.as_str().unwrap().to_string();
    assert!(rev_id.starts_with("rev_"));

    // Simulate the client answering by injecting a response into the daemon.
    forged::reverse_rpc::resolve(&state, &rev_id, serde_json::json!({"decision": "allow"}));

    let result = issue.await.unwrap().unwrap();
    assert_eq!(result["decision"], serde_json::json!("allow"));
}

#[tokio::test]
async fn issue_with_no_primary_parks_in_queue() {
    let state = Arc::new(forged::registry::DaemonState::new());
    let sid = forged::session_state::SessionId("sess_no_primary".into());
    let (handle, _rx) = state.register_session(sid.clone());
    assert!(handle.primary.lock().is_none());

    // Issue with a short timeout so the test doesn't hang on an empty queue.
    let st = state.clone();
    let sid_for = sid.clone();
    let issue = tokio::spawn(async move {
        forged::reverse_rpc::issue_to_primary(
            &st,
            &sid_for,
            "permission.request",
            serde_json::json!({}),
            forged::prompt_queue::PromptKind::Permission,
            Duration::from_millis(150),
        )
        .await
    });

    // Give it a moment to enqueue.
    tokio::time::sleep(Duration::from_millis(20)).await;
    let snapshot = handle.prompts.snapshot();
    assert_eq!(snapshot.len(), 1, "expected one parked prompt");

    // After timeout fires, the prompt is purged from the queue and the
    // call returns an error.
    let r = issue.await.unwrap();
    assert!(r.is_err(), "expected timeout: {r:?}");
    assert_eq!(
        handle.prompts.snapshot().len(),
        0,
        "queue should be drained on timeout"
    );
}

#[tokio::test]
async fn issue_unknown_session_returns_session_not_found() {
    let state = Arc::new(forged::registry::DaemonState::new());
    let sid = forged::session_state::SessionId("sess_does_not_exist".into());
    let r = forged::reverse_rpc::issue_to_primary(
        &state,
        &sid,
        "permission.request",
        serde_json::json!({}),
        forged::prompt_queue::PromptKind::Permission,
        Duration::from_secs(1),
    )
    .await;
    let err = r.unwrap_err();
    assert!(
        matches!(err, forged::Error::SessionNotFound(_)),
        "expected SessionNotFound, got {err:?}"
    );
}

#[tokio::test]
async fn timeout_emits_prompts_expired_to_subscribers() {
    let state = Arc::new(forged::registry::DaemonState::new());
    let sid = forged::session_state::SessionId("sess_timeout".into());
    let (handle, _rx) = state.register_session(sid.clone());

    // Subscribe a fake connection so we can observe `prompts.expired`.
    let (sub_tx, mut sub_rx) = tokio::sync::mpsc::unbounded_channel();
    let sub_conn = forged::connection::Connection::new(
        forged::connection::ConnectionId("conn_sub".into()),
        sub_tx,
    );
    state.register_connection(sub_conn.clone());
    handle.subscribers.lock().push(sub_conn.id.clone());

    let r = forged::reverse_rpc::issue_to_primary(
        &state,
        &sid,
        "permission.request",
        serde_json::json!({}),
        forged::prompt_queue::PromptKind::Permission,
        Duration::from_millis(100),
    )
    .await;
    assert!(r.is_err(), "expected timeout");

    // The subscriber should have received a `prompts.expired` notification.
    let frame = sub_rx.recv().await.unwrap();
    let n = match frame {
        forged::connection::Outbound::Notification(n) => n,
        other => panic!("expected Notification, got {other:?}"),
    };
    assert_eq!(n.method, "prompts.expired");
    let p = n.params.unwrap();
    assert_eq!(p["session_id"], serde_json::json!("sess_timeout"));
    assert!(p["prompt_id"].as_str().unwrap().starts_with("prompt_"));
    assert_eq!(p["fallback"], serde_json::json!("deny"));
}

// ============================================================================
// M4.5 — prompts.respond + subscribe surfaces queue
// ============================================================================

#[tokio::test]
async fn prompts_respond_resolves_a_queued_prompt() {
    let state = forged::registry::DaemonState::new();
    let sid = forged::session_state::SessionId("sess_park".into());
    let (handle, _rx) = state.register_session(sid.clone());

    // Park a prompt manually with a oneshot we can await.
    let (tx, rx) = tokio::sync::oneshot::channel();
    handle.prompts.enqueue(forged::prompt_queue::PendingPrompt {
        prompt_id: "prompt_xyz".into(),
        kind: forged::prompt_queue::PromptKind::Permission,
        issued_at: SystemTime::now(),
        expires_at: SystemTime::now() + Duration::from_secs(3600),
        params: serde_json::json!({}),
        responder: tx,
    });

    forged::methods::prompts::respond(
        &state,
        &sid,
        "prompt_xyz",
        serde_json::json!({"decision": "allow"}),
    )
    .unwrap();

    let value = rx.await.unwrap();
    assert_eq!(value["decision"], serde_json::json!("allow"));
    assert_eq!(handle.prompts.snapshot().len(), 0);
}

#[test]
fn prompts_respond_unknown_prompt_id_returns_invalid_params() {
    let state = forged::registry::DaemonState::new();
    let sid = forged::session_state::SessionId("sess_park2".into());
    let _kept = state.register_session(sid.clone());

    let err = forged::methods::prompts::respond(
        &state,
        &sid,
        "prompt_does_not_exist",
        serde_json::json!({}),
    )
    .unwrap_err();
    assert!(
        matches!(err, forged::Error::InvalidParams(_)),
        "expected InvalidParams, got {err:?}"
    );
}

#[test]
fn prompts_respond_unknown_session_returns_session_not_found() {
    let state = forged::registry::DaemonState::new();
    let sid = forged::session_state::SessionId("sess_unknown".into());
    let err =
        forged::methods::prompts::respond(&state, &sid, "p", serde_json::json!({})).unwrap_err();
    assert!(
        matches!(err, forged::Error::SessionNotFound(_)),
        "expected SessionNotFound, got {err:?}"
    );
}

#[test]
fn snapshot_for_wire_includes_pending_prompts() {
    let state = forged::registry::DaemonState::new();
    let sid = forged::session_state::SessionId("sess_pend".into());
    let (handle, _rx) = state.register_session(sid.clone());
    let (tx, _rx2) = tokio::sync::oneshot::channel();
    handle.prompts.enqueue(forged::prompt_queue::PendingPrompt {
        prompt_id: "prompt_q".into(),
        kind: forged::prompt_queue::PromptKind::Hook {
            kind: "pre_tool_use".into(),
        },
        issued_at: SystemTime::now(),
        expires_at: SystemTime::now() + Duration::from_secs(3600),
        params: serde_json::json!({"tool_name": "Bash"}),
        responder: tx,
    });

    let snapshot = handle.prompts.snapshot_for_wire();
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].kind, "hook.pre_tool_use");
    assert_eq!(snapshot[0].params["tool_name"], serde_json::json!("Bash"));
}

#[test]
fn subscribe_returns_pending_prompts_in_response() {
    use forged::methods::session::{SubscribeResult, subscribe};

    let state = forged::registry::DaemonState::new();
    let sid = forged::session_state::SessionId("sess_sub".into());
    let (handle, _rx) = state.register_session(sid.clone());

    // Park a prompt.
    let (tx, _rx2) = tokio::sync::oneshot::channel();
    handle.prompts.enqueue(forged::prompt_queue::PendingPrompt {
        prompt_id: "prompt_for_subscribe".into(),
        kind: forged::prompt_queue::PromptKind::Permission,
        issued_at: SystemTime::now(),
        expires_at: SystemTime::now() + Duration::from_secs(3600),
        params: serde_json::json!({"tool_name": "Edit"}),
        responder: tx,
    });

    // Build a fake connection (subscribe needs one).
    let (out_tx, _out_rx) = tokio::sync::mpsc::unbounded_channel();
    let conn = forged::connection::Connection::new(
        forged::connection::ConnectionId("conn_sub".into()),
        out_tx,
    );

    let SubscribeResult {
        pending_prompts, ..
    } = subscribe(&state, &conn, &sid, None).unwrap();

    assert_eq!(pending_prompts.len(), 1);
    assert_eq!(
        pending_prompts[0]["prompt_id"],
        serde_json::json!("prompt_for_subscribe")
    );
    assert_eq!(
        pending_prompts[0]["kind"],
        serde_json::json!("permission.request")
    );
}

// ============================================================================
// M4 — reverse-RPC error response variant tests (fix #5/#10)
//
// Spec: when the answering client returns `{"error": {...}}` the server
// dispatches via `resolve_error`, which wraps the error as
// `{"_jsonrpc_error": <error-obj>}` so the SDK bridges can map to a
// typed deny with reason. The daemon must distinguish "client denied"
// from "client errored" in operator logs.
// ============================================================================

#[tokio::test]
async fn rev_error_response_resolves_with_typed_jsonrpc_error_sentinel() {
    use forged::registry::OutstandingEntry;
    let state = Arc::new(forged::registry::DaemonState::new());
    let sid = forged::session_state::SessionId("sess_err_1".into());
    let _kept = state.register_session(sid.clone());
    let (tx, rx) = tokio::sync::oneshot::channel();
    state.outstanding_reverse.lock().insert(
        "rev_e1".into(),
        OutstandingEntry {
            session_id: sid,
            conn_id: None,
            responder: tx,
        },
    );

    forged::reverse_rpc::resolve_error(
        &state,
        "rev_e1",
        &serde_json::json!({"code": -32601, "message": "method not found"}),
    );

    let v = rx.await.unwrap();
    let err = v
        .get("_jsonrpc_error")
        .expect("missing _jsonrpc_error sentinel");
    assert_eq!(err["code"], serde_json::json!(-32601));
    assert_eq!(err["message"], serde_json::json!("method not found"));
}

#[tokio::test]
async fn rev_error_with_arbitrary_code_carries_through_unchanged() {
    use forged::registry::OutstandingEntry;
    let state = Arc::new(forged::registry::DaemonState::new());
    let sid = forged::session_state::SessionId("sess_err_2".into());
    let _kept = state.register_session(sid.clone());
    let (tx, rx) = tokio::sync::oneshot::channel();
    state.outstanding_reverse.lock().insert(
        "rev_e2".into(),
        OutstandingEntry {
            session_id: sid,
            conn_id: None,
            responder: tx,
        },
    );

    forged::reverse_rpc::resolve_error(
        &state,
        "rev_e2",
        &serde_json::json!({"code": -1, "message": "x"}),
    );

    let v = rx.await.unwrap();
    let err = v.get("_jsonrpc_error").unwrap();
    assert_eq!(err["code"], serde_json::json!(-1));
}

#[tokio::test]
async fn rev_value_null_resolves_to_typed_deny_via_unknown_decision() {
    // Value::Null is not a `_jsonrpc_error`, not a `_client_disconnected`,
    // not a `_session_closed` sentinel, and not an `{decision: ...}` shape.
    // The bridge must surface it as a deny with "unknown decision".
    let value = serde_json::Value::Null;
    let decision = decode_perm_decision_from_value(&value);
    assert!(!decision.is_allow(), "expected deny");
}

#[tokio::test]
async fn rev_value_decision_42_resolves_to_typed_deny() {
    // {"decision": 42} — string-required field is a number.
    let value = serde_json::json!({"decision": 42});
    let decision = decode_perm_decision_from_value(&value);
    assert!(!decision.is_allow());
}

#[tokio::test]
async fn rev_value_empty_object_resolves_to_typed_deny() {
    // {} — no decision key at all. Wire shape default is "deny".
    let value = serde_json::json!({});
    let decision = decode_perm_decision_from_value(&value);
    assert!(!decision.is_allow());
}

#[tokio::test]
async fn rev_value_missing_decision_key_resolves_to_typed_deny() {
    // {"reason": "..."} — only `reason`, no `decision` key.
    let value = serde_json::json!({"reason": "no decision present"});
    let decision = decode_perm_decision_from_value(&value);
    assert!(!decision.is_allow());
    assert_eq!(decision.reason(), Some("no decision present"));
}

/// Test helper: replays the permission-bridge `decode_*` logic
/// against a raw value and returns the resulting [`PermissionDecision`].
/// Mirrors the wire-shape branch in `ForgedPermissionBridge::issue_and_decode`
/// without going through reverse-RPC issuance.
fn decode_perm_decision_from_value(value: &serde_json::Value) -> forge_sdk::PermissionDecision {
    use forge_sdk::PermissionDecision;
    if let Some(err) = value.get("_jsonrpc_error") {
        let code = err
            .get("code")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(-1);
        let message = err
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        return PermissionDecision::deny(format!("client error {code}: {message}"));
    }
    if value.get("_client_disconnected").is_some() {
        return PermissionDecision::deny(String::from(
            "answering client disconnected before responding",
        ));
    }
    if value.get("_session_closed").is_some() {
        return PermissionDecision::deny(String::from("session closed before prompt answered"));
    }
    let decision_str = value
        .get("decision")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("deny");
    let updated_input = value.get("updated_input").cloned();
    let reason = value
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .map(String::from);
    match (decision_str, updated_input) {
        ("allow", None) => PermissionDecision::allow(),
        ("allow", Some(updates)) => PermissionDecision::allow_with_input(updates),
        ("deny", _) => PermissionDecision::deny(reason.unwrap_or_default()),
        (other, _) => PermissionDecision::deny(format!("unknown decision: {other}")),
    }
}

// ============================================================================
// Fix #4: prompts.expired on disconnect
// ============================================================================

/// When the answering primary disconnects mid-prompt, the parked
/// reverse-RPC must unblock within ~50ms (synthetic
/// `_client_disconnected` answer), NOT the full 1h timeout.
#[tokio::test]
async fn outstanding_reverse_unblocks_when_answering_conn_disconnects() {
    use forged::registry::OutstandingEntry;
    let state = Arc::new(forged::registry::DaemonState::new());
    let sid = forged::session_state::SessionId("sess_disc".into());
    let _kept = state.register_session(sid.clone());

    let (out_tx, _out_rx) = tokio::sync::mpsc::unbounded_channel();
    let conn = forged::connection::Connection::new(
        forged::connection::ConnectionId("conn_disc".into()),
        out_tx,
    );
    state.register_connection(conn.clone());

    // Manually wire an OutstandingEntry whose conn_id points at this
    // conn — mimicking what `try_send_to_primary` would do once the
    // request is in flight.
    let (tx, rx) = tokio::sync::oneshot::channel();
    state.outstanding_reverse.lock().insert(
        "rev_disc".into(),
        OutstandingEntry {
            session_id: sid.clone(),
            conn_id: Some(conn.id.clone()),
            responder: tx,
        },
    );

    // Disconnect — this must drain the outstanding entry and resolve
    // the responder with the synthetic `_client_disconnected` answer.
    let _ = state.unregister_connection(&conn.id);

    let value = tokio::time::timeout(Duration::from_millis(50), rx)
        .await
        .expect("disconnect did not unblock outstanding rev within 50ms")
        .expect("oneshot dropped without value");
    assert_eq!(
        value.get("_client_disconnected"),
        Some(&serde_json::json!(true)),
        "expected synthetic _client_disconnected answer, got {value}"
    );
}

// ============================================================================
// End-to-end: WS dispatch routes inbound rev_ responses to the resolver
// ============================================================================

#[tokio::test]
async fn ws_response_with_rev_id_resolves_outstanding_reverse_rpc() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message as WsMsg;

    // Bring up a real server bound to an ephemeral port.
    let state = forged::registry::DaemonState::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(forged::server::run(listener, state.clone()));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let url = format!("ws://{addr}/");
    let (mut ws, _) = connect_async(&url).await.unwrap();

    // Drain the initial client.identify notification so it doesn't
    // confuse later asserts.
    let WsMsg::Text(t) = ws.next().await.unwrap().unwrap() else {
        panic!("expected text frame")
    };
    assert!(t.contains("client.identify"));

    // The connection id is fresh per ws upgrade. Find it on the server.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let conn_id = state.connections.lock().keys().next().cloned().unwrap();

    // Manually register a session and mark this connection as primary.
    let sid = forged::session_state::SessionId("sess_e2e".into());
    let (handle, _rx) = state.register_session(sid.clone());
    *handle.primary.lock() = Some(conn_id.clone());

    // Fire issue_to_primary in the background; the daemon will forward
    // the request over the WS to our test client.
    let state_arc = Arc::new(state.clone());
    let sid_for = sid.clone();
    let issue = tokio::spawn(async move {
        forged::reverse_rpc::issue_to_primary(
            &state_arc,
            &sid_for,
            "permission.request",
            serde_json::json!({"hello": "world"}),
            forged::prompt_queue::PromptKind::Permission,
            Duration::from_secs(5),
        )
        .await
    });

    // Receive the reverse-RPC frame the daemon issued.
    let WsMsg::Text(t) = ws.next().await.unwrap().unwrap() else {
        panic!("expected text frame")
    };
    let v: serde_json::Value = serde_json::from_str(&t).unwrap();
    assert_eq!(v["method"], serde_json::json!("permission.request"));
    let rev_id = v["id"].as_str().unwrap().to_string();
    assert!(rev_id.starts_with("rev_"), "expected rev_ id, got {rev_id}");

    // Send the response back over the same WS.
    let resp = serde_json::json!({
        "jsonrpc": "2.0",
        "id": rev_id,
        "result": {"decision": "allow"}
    });
    ws.send(WsMsg::Text(resp.to_string())).await.unwrap();

    // The issuing future should now resolve.
    let value = issue.await.unwrap().unwrap();
    assert_eq!(value["decision"], serde_json::json!("allow"));
}

#[tokio::test]
async fn ws_prompts_respond_resolves_queued_prompt_end_to_end() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message as WsMsg;

    let state = forged::registry::DaemonState::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(forged::server::run(listener, state.clone()));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let url = format!("ws://{addr}/");
    let (mut ws, _) = connect_async(&url).await.unwrap();

    // Drain client.identify
    let _ = ws.next().await;

    // Register a session with no primary so the prompt parks in the queue.
    let sid = forged::session_state::SessionId("sess_park_e2e".into());
    let (handle, _rx) = state.register_session(sid.clone());

    // Park a prompt manually.
    let (tx, rx) = tokio::sync::oneshot::channel();
    handle.prompts.enqueue(forged::prompt_queue::PendingPrompt {
        prompt_id: "prompt_e2e".into(),
        kind: forged::prompt_queue::PromptKind::Permission,
        issued_at: SystemTime::now(),
        expires_at: SystemTime::now() + Duration::from_secs(3600),
        params: serde_json::json!({}),
        responder: tx,
    });

    // Send `prompts.respond`.
    let req = forged::jsonrpc::Request::new(
        "prompts.respond",
        serde_json::json!({
            "session_id": "sess_park_e2e",
            "prompt_id": "prompt_e2e",
            "result": {"decision": "allow"},
        }),
        serde_json::json!(7),
    );
    ws.send(WsMsg::Text(serde_json::to_string(&req).unwrap()))
        .await
        .unwrap();

    // Drain until we see the response with id 7.
    loop {
        let WsMsg::Text(t) = ws.next().await.unwrap().unwrap() else {
            continue;
        };
        let v: serde_json::Value = serde_json::from_str(&t).unwrap();
        if v.get("id") == Some(&serde_json::json!(7)) {
            assert!(
                v["result"].is_null(),
                "expected null result, got {}",
                v["result"]
            );
            break;
        }
    }

    // The oneshot should resolve with the answer we sent.
    let answer = rx.await.unwrap();
    assert_eq!(answer["decision"], serde_json::json!("allow"));
    assert_eq!(handle.prompts.snapshot().len(), 0);
}
