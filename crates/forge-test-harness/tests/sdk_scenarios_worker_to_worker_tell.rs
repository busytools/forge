//! Wire-conformance: spawn two workers, then worker A calls
//! `mcp__forge__workers__tell` to send a message to worker B.
//! Observes spawn x 2, tell, deliver-worker-prompt, worker B reply.
//!
//! Mirror of `sdk_scenarios_in_process_mcp.rs`. Replay mode runs
//! against `baselines/sdk/<PINNED_CLI_VERSION>/worker_to_worker_tell.jsonl`.
//! Baseline not yet captured - re-run with `FORGE_WIRE_CAPTURE=1` to
//! generate it before un-ignoring this test.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live capture pending: run with FORGE_WIRE_CAPTURE=1 to record baseline"]
async fn worker_to_worker_tell_scenario() {
    // Baseline not yet captured. Copy the harness invocation from
    // `sdk_scenarios_in_process_mcp.rs`. Seed prompt: spawn worker A
    // and worker B, then instruct worker A to call
    // `mcp__forge__workers__tell` targeting worker B's session_id with
    // a short message. Assertions: two spawn calls, one tell call,
    // and worker B's reply visible in the captured trace.
    //
    // Until the baseline exists, fail loudly if the test is ever
    // un-ignored without the body being populated.
    panic!(
        "worker_to_worker_tell_scenario: baseline not yet captured - populate harness body"
    );
}
