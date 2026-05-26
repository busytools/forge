//! Live-capture scenario: lead drives `mcp__forge__workers__spawn`
//! followed by `mcp__forge__workers__tell`.
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
//! - `mcp_message:tools/call` for `workers__spawn` (CLI -> SDK).
//! - SDK `control_response` carrying the mock's `{session_id, tag}`.
//! - `mcp_message:tools/call` for `workers__tell` targeting the
//!   spawned worker by label.
//! - SDK `control_response` carrying the mock's
//!   `{correlation_id, status: "delivered"}`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;
use std::time::SystemTime;

use forge_sdk::{OptionsBuilder, PermissionMode};
use forge_test_harness::sdk_wire::run_live_scenario;
use forge_workspace::SessionKey;
use forge_workspace::protocol::WorkerSpawnReply;
use forge_workspace::{
    CallerKeyResolver, CallerProject, MockWorkerFacade, WorkerFacade, build_workers_server,
};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn worker_to_worker_tell_scenario() {
    let caller_key = SessionKey::from_session_id("lead-test-session");
    let project_key = forge_workspace::ProjectKey::new_for_test("forge");

    let mock = MockWorkerFacade::new();
    mock.callers.lock().insert(caller_key.clone(), CallerProject { project_key, is_lead: true });
    *mock.spawn_reply.lock() = Some(Ok(WorkerSpawnReply {
        session_id: "beta-session-uuid-stub".into(),
        tag: forge_primitives::worker_tag("beta"),
    }));
    // Pre-seed the worker pool so workers__tell finds a live target
    // by label. The spawn call captures the request but does not
    // mutate this map on its own.
    mock.workers.lock().insert(
        "forge".to_string(),
        vec![forge_primitives::WorkerStatus {
            label: "beta".into(),
            charter: "You are beta. When told something, acknowledge briefly.".into(),
            status: forge_primitives::WorkerLiveness::Running,
            session_id: "beta-session-uuid-stub".into(),
            spawned_at: SystemTime::now(),
            spawned_by_session_id: "lead-test-session".into(),
        }],
    );
    let facade: Arc<dyn WorkerFacade> = Arc::new(mock);

    let server = build_workers_server(facade, CallerKeyResolver::from_fixed(caller_key));

    let opts = OptionsBuilder::new()
        .max_turns(4)
        .permission_mode(PermissionMode::BypassPermissions)
        .mcp_server("forge", server)
        .build();

    run_live_scenario("worker_to_worker_tell", opts, |client, events| async move {
        client
            .send_user_message(
                "Call mcp__forge__workers__spawn with label=\"beta\" and \
                 charter=\"You are beta. When told something, acknowledge briefly.\". \
                 Then call mcp__forge__workers__tell with label=\"beta\" and \
                 message=\"hello beta, please acknowledge\". Reply with a one-line \
                 summary confirming the tell was delivered.",
            )
            .await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}
