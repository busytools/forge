//! M4 — reverse-RPC + pending prompts.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use forge_daemon::prompt_queue::{PendingPrompt, PromptKind, PromptQueue};

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
        rev_id: None,
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
        rev_id: None,
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
        rev_id: None,
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
    let state = Arc::new(forge_daemon::registry::DaemonState::new());

    // Register a fake connection with a captured outbound channel.
    let (out_tx, mut out_rx) =
        tokio::sync::mpsc::channel(forge_daemon::connection::OUTBOUND_CHANNEL_CAPACITY);
    let conn = forge_daemon::connection::Connection::new(
        forge_daemon::connection::ConnectionId("conn_test".into()),
        out_tx,
    );
    state.register_connection(conn.clone());

    // Register a session with conn as primary.
    let sid = forge_daemon::session_state::SessionId("sess_rev".into());
    let (handle, _rx) = state.register_session(sid.clone());
    *handle.primary.lock() = Some(conn.id.clone());

    // Spawn the issue call concurrently.
    let st_for_issue = state.clone();
    let sid_for_issue = sid.clone();
    let issue = tokio::spawn(async move {
        forge_daemon::reverse_rpc::issue_to_primary(
            &st_for_issue,
            &sid_for_issue,
            "permission.request",
            serde_json::json!({"foo": "bar"}),
            forge_daemon::prompt_queue::PromptKind::Permission,
            Duration::from_secs(3600),
        )
        .await
    });

    // Wait for the daemon to send the reverse-RPC request out.
    let outbound_frame = out_rx.recv().await.unwrap();
    let req = match outbound_frame {
        forge_daemon::connection::Outbound::Request(r) => r,
        other => panic!("expected Outbound::Request, got {other:?}"),
    };
    assert_eq!(req.method, "permission.request");
    let rev_id = req.id.as_str().unwrap().to_string();
    assert!(rev_id.starts_with("rev_"));

    // Simulate the client answering by injecting a response into the daemon.
    forge_daemon::reverse_rpc::resolve(&state, &rev_id, serde_json::json!({"decision": "allow"}));

    let result = issue.await.unwrap().unwrap();
    assert_eq!(result["decision"], serde_json::json!("allow"));
}

#[tokio::test]
async fn issue_with_no_primary_parks_in_queue() {
    let state = Arc::new(forge_daemon::registry::DaemonState::new());
    let sid = forge_daemon::session_state::SessionId("sess_no_primary".into());
    let (handle, _rx) = state.register_session(sid.clone());
    assert!(handle.primary.lock().is_none());

    // Issue with a short timeout so the test doesn't hang on an empty queue.
    let st = state.clone();
    let sid_for = sid.clone();
    let issue = tokio::spawn(async move {
        forge_daemon::reverse_rpc::issue_to_primary(
            &st,
            &sid_for,
            "permission.request",
            serde_json::json!({}),
            forge_daemon::prompt_queue::PromptKind::Permission,
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
    let state = Arc::new(forge_daemon::registry::DaemonState::new());
    let sid = forge_daemon::session_state::SessionId("sess_does_not_exist".into());
    let r = forge_daemon::reverse_rpc::issue_to_primary(
        &state,
        &sid,
        "permission.request",
        serde_json::json!({}),
        forge_daemon::prompt_queue::PromptKind::Permission,
        Duration::from_secs(1),
    )
    .await;
    let err = r.unwrap_err();
    assert!(
        matches!(err, forge_daemon::Error::SessionNotFound(_)),
        "expected SessionNotFound, got {err:?}"
    );
}

#[tokio::test]
async fn timeout_emits_prompts_expired_to_subscribers() {
    let state = Arc::new(forge_daemon::registry::DaemonState::new());
    let sid = forge_daemon::session_state::SessionId("sess_timeout".into());
    let (handle, _rx) = state.register_session(sid.clone());

    // Subscribe a fake connection so we can observe `prompts.expired`.
    let (sub_tx, mut sub_rx) =
        tokio::sync::mpsc::channel(forge_daemon::connection::OUTBOUND_CHANNEL_CAPACITY);
    let sub_conn = forge_daemon::connection::Connection::new(
        forge_daemon::connection::ConnectionId("conn_sub".into()),
        sub_tx,
    );
    state.register_connection(sub_conn.clone());
    handle.subscribers.lock().push(sub_conn.id.clone());

    let r = forge_daemon::reverse_rpc::issue_to_primary(
        &state,
        &sid,
        "permission.request",
        serde_json::json!({}),
        forge_daemon::prompt_queue::PromptKind::Permission,
        Duration::from_millis(100),
    )
    .await;
    assert!(r.is_err(), "expected timeout");

    // The subscriber should have received a `prompts.expired` notification.
    let frame = sub_rx.recv().await.unwrap();
    let n = match frame {
        forge_daemon::connection::Outbound::Notification(n) => n,
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
    let state = forge_daemon::registry::DaemonState::new();
    let sid = forge_daemon::session_state::SessionId("sess_park".into());
    let (handle, _rx) = state.register_session(sid.clone());

    // Park a prompt manually with a oneshot we can await.
    let (tx, rx) = tokio::sync::oneshot::channel();
    handle
        .prompts
        .enqueue(forge_daemon::prompt_queue::PendingPrompt {
            prompt_id: "prompt_xyz".into(),
            kind: forge_daemon::prompt_queue::PromptKind::Permission,
            issued_at: SystemTime::now(),
            expires_at: SystemTime::now() + Duration::from_secs(3600),
            params: serde_json::json!({}),
            responder: tx,
            rev_id: None,
        });

    forge_daemon::methods::prompts::respond(
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
    let state = forge_daemon::registry::DaemonState::new();
    let sid = forge_daemon::session_state::SessionId("sess_park2".into());
    let _kept = state.register_session(sid.clone());

    let err = forge_daemon::methods::prompts::respond(
        &state,
        &sid,
        "prompt_does_not_exist",
        serde_json::json!({}),
    )
    .unwrap_err();
    assert!(
        matches!(err, forge_daemon::Error::InvalidParams(_)),
        "expected InvalidParams, got {err:?}"
    );
}

#[test]
fn prompts_respond_unknown_session_returns_session_not_found() {
    let state = forge_daemon::registry::DaemonState::new();
    let sid = forge_daemon::session_state::SessionId("sess_unknown".into());
    let err = forge_daemon::methods::prompts::respond(&state, &sid, "p", serde_json::json!({}))
        .unwrap_err();
    assert!(
        matches!(err, forge_daemon::Error::SessionNotFound(_)),
        "expected SessionNotFound, got {err:?}"
    );
}

#[test]
fn snapshot_for_wire_includes_pending_prompts() {
    let state = forge_daemon::registry::DaemonState::new();
    let sid = forge_daemon::session_state::SessionId("sess_pend".into());
    let (handle, _rx) = state.register_session(sid.clone());
    let (tx, _rx2) = tokio::sync::oneshot::channel();
    handle
        .prompts
        .enqueue(forge_daemon::prompt_queue::PendingPrompt {
            prompt_id: "prompt_q".into(),
            kind: forge_daemon::prompt_queue::PromptKind::Hook {
                kind: "pre_tool_use".into(),
            },
            issued_at: SystemTime::now(),
            expires_at: SystemTime::now() + Duration::from_secs(3600),
            params: serde_json::json!({"tool_name": "Bash"}),
            responder: tx,
            rev_id: None,
        });

    let snapshot = handle.prompts.snapshot_for_wire();
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].kind, "hook.pre_tool_use");
    assert_eq!(snapshot[0].params["tool_name"], serde_json::json!("Bash"));
}

#[test]
fn subscribe_returns_pending_prompts_in_response() {
    use forge_daemon::methods::session::{SubscribeResult, subscribe};

    let state = forge_daemon::registry::DaemonState::new();
    let sid = forge_daemon::session_state::SessionId("sess_sub".into());
    let (handle, _rx) = state.register_session(sid.clone());

    // Park a prompt.
    let (tx, _rx2) = tokio::sync::oneshot::channel();
    handle
        .prompts
        .enqueue(forge_daemon::prompt_queue::PendingPrompt {
            prompt_id: "prompt_for_subscribe".into(),
            kind: forge_daemon::prompt_queue::PromptKind::Permission,
            issued_at: SystemTime::now(),
            expires_at: SystemTime::now() + Duration::from_secs(3600),
            params: serde_json::json!({"tool_name": "Edit"}),
            responder: tx,
            rev_id: None,
        });

    // Build a fake connection (subscribe needs one).
    let (out_tx, _out_rx) =
        tokio::sync::mpsc::channel(forge_daemon::connection::OUTBOUND_CHANNEL_CAPACITY);
    let conn = forge_daemon::connection::Connection::new(
        forge_daemon::connection::ConnectionId("conn_sub".into()),
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
    use forge_daemon::registry::OutstandingEntry;
    let state = Arc::new(forge_daemon::registry::DaemonState::new());
    let sid = forge_daemon::session_state::SessionId("sess_err_1".into());
    let _kept = state.register_session(sid.clone());
    let (tx, rx) = tokio::sync::oneshot::channel();
    state.outstanding_reverse.lock().insert(
        "rev_e1".into(),
        OutstandingEntry {
            session_id: sid,
            conn_id: None,
            prompt_id: "prompt_e1".into(),
            responder: tx,
        },
    );

    forge_daemon::reverse_rpc::resolve_error(
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
    use forge_daemon::registry::OutstandingEntry;
    let state = Arc::new(forge_daemon::registry::DaemonState::new());
    let sid = forge_daemon::session_state::SessionId("sess_err_2".into());
    let _kept = state.register_session(sid.clone());
    let (tx, rx) = tokio::sync::oneshot::channel();
    state.outstanding_reverse.lock().insert(
        "rev_e2".into(),
        OutstandingEntry {
            session_id: sid,
            conn_id: None,
            prompt_id: "prompt_e2".into(),
            responder: tx,
        },
    );

    forge_daemon::reverse_rpc::resolve_error(
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
    let decision = forge_daemon::sdk_callbacks::decode_permission_response(&value);
    assert!(!decision.is_allow(), "expected deny");
}

#[tokio::test]
async fn rev_value_decision_42_resolves_to_typed_deny() {
    // {"decision": 42} — string-required field is a number.
    let value = serde_json::json!({"decision": 42});
    let decision = forge_daemon::sdk_callbacks::decode_permission_response(&value);
    assert!(!decision.is_allow());
}

#[tokio::test]
async fn rev_value_empty_object_resolves_to_typed_deny() {
    // {} — no decision key at all. Wire shape default is "deny".
    let value = serde_json::json!({});
    let decision = forge_daemon::sdk_callbacks::decode_permission_response(&value);
    assert!(!decision.is_allow());
}

#[tokio::test]
async fn rev_value_missing_decision_key_resolves_to_typed_deny() {
    // {"reason": "..."} — only `reason`, no `decision` key. Round 4 —
    // fix M4 made the missing-decision branch warn-and-return-deny
    // with a fixed reason ("missing decision field") rather than
    // silently falling through to "deny" with the user-supplied reason.
    // The fix prioritises operator visibility over reason-passthrough:
    // a malformed payload should look distinct from a deliberate deny.
    let value = serde_json::json!({"reason": "no decision present"});
    let decision = forge_daemon::sdk_callbacks::decode_permission_response(&value);
    assert!(!decision.is_allow());
    assert_eq!(decision.reason(), Some("missing decision field"));
}

// ============================================================================
// Permission-bridge sentinel coverage (round 2 — fix C5)
// ============================================================================

#[test]
fn perm_bridge_jsonrpc_error_sentinel_denies_with_code_and_message() {
    let v = serde_json::json!({"_jsonrpc_error": {"code": -32601, "message": "method not found"}});
    let d = forge_daemon::sdk_callbacks::decode_permission_response(&v);
    assert!(!d.is_allow());
    let reason = d.reason().unwrap_or("");
    assert!(reason.contains("-32601"), "reason: {reason}");
    assert!(reason.contains("method not found"), "reason: {reason}");
}

#[test]
fn perm_bridge_client_disconnected_sentinel_denies() {
    let v = serde_json::json!({"_client_disconnected": true});
    let d = forge_daemon::sdk_callbacks::decode_permission_response(&v);
    assert!(!d.is_allow());
    assert_eq!(
        d.reason(),
        Some("answering client disconnected before responding")
    );
}

#[test]
fn perm_bridge_session_closed_sentinel_denies() {
    let v = serde_json::json!({"_session_closed": true});
    let d = forge_daemon::sdk_callbacks::decode_permission_response(&v);
    assert!(!d.is_allow());
    assert_eq!(d.reason(), Some("session closed before prompt answered"));
}

#[test]
fn perm_bridge_unknown_decision_denies() {
    let v = serde_json::json!({"decision": "shrug"});
    let d = forge_daemon::sdk_callbacks::decode_permission_response(&v);
    assert!(!d.is_allow());
    let reason = d.reason().unwrap_or("");
    assert!(reason.contains("unknown decision"), "reason: {reason}");
    assert!(reason.contains("shrug"), "reason: {reason}");
}

// ============================================================================
// Hook-bridge sentinel coverage (round 2 — fix C1)
//
// Spec: pre_tool_use + permission_request are SECURITY-CRITICAL — sentinel
// failure modes must DENY. All other hook kinds (post_tool_use,
// notification, stop, etc.) are OBSERVATIONAL — sentinels passthrough so a
// hook outage doesn't deadlock the agent.
//
// `HookDecision::is_allow` returns true for both Allow and Passthrough;
// Deny is the only !is_allow shape. We tell Allow / Passthrough apart by
// `reason()` (only Deny has one) + `updated_input()` (only Allow with
// substitution has one).
// ============================================================================

fn assert_is_deny(d: &forge_sdk::HookDecision, ctx: &str) {
    assert!(!d.is_allow(), "{ctx}: expected deny, got allow/passthrough");
    assert!(d.reason().is_some(), "{ctx}: deny without reason");
}

fn assert_is_passthrough(d: &forge_sdk::HookDecision, ctx: &str) {
    assert!(d.is_allow(), "{ctx}: expected passthrough, got deny");
    assert!(
        d.reason().is_none(),
        "{ctx}: passthrough/allow should have no reason"
    );
    assert!(
        d.updated_input().is_none(),
        "{ctx}: passthrough should not carry updated_input"
    );
}

#[test]
fn hook_bridge_pre_tool_use_jsonrpc_error_denies() {
    let v = serde_json::json!({"_jsonrpc_error": {"code": -32601, "message": "denied"}});
    let d = forge_daemon::sdk_callbacks::decode_hook_response("pre_tool_use", &v);
    assert_is_deny(&d, "pre_tool_use jsonrpc_error");
}

#[test]
fn hook_bridge_pre_tool_use_client_disconnected_denies() {
    let v = serde_json::json!({"_client_disconnected": true});
    let d = forge_daemon::sdk_callbacks::decode_hook_response("pre_tool_use", &v);
    assert_is_deny(&d, "pre_tool_use client_disconnected");
}

#[test]
fn hook_bridge_pre_tool_use_session_closed_denies() {
    let v = serde_json::json!({"_session_closed": true});
    let d = forge_daemon::sdk_callbacks::decode_hook_response("pre_tool_use", &v);
    assert_is_deny(&d, "pre_tool_use session_closed");
}

#[test]
fn hook_bridge_pre_tool_use_encode_error_denies() {
    let v = serde_json::json!({"_encode_error": {"message": "non-serializable input"}});
    let d = forge_daemon::sdk_callbacks::decode_hook_response("pre_tool_use", &v);
    assert_is_deny(&d, "pre_tool_use encode_error");
}

#[test]
fn hook_bridge_pre_tool_use_unknown_decision_denies() {
    // Round 3 — fix I1. Unknown decision on a security-critical hook
    // must DENY, not passthrough. Previously this fell through to
    // `HookDecision::passthrough` regardless of `kind`, which silently
    // approved the tool call when the client returned a typo'd or
    // unknown decision string. The fix: route unknown decisions
    // through `fail_closed_decision(kind, ...)` so `pre_tool_use` and
    // `permission_request` deny while observational kinds passthrough.
    let v = serde_json::json!({"decision": "shrug"});
    let d = forge_daemon::sdk_callbacks::decode_hook_response("pre_tool_use", &v);
    assert_is_deny(&d, "pre_tool_use unknown decision");
}

#[test]
fn hook_bridge_post_tool_use_unknown_decision_passthroughs() {
    // Round 3 — fix I1 sibling. Observational kinds (post_tool_use,
    // stop, notification, …) keep passthrough on unknown decisions
    // so a flapping client doesn't break the agent. Locks the
    // kind-aware fail-closed contract from both directions.
    let v = serde_json::json!({"decision": "shrug"});
    let d = forge_daemon::sdk_callbacks::decode_hook_response("post_tool_use", &v);
    assert_is_passthrough(&d, "post_tool_use unknown decision");
}

#[test]
fn hook_bridge_pre_tool_use_missing_decision_field_denies() {
    // Round 3 — fix I1. Missing `decision` field on a security-critical
    // kind must DENY. Previously the missing-field default was
    // passthrough (via `unwrap_or("passthrough")`), which bypassed the
    // fail-closed contract entirely.
    let v = serde_json::json!({});
    let d = forge_daemon::sdk_callbacks::decode_hook_response("pre_tool_use", &v);
    assert_is_deny(&d, "pre_tool_use missing decision");
}

#[test]
fn hook_bridge_post_tool_use_missing_decision_field_passthroughs() {
    // Round 3 — fix I1 sibling. Observational kinds passthrough on
    // missing decision; the fail-closed-decision lookup honours kind.
    let v = serde_json::json!({});
    let d = forge_daemon::sdk_callbacks::decode_hook_response("post_tool_use", &v);
    assert_is_passthrough(&d, "post_tool_use missing decision");
}

#[test]
fn hook_bridge_permission_request_unknown_decision_denies() {
    // Round 3 — fix I1. permission_request is also security-critical;
    // unknown decision must deny.
    let v = serde_json::json!({"decision": "maybe"});
    let d = forge_daemon::sdk_callbacks::decode_hook_response("permission_request", &v);
    assert_is_deny(&d, "permission_request unknown decision");
}

#[test]
fn hook_bridge_permission_request_missing_decision_field_denies() {
    let v = serde_json::json!({});
    let d = forge_daemon::sdk_callbacks::decode_hook_response("permission_request", &v);
    assert_is_deny(&d, "permission_request missing decision");
}

#[test]
fn hook_bridge_permission_request_jsonrpc_error_denies() {
    let v = serde_json::json!({"_jsonrpc_error": {"code": -32601, "message": "denied"}});
    let d = forge_daemon::sdk_callbacks::decode_hook_response("permission_request", &v);
    assert_is_deny(&d, "permission_request jsonrpc_error");
}

#[test]
fn hook_bridge_permission_request_client_disconnected_denies() {
    let v = serde_json::json!({"_client_disconnected": true});
    let d = forge_daemon::sdk_callbacks::decode_hook_response("permission_request", &v);
    assert_is_deny(&d, "permission_request client_disconnected");
}

#[test]
fn hook_bridge_permission_request_session_closed_denies() {
    let v = serde_json::json!({"_session_closed": true});
    let d = forge_daemon::sdk_callbacks::decode_hook_response("permission_request", &v);
    assert_is_deny(&d, "permission_request session_closed");
}

#[test]
fn hook_bridge_post_tool_use_jsonrpc_error_passthroughs() {
    let v = serde_json::json!({"_jsonrpc_error": {"code": -32601, "message": "x"}});
    let d = forge_daemon::sdk_callbacks::decode_hook_response("post_tool_use", &v);
    assert_is_passthrough(&d, "post_tool_use jsonrpc_error");
}

#[test]
fn hook_bridge_post_tool_use_client_disconnected_passthroughs() {
    let v = serde_json::json!({"_client_disconnected": true});
    let d = forge_daemon::sdk_callbacks::decode_hook_response("post_tool_use", &v);
    assert_is_passthrough(&d, "post_tool_use client_disconnected");
}

#[test]
fn hook_bridge_post_tool_use_session_closed_passthroughs() {
    let v = serde_json::json!({"_session_closed": true});
    let d = forge_daemon::sdk_callbacks::decode_hook_response("post_tool_use", &v);
    assert_is_passthrough(&d, "post_tool_use session_closed");
}

#[test]
fn hook_bridge_notification_session_closed_passthroughs() {
    // Sanity: another observational kind should also passthrough.
    let v = serde_json::json!({"_session_closed": true});
    let d = forge_daemon::sdk_callbacks::decode_hook_response("notification", &v);
    assert_is_passthrough(&d, "notification session_closed");
}

#[test]
fn hook_bridge_post_tool_use_transport_error_passthroughs() {
    let v = serde_json::json!({"_transport_error": {"message": "timed out"}});
    let d = forge_daemon::sdk_callbacks::decode_hook_response("post_tool_use", &v);
    assert_is_passthrough(&d, "post_tool_use transport_error");
}

#[test]
fn hook_bridge_pre_tool_use_transport_error_denies() {
    let v = serde_json::json!({"_transport_error": {"message": "timed out"}});
    let d = forge_daemon::sdk_callbacks::decode_hook_response("pre_tool_use", &v);
    assert_is_deny(&d, "pre_tool_use transport_error");
}

// ============================================================================
// Affirmative-with-payload decode tests (round 3 — fix I5).
//
// Lock the contract that allow + updated_input on `permission.request`,
// and `replace_input` on hook bridges, both carry their payload through
// to the typed decision rather than getting silently flattened.
// ============================================================================

#[test]
fn perm_bridge_allow_with_updated_input_carries_through() {
    let v = serde_json::json!({
        "decision": "allow",
        "updated_input": {"command": "ls -A"},
    });
    let d = forge_daemon::sdk_callbacks::decode_permission_response(&v);
    assert!(d.is_allow(), "expected allow, got deny");
    let updated = d
        .updated_input()
        .expect("allow with updated_input must surface the value");
    assert_eq!(updated["command"], serde_json::json!("ls -A"));
}

#[test]
fn perm_bridge_plain_allow_has_no_updated_input() {
    // Sanity guard so the test above is meaningful — plain allow
    // should NOT surface `updated_input`.
    let v = serde_json::json!({"decision": "allow"});
    let d = forge_daemon::sdk_callbacks::decode_permission_response(&v);
    assert!(d.is_allow());
    assert!(d.updated_input().is_none());
}

#[test]
fn hook_bridge_replace_input_carries_through() {
    let v = serde_json::json!({
        "decision": "replace_input",
        "updated_input": {"file_path": "/safe/path"},
    });
    let d = forge_daemon::sdk_callbacks::decode_hook_response("pre_tool_use", &v);
    // replace_input is encoded as Allow with updated_input. The
    // callback view: `is_allow() == true`, `updated_input()` carries
    // the substitution.
    assert!(d.is_allow(), "replace_input must read as allow");
    let updated = d
        .updated_input()
        .expect("replace_input must carry updated_input");
    assert_eq!(updated["file_path"], serde_json::json!("/safe/path"));
}

#[test]
fn hook_bridge_decodes_all_sync_output_fields() {
    // Round 3 — fix I6. Lock that every Python SyncHookJSONOutput
    // control field round-trips through `decode_hook_response`.
    let v = serde_json::json!({
        "decision": "passthrough",
        "continue": false,
        "suppressOutput": true,
        "stopReason": "policy violation",
        "systemMessage": "Unsafe tool",
    });
    let d = forge_daemon::sdk_callbacks::decode_hook_response("post_tool_use", &v);
    assert_eq!(d.continue_execution(), Some(false));
    assert_eq!(d.suppress_output(), Some(true));
    assert_eq!(d.stop_reason(), Some("policy violation"));
    assert_eq!(d.system_message(), Some("Unsafe tool"));
}

// ============================================================================
// drain_prompts_on_session_exit (round 2 — fix C2)
//
// Spec: when the session actor exits, every parked prompt AND every
// in-flight outstanding_reverse entry for that session must be
// drained. Each gets a synthetic `_session_closed: true` answer so
// the SDK callback unblocks; subscribers see one `prompts.expired`
// notification per drained entry.
// ============================================================================

#[allow(
    clippy::similar_names,
    reason = "test fixtures use a/b/x/y suffixes for parallel oneshot pairs; renaming to dissimilar names hurts readability"
)]
#[allow(
    clippy::too_many_lines,
    reason = "round 3 — fix I2 added prompt_id assertions inline; splitting into helpers obscures the fixture-by-fixture flow"
)]
#[tokio::test]
async fn drain_prompts_on_session_exit_drains_parked_and_in_flight() {
    use forge_daemon::registry::OutstandingEntry;

    let state = Arc::new(forge_daemon::registry::DaemonState::new());
    let sid = forge_daemon::session_state::SessionId("sess_drain_round2".into());
    let (handle, _rx) = state.register_session(sid.clone());

    // Subscribe a fake connection so we can observe the
    // `prompts.expired` broadcasts.
    let (sub_tx, mut sub_rx) =
        tokio::sync::mpsc::channel(forge_daemon::connection::OUTBOUND_CHANNEL_CAPACITY);
    let sub_conn = forge_daemon::connection::Connection::new(
        forge_daemon::connection::ConnectionId("conn_drain_obs".into()),
        sub_tx,
    );
    state.register_connection(sub_conn.clone());
    handle.subscribers.lock().push(sub_conn.id.clone());

    // Two parked prompts directly via handle.prompts.enqueue.
    let (q_tx_a, q_rx_a) = tokio::sync::oneshot::channel();
    handle
        .prompts
        .enqueue(forge_daemon::prompt_queue::PendingPrompt {
            prompt_id: "prompt_park_a".into(),
            kind: forge_daemon::prompt_queue::PromptKind::Permission,
            issued_at: SystemTime::now(),
            expires_at: SystemTime::now() + Duration::from_secs(3600),
            params: serde_json::json!({}),
            responder: q_tx_a,
            rev_id: None,
        });
    let (q_tx_b, q_rx_b) = tokio::sync::oneshot::channel();
    handle
        .prompts
        .enqueue(forge_daemon::prompt_queue::PendingPrompt {
            prompt_id: "prompt_park_b".into(),
            kind: forge_daemon::prompt_queue::PromptKind::Hook {
                kind: "post_tool_use".into(),
            },
            issued_at: SystemTime::now(),
            expires_at: SystemTime::now() + Duration::from_secs(3600),
            params: serde_json::json!({}),
            responder: q_tx_b,
            rev_id: None,
        });

    // Two in-flight outstanding-reverse entries.
    let (rev_tx_x, rev_rx_x) = tokio::sync::oneshot::channel();
    let (rev_tx_y, rev_rx_y) = tokio::sync::oneshot::channel();
    state.outstanding_reverse.lock().insert(
        "rev_inflight_x".into(),
        OutstandingEntry {
            session_id: sid.clone(),
            conn_id: None,
            prompt_id: "prompt_inflight_x".into(),
            responder: rev_tx_x,
        },
    );
    state.outstanding_reverse.lock().insert(
        "rev_inflight_y".into(),
        OutstandingEntry {
            session_id: sid.clone(),
            conn_id: None,
            prompt_id: "prompt_inflight_y".into(),
            responder: rev_tx_y,
        },
    );

    // Drain.
    forge_daemon::reverse_rpc::drain_prompts_on_session_exit(&state, &sid);

    // Each oneshot should have received `_session_closed: true`.
    for (label, rx) in [
        ("park_a", q_rx_a),
        ("park_b", q_rx_b),
        ("inflight_x", rev_rx_x),
        ("inflight_y", rev_rx_y),
    ] {
        let v = tokio::time::timeout(Duration::from_millis(100), rx)
            .await
            .unwrap_or_else(|_| panic!("{label}: drain did not resolve oneshot"))
            .unwrap_or_else(|_| panic!("{label}: oneshot dropped without value"));
        assert_eq!(
            v.get("_session_closed"),
            Some(&serde_json::json!(true)),
            "{label}: expected _session_closed sentinel, got {v}"
        );
    }

    // Subscriber should see 4 prompts.expired notifications (2 parked +
    // 2 in-flight). Drain everything and assert the emitted prompt_ids
    // are the user-visible `prompt_<uuid>`s — NOT the daemon-internal
    // `rev_<uuid>`s. The drain path's contract (round 3 — fix I2) is
    // that subscribers see prompt-id-keyed expiry so the TUI's matcher
    // (`PromptsExpired::prompt_id` against `PendingPermission::prompt_id`)
    // can dismiss the right modal.
    let mut emitted_prompt_ids: Vec<String> = Vec::new();
    while let Ok(frame) = sub_rx.try_recv() {
        if let forge_daemon::connection::Outbound::Notification(n) = frame {
            if n.method == "prompts.expired" {
                let pid = n
                    .params
                    .as_ref()
                    .and_then(|p| p.get("prompt_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                emitted_prompt_ids.push(pid);
            }
        }
    }
    assert_eq!(
        emitted_prompt_ids.len(),
        4,
        "expected 4 prompts.expired notifications, got {emitted_prompt_ids:?}"
    );
    // Sort so order doesn't matter; assertion is on set membership.
    emitted_prompt_ids.sort();
    let mut expected = vec![
        "prompt_park_a".to_string(),
        "prompt_park_b".to_string(),
        "prompt_inflight_x".to_string(),
        "prompt_inflight_y".to_string(),
    ];
    expected.sort();
    assert_eq!(
        emitted_prompt_ids, expected,
        "drain must emit user-visible prompt_<uuid>s, never rev_<uuid>s"
    );
    // Defensive: confirm none of the emitted ids accidentally leaked
    // the rev_ prefix.
    for pid in &emitted_prompt_ids {
        assert!(
            !pid.starts_with("rev_"),
            "drain emitted rev_id where prompt_id was expected: {pid}"
        );
    }

    // Both maps fully drained.
    assert_eq!(handle.prompts.snapshot().len(), 0);
    assert_eq!(state.outstanding_reverse.lock().len(), 0);
}

// ============================================================================
// Fix #4: prompts.expired on disconnect
// ============================================================================

/// When the answering primary disconnects mid-prompt, the parked
/// reverse-RPC must unblock within ~50ms (synthetic
/// `_client_disconnected` answer), NOT the full 1h timeout.
#[tokio::test]
async fn outstanding_reverse_unblocks_when_answering_conn_disconnects() {
    use forge_daemon::registry::OutstandingEntry;
    let state = Arc::new(forge_daemon::registry::DaemonState::new());
    let sid = forge_daemon::session_state::SessionId("sess_disc".into());
    let _kept = state.register_session(sid.clone());

    let (out_tx, _out_rx) =
        tokio::sync::mpsc::channel(forge_daemon::connection::OUTBOUND_CHANNEL_CAPACITY);
    let conn = forge_daemon::connection::Connection::new(
        forge_daemon::connection::ConnectionId("conn_disc".into()),
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
            prompt_id: "prompt_disc".into(),
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
    let state = forge_daemon::registry::DaemonState::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(forge_daemon::server::run(listener, state.clone()));
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
    // Deterministic poll instead of a timing-dependent `sleep`.
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

    // Manually register a session and mark this connection as primary.
    let sid = forge_daemon::session_state::SessionId("sess_e2e".into());
    let (handle, _rx) = state.register_session(sid.clone());
    *handle.primary.lock() = Some(conn_id.clone());

    // Fire issue_to_primary in the background; the daemon will forward
    // the request over the WS to our test client.
    let state_arc = Arc::new(state.clone());
    let sid_for = sid.clone();
    let issue = tokio::spawn(async move {
        forge_daemon::reverse_rpc::issue_to_primary(
            &state_arc,
            &sid_for,
            "permission.request",
            serde_json::json!({"hello": "world"}),
            forge_daemon::prompt_queue::PromptKind::Permission,
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

/// End-to-end (round 2 — fix C3): WS frame
/// `{id: "rev_…", error: {…}}` flows `read_loop` → `resolve_error` →
/// `outstanding_reverse` → bridge → typed deny via the `_jsonrpc_error`
/// sentinel. The piece-wise tests above lock the `resolve_error` /
/// `decode_permission_response` paths separately; this one wires them
/// together through the actual server.
#[tokio::test]
async fn ws_response_with_rev_id_error_resolves_with_typed_jsonrpc_error_sentinel() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message as WsMsg;

    let state = forge_daemon::registry::DaemonState::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(forge_daemon::server::run(listener, state.clone()));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let url = format!("ws://{addr}/");
    let (mut ws, _) = connect_async(&url).await.unwrap();

    // Drain client.identify.
    let WsMsg::Text(t) = ws.next().await.unwrap().unwrap() else {
        panic!("expected text frame")
    };
    assert!(t.contains("client.identify"));

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

    let sid = forge_daemon::session_state::SessionId("sess_e2e_err".into());
    let (handle, _rx) = state.register_session(sid.clone());
    *handle.primary.lock() = Some(conn_id.clone());

    let state_arc = Arc::new(state.clone());
    let sid_for = sid.clone();
    let issue = tokio::spawn(async move {
        forge_daemon::reverse_rpc::issue_to_primary(
            &state_arc,
            &sid_for,
            "permission.request",
            serde_json::json!({"hello": "world"}),
            forge_daemon::prompt_queue::PromptKind::Permission,
            Duration::from_secs(5),
        )
        .await
    });

    // Receive the reverse-RPC frame the daemon issued.
    let WsMsg::Text(t) = ws.next().await.unwrap().unwrap() else {
        panic!("expected text frame")
    };
    let v: serde_json::Value = serde_json::from_str(&t).unwrap();
    let rev_id = v["id"].as_str().unwrap().to_string();
    assert!(rev_id.starts_with("rev_"));

    // Send back a JSON-RPC error response.
    let resp = serde_json::json!({
        "jsonrpc": "2.0",
        "id": rev_id,
        "error": {"code": -32601, "message": "denied"},
    });
    ws.send(WsMsg::Text(resp.to_string())).await.unwrap();

    // The issuing future should resolve with the typed `_jsonrpc_error`
    // sentinel (raw value, not yet decoded into PermissionDecision —
    // decode_permission_response would do that step).
    let value = issue.await.unwrap().unwrap();
    let err = value
        .get("_jsonrpc_error")
        .expect("missing _jsonrpc_error sentinel");
    assert_eq!(err["code"], serde_json::json!(-32601));
    assert_eq!(err["message"], serde_json::json!("denied"));
}

#[tokio::test]
async fn ws_prompts_respond_resolves_queued_prompt_end_to_end() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message as WsMsg;

    let state = forge_daemon::registry::DaemonState::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(forge_daemon::server::run(listener, state.clone()));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let url = format!("ws://{addr}/");
    let (mut ws, _) = connect_async(&url).await.unwrap();

    // Drain client.identify
    let _ = ws.next().await;

    // Register a session with no primary so the prompt parks in the queue.
    let sid = forge_daemon::session_state::SessionId("sess_park_e2e".into());
    let (handle, _rx) = state.register_session(sid.clone());

    // Park a prompt manually.
    let (tx, rx) = tokio::sync::oneshot::channel();
    handle
        .prompts
        .enqueue(forge_daemon::prompt_queue::PendingPrompt {
            prompt_id: "prompt_e2e".into(),
            kind: forge_daemon::prompt_queue::PromptKind::Permission,
            issued_at: SystemTime::now(),
            expires_at: SystemTime::now() + Duration::from_secs(3600),
            params: serde_json::json!({}),
            responder: tx,
            rev_id: None,
        });

    // Send `prompts.respond`.
    let req = forge_daemon::jsonrpc::Request::new(
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

// ============================================================================
// C1 (round 3) — prompts.respond rev_id-Some hot path.
//
// The production path through `methods::prompts::respond` has two
// branches: `rev_id: Some(...)` (the path real reverse-RPC traffic
// takes — calls `reverse_rpc::resolve` directly so the SDK-side
// handler unblocks synchronously) and `rev_id: None` (a fallback for
// legacy / direct-test enqueues).
//
// Every prior test exercises the rev_id-None branch. This locks the
// rev_id-Some branch end-to-end:
//
//   1. `issue_to_primary` with no primary parks — populates BOTH
//      `handle.prompts` (with rev_id) AND `outstanding_reverse`.
//   2. `prompts::respond(state, sid, prompt_id, ...)` resolves via
//      the rev_id; the awaiting `issue_to_primary` future unblocks.
//   3. Both maps are drained.
// ============================================================================

#[tokio::test]
async fn prompts_respond_resolves_via_outstanding_reverse_when_prompt_carries_rev_id() {
    let state = Arc::new(forge_daemon::registry::DaemonState::new());
    let sid = forge_daemon::session_state::SessionId("sess_c1".into());
    // Register a session with no primary so issue_to_primary parks.
    let (handle, _rx) = state.register_session(sid.clone());

    // Spawn the issuer in the background — it'll park because there
    // is no primary, populating both `handle.prompts` AND
    // `outstanding_reverse` with a matching rev_id.
    let st_for_issue = state.clone();
    let sid_for_issue = sid.clone();
    let issue = tokio::spawn(async move {
        forge_daemon::reverse_rpc::issue_to_primary(
            &st_for_issue,
            &sid_for_issue,
            "permission.request",
            serde_json::json!({"tool_name": "Bash", "tool_input": {"command": "ls"}}),
            forge_daemon::prompt_queue::PromptKind::Permission,
            // 5s timeout — plenty of headroom for the test path.
            Duration::from_secs(5),
        )
        .await
    });

    // Poll until the prompt is parked (rather than sleeping a fixed
    // duration). `tokio::task::yield_now` keeps us cache-warm and
    // avoids flakes on slow CI.
    let prompt_id = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let snap = handle.prompts.snapshot();
            if let Some(id) = snap.first().cloned() {
                break id;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("prompt did not appear in queue within 2s");
    assert!(
        prompt_id.starts_with("prompt_"),
        "expected prompt_<uuid>, got {prompt_id}"
    );

    // Sanity: the parked prompt carries a rev_id (rev_<uuid>) and the
    // outstanding-reverse map mirrors it.
    let outstanding_count_before = state.outstanding_reverse.lock().len();
    assert_eq!(
        outstanding_count_before, 1,
        "expected one outstanding entry mirroring the parked prompt, got {outstanding_count_before}"
    );

    // Production hot path: prompts::respond with the prompt_id —
    // routes through the `Some(rev_id)` branch which calls
    // `reverse_rpc::resolve` directly, without going through the
    // queue's responder. The awaiting issuer must see the answer.
    forge_daemon::methods::prompts::respond(
        &state,
        &sid,
        &prompt_id,
        serde_json::json!({"decision": "allow"}),
    )
    .expect("prompts::respond returned an error");

    // The issue future resolves with the answer fed via prompts.respond.
    let value = tokio::time::timeout(Duration::from_secs(2), issue)
        .await
        .expect("issue_to_primary did not resolve within 2s")
        .expect("issue task panicked")
        .expect("issue_to_primary returned Err");
    assert_eq!(
        value["decision"],
        serde_json::json!("allow"),
        "expected the response prompts.respond fed in, got {value}"
    );
    // Confirm the response came from the rev_id-Some hot path — NOT
    // the queue's sentinel (which would carry no payload).
    assert!(
        !value.is_null(),
        "rev_id-Some path must deliver the supplied answer, not the queue sentinel"
    );

    // Both maps fully drained — outstanding-reverse via `resolve()`,
    // queue via the `prompts::respond` `take()`.
    assert!(
        state.outstanding_reverse.lock().is_empty(),
        "outstanding_reverse must be empty after prompts.respond resolves the rev_id"
    );
    assert!(
        handle.prompts.snapshot().is_empty(),
        "prompts queue must be empty after prompts.respond consumes the entry"
    );
}
