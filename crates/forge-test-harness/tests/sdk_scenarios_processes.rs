//! Live-capture scenarios for the long-running tool lifecycle events
//! the Inspector pane's PROCESSES section depends on.
//!
//! - `backgrounded_bash_lifecycle` — drives the model to invoke Bash
//!   with `run_in_background: true`, lets the spawned process exit,
//!   and captures the `backgroundTaskId` round-trip plus any
//!   `task_notification` the kill / completion path emits.
//! - `monitor_persistent_stream` — drives the model to invoke the
//!   `Monitor` tool with a short script that emits a handful of
//!   stdout lines, then captures every per-line notification + the
//!   final stop notification.
//!
//! Both scenarios pin wire surfaces the binary trace says
//! `TaskNotification` covers but the existing `subagent.jsonl`
//! baseline doesn't exercise. Without these baselines, forge has no
//! evidence of what the wire actually looks like for these tool
//! kinds — synthesising baselines from binary strings is explicitly
//! banned (see TIL 2026-05-13).

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use forge_sdk::{OptionsBuilder, PermissionMode};
use forge_test_harness::sdk_wire::run_live_scenario;

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_backgrounded_bash_lifecycle() {
    // AcceptEdits + Bash + BashOutput so the model can both spawn
    // the background task and poll its output without a permission
    // round-trip. Limit turns so a misbehaving model can't hang the
    // capture.
    let opts = OptionsBuilder::new()
        .max_turns(6)
        .permission_mode(PermissionMode::AcceptEdits)
        .allowed_tools(vec!["Bash".to_string(), "BashOutput".to_string(), "KillBash".to_string()])
        .build();

    run_live_scenario("backgrounded_bash_lifecycle", opts, |client, events| async move {
        client
            .send_user_message(
                "Use the Bash tool with `run_in_background: true` to run \
                 `sleep 1 && echo forge-bash-bg-ok`. After spawning, use \
                 BashOutput on the returned backgroundTaskId until the \
                 task has exited, then reply with exactly the line the \
                 command printed.",
            )
            .await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_monitor_persistent_stream() {
    // Monitor + TaskStop so the model can both start the stream and
    // terminate it deterministically. Persistent: true to capture
    // the "lives until TaskStop" branch; the test script emits 3
    // lines and exits, which then triggers the stop notification.
    let opts = OptionsBuilder::new()
        .max_turns(8)
        .permission_mode(PermissionMode::AcceptEdits)
        .allowed_tools(vec!["Monitor".to_string(), "TaskStop".to_string()])
        .build();

    run_live_scenario("monitor_persistent_stream", opts, |client, events| async move {
        client
            .send_user_message(
                "Use the Monitor tool with `persistent: true` to stream \
                 events from `for i in 1 2 3; do echo monitor-line-$i; \
                 sleep 0.3; done`. Description: `forge-monitor-test`. \
                 Wait until you've received all three event notifications, \
                 then use TaskStop with the monitor's taskId to terminate \
                 the watch. Reply with the three lines you saw.",
            )
            .await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}
