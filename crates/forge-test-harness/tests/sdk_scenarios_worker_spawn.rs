//! Live-capture scenario: lead drives `mcp__forge__agents__spawn` and
//! `mcp__forge__agents__list`.
//!
//! Registers the in-process agents MCP server (backed by
//! `MockWorkerFacade` + `MockWorkspaceFacade`) on a single `claude`
//! subprocess and asks the model to spawn a worker labelled "reviewer"
//! then list agents. The captured trace covers the wire shape we care
//! about:
//!
//! - `mcp_message:initialize` + `tools/list` round trips for the
//!   `forge` MCP server (carries the `agents__*` tool definitions).
//! - `mcp_message:tools/call` for `agents__spawn` (CLI -> SDK).
//! - SDK `control_response` carrying the mock's
//!   `{session_id, tag: "forge:worker:reviewer"}` reply.
//! - `mcp_message:tools/call` for `agents__list` (CLI -> SDK) with
//!   the SDK responding with the pre-seeded agents.
//!
//! No real worker subprocess is spawned. The mock facade returns
//! synthetic IDs so the test stays a single-process wire-conformance
//! check, mirroring how `sdk_scenarios_in_process_mcp.rs` exercises
//! its `greet` tool. Real worker spawn lifecycle is covered by the
//! workspace integration tests; this harness focuses on the
//! stream-json layer.
//!
//! `PermissionMode::BypassPermissions` keeps the permission callback
//! out of the path so the trace stays focused on MCP tool round trips.

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
async fn worker_spawn_scenario() {
    let caller = SessionSlot::from_str_for_test("lead-test-session");
    let project_key = forge_workspace::ProjectKey::new("forge");

    let mock = MockWorkerFacade::new();
    mock.callers.lock().insert(caller.clone(), CallerProject { project_key, is_lead: true });
    // Preloaded spawn reply: the mock returns this synthetic
    // {session_id, tag} as if a real worker had been spawned.
    *mock.spawn_reply.lock() = Some(Ok(WorkerSpawnReply {
        session_id: "worker-session-uuid-stub".into(),
        tag: forge_primitives::worker_tag("reviewer"),
        rate_limited_account: None,
        durability_warning: None,
        session_choice: forge_workspace::protocol::SessionChoice::Fresh,
    }));
    // Pre-seed the worker pool so a follow-up agents__list call
    // returns the spawned worker without needing the spawn-side
    // dispatch path to mutate state (the mock's spawn_worker captures
    // the call but does not update its own `workers` map).
    mock.workers.lock().insert(
        "forge".to_string(),
        vec![forge_primitives::WorkerStatus {
            label: "reviewer".into(),
            charter: "You are a terse reviewer. Reply with one word answers.".into(),
            status: forge_primitives::WorkerLiveness::Running,
            session_id: "worker-session-uuid-stub".into(),
            slot: SessionSlot::worker("TestOrg", "forge", "reviewer"),
            spawned_at: SystemTime::now(),
            spawned_by: SessionSlot::from_str_for_test("lead-test-session"),
            diagnostic: None,
            activity: Some(forge_primitives::SessionLifecycleState::Idle),
        }],
    );
    let facade: Arc<dyn WorkerFacade> = Arc::new(mock);

    // `agents__list` reads the configured projects off the peers facade,
    // so seed the one the caller's workers live in.
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

    run_live_scenario("worker_spawn", opts, |client, events| async move {
        client
            .send_user_message(
                "Call mcp__forge__agents__spawn with label=\"reviewer\" and \
                 charter=\"You are a terse reviewer. Reply with one word answers.\". \
                 Then call mcp__forge__agents__list (no arguments) and report the list. \
                 Reply with a one-line summary of what you spawned and the agents you see.",
            )
            .await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}
