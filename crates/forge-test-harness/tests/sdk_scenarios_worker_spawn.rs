//! Live-capture scenario: lead drives `mcp__forge__workers__spawn` and
//! `mcp__forge__workers__list`.
//!
//! Registers the in-process workers MCP server (backed by
//! `MockWorkerFacade`) on a single `claude` subprocess and asks the
//! model to spawn a worker labelled "reviewer" then list workers. The
//! captured trace covers the wire shape we care about:
//!
//! - `mcp_message:initialize` + `tools/list` round trips for the
//!   `forge` MCP server (carries the `workers__*` tool definitions).
//! - `mcp_message:tools/call` for `workers__spawn` (CLI -> SDK).
//! - SDK `control_response` carrying the mock's
//!   `{session_id, tag: "forge:worker:reviewer"}` reply.
//! - `mcp_message:tools/call` for `workers__list` (CLI -> SDK) with
//!   the SDK responding with the pre-seeded worker pool.
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
use forge_workspace::SessionKey;
use forge_workspace::protocol::WorkerSpawnReply;
use forge_workspace::{
    CallerKeyResolver, CallerProject, MockWorkerFacade, WorkerFacade, build_workers_server,
};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn worker_spawn_scenario() {
    let caller_key = SessionKey::from_session_id("lead-test-session");
    let project_key = forge_workspace::ProjectKey::new_for_test("forge");

    let mock = MockWorkerFacade::new();
    mock.callers.lock().insert(caller_key.clone(), CallerProject { project_key, is_lead: true });
    // Preloaded spawn reply: the mock returns this synthetic
    // {session_id, tag} as if a real worker had been spawned.
    *mock.spawn_reply.lock() = Some(Ok(WorkerSpawnReply {
        session_id: "worker-session-uuid-stub".into(),
        tag: forge_primitives::worker_tag("reviewer"),
    }));
    // Pre-seed the worker pool so a follow-up workers__list call
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

    run_live_scenario("worker_spawn", opts, |client, events| async move {
        client
            .send_user_message(
                "Call mcp__forge__workers__spawn with label=\"reviewer\" and \
                 charter=\"You are a terse reviewer. Reply with one word answers.\". \
                 Then call mcp__forge__workers__list (no arguments) and report the list. \
                 Reply with a one-line summary of what you spawned and the workers you see.",
            )
            .await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}
