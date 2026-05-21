//! Wire-conformance: spawn a worker via `mcp__forge__workers__spawn`,
//! observe the spawn round-trip + the new worker's bootstrap + the
//! tag-write JSONL append.
//!
//! Mirror of `sdk_scenarios_in_process_mcp.rs`. Replay mode runs
//! against `baselines/sdk/<PINNED_CLI_VERSION>/worker_spawn.jsonl`.
//! Baseline not yet captured - re-run with `FORGE_WIRE_CAPTURE=1` to
//! generate it before un-ignoring this test.
//!
//! Seed prompt instructs the lead to call `mcp__forge__workers__spawn`
//! with a label and charter, list workers, then close the spawned
//! worker. Captured trace assertions:
//! - contains a `mcp__forge__workers__spawn` tool call
//! - the worker's session_id appears in the parent's chat
//! - a follow-up `mcp__forge__workers__list` reflects the new worker

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live capture pending: run with FORGE_WIRE_CAPTURE=1 to record baseline"]
async fn worker_spawn_scenario() {
    // Baseline not yet captured. Copy the harness invocation from
    // `sdk_scenarios_in_process_mcp.rs` and adapt for the workers
    // MCP server with the prompt:
    //   "Spawn a worker labeled \"test-reviewer\" with charter \
    //    \"be terse\". List workers, then close the spawned worker."
    //
    // Until the baseline exists, fail loudly if the test is ever
    // un-ignored without the body being populated.
    panic!("worker_spawn_scenario: baseline not yet captured - populate harness body");
}
