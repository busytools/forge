//! Live-capture scenario: lead drives `mcp__forge__agents__spawn`
//! followed by `mcp__forge__agents__tell`.
//!
//! Simplified to lead-to-worker tell because the harness spawns a
//! single `claude` subprocess and cannot orchestrate a second worker
//! subprocess from within the same trace. Worker-to-worker tell
//! exercises the same SDK-side `deliver_worker_prompt` path in
//! production (the second worker just lives in a different process);
//! the wire shape on the lead's stream-json is identical. Full
//! worker-to-worker delivery is covered by `forge-workspace`
//! integration tests.
//!
//! Captured trace shape:
//! - `mcp_message:initialize` + `tools/list` round trip for the
//!   `forge` MCP server.
//! - `mcp_message:tools/call` for `agents__spawn` (CLI -> SDK).
//! - SDK `control_response` carrying the mock's `{session_id, tag}`.
//! - `mcp_message:tools/call` for `agents__tell` targeting the
//!   spawned worker by label.
//! - SDK `control_response` carrying the mock's
//!   `{correlation_id, target_status: "delivered"}`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;
use std::time::SystemTime;

use forge_sdk::{OptionsBuilder, PermissionMode};
use forge_test_harness::sdk_wire::run_live_scenario;
use forge_workspace::SessionSlot;
use forge_workspace::protocol::WorkerSpawnReply;
use forge_workspace::{
    CallerProject, MockWorkerFacade, MockWorkspaceFacade, WorkerFacade, build_agents_server,
};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn worker_to_worker_tell_scenario() {
    let caller = SessionSlot::from_str_for_test("lead-test-session");
    let project_key = forge_workspace::ProjectKey::new("forge");

    let mock = MockWorkerFacade::new();
    mock.callers.lock().insert(caller.clone(), CallerProject { project_key, is_lead: true });
    *mock.spawn_reply.lock() = Some(Ok(WorkerSpawnReply {
        session_id: "beta-session-uuid-stub".into(),
        tag: forge_primitives::worker_tag("beta"),
        rate_limited_account: None,
        durability_warning: None,
        session_choice: forge_workspace::protocol::SessionChoice::Fresh,
    }));
    // Pre-seed the worker pool so agents__tell finds a live target
    // by label. The spawn call captures the request but does not
    // mutate this map on its own.
    mock.workers.lock().insert(
        "forge".to_string(),
        vec![forge_primitives::WorkerStatus {
            label: "beta".into(),
            charter: "You are beta. When told something, acknowledge briefly.".into(),
            status: forge_primitives::WorkerLiveness::Running,
            session_id: "beta-session-uuid-stub".into(),
            slot: SessionSlot::worker("TestOrg", "forge", "beta"),
            spawned_at: SystemTime::now(),
            spawned_by: SessionSlot::from_str_for_test("lead-test-session"),
            diagnostic: None,
            activity: Some(forge_primitives::SessionLifecycleState::Idle),
        }],
    );
    let facade: Arc<dyn WorkerFacade> = Arc::new(mock);

    // `agents__tell` resolves its target against the configured
    // projects, so seed the one the caller's workers live in.
    let peers = MockWorkspaceFacade::new();
    peers.peers.lock().push(forge_workspace::PeerStatus {
        name: "forge".into(),
        org: "TestOrg".into(),
        path: std::path::PathBuf::from("/tmp/forge"),
        status: forge_workspace::PeerLiveness::Running,
        in_flight_incoming: 0,
        in_flight_outgoing: 0,
        spawned_at: None,
    });
    let server = build_agents_server(Arc::new(peers), facade, caller);

    let opts = OptionsBuilder::new()
        .max_turns(4)
        .permission_mode(PermissionMode::BypassPermissions)
        .mcp_server("forge", server)
        .build();

    run_live_scenario("worker_to_worker_tell", opts, |client, events| async move {
        client
            .send_user_message(
                "Call mcp__forge__agents__spawn with label=\"beta\" and \
                 charter=\"You are beta. When told something, acknowledge briefly.\". \
                 Then call mcp__forge__agents__tell with org=\"TestOrg\", project=\"forge\", \
                 label=\"beta\" and message=\"hello beta, please acknowledge\". Reply with a \
                 one-line summary confirming the tell was delivered.",
            )
            .await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}
