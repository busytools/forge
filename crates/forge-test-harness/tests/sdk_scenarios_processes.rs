//! Live-capture scenarios for the long-running tool lifecycle events
//! the Inspector pane's PROCESSES section depends on.
//!
//! - `backgrounded_bash_lifecycle` - drives the model to invoke Bash
//!   with `run_in_background: true`, lets the spawned process exit,
//!   and captures the `backgroundTaskId` round-trip plus any
//!   `task_notification` the kill / completion path emits.
//! - `monitor_stream` - drives the model to invoke the `Monitor` tool
//!   with a short script that emits a handful of stdout lines, then
//!   captures the task lifecycle frames plus the per-line event
//!   notifications and the ending notification.
//!
//! Both scenarios pin wire surfaces the binary trace says
//! `TaskNotification` covers but the existing `subagent.jsonl`
//! baseline doesn't exercise. Without these baselines, forge has no
//! evidence of what the wire actually looks like for these tool
//! kinds - synthesising baselines from binary strings is explicitly
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
async fn wire_capture_monitor_stream() {
    // Monitor + TaskStop so the model can start the watch and, if it
    // outlives its source, stop it. The script emits 3 lines and exits,
    // which ends the watch with the task frames this scenario records -
    // at 2.1.280 the tool's schema takes command/description/timeout_ms
    // and no `persistent`.
    //
    // The command is a plain `&&` chain on purpose: the CLI's shell
    // analyzer refuses loops and expansions, and a refused command never
    // reaches the Monitor it was meant to start.
    //
    // Monitor is advertised only while the CLI's feature-flag service
    // reports its `tengu_amber_sentinel` gate true; an env carrying
    // forge's `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` gets the false
    // default, and the model then improvises with Bash. Run this capture
    // with that variable unset.
    let opts = OptionsBuilder::new()
        .max_turns(8)
        .permission_mode(PermissionMode::AcceptEdits)
        .allowed_tools(vec!["Monitor".to_string(), "TaskStop".to_string()])
        .build();

    run_live_scenario("monitor_stream", opts, |client, events| async move {
        client
            .send_user_message(
                "Use the Monitor tool to stream events from the shell \
                 command `sleep 1 && echo monitor-line-1 && sleep 0.5 && \
                 echo monitor-line-2 && sleep 0.5 && echo monitor-line-3`. \
                 Description: `forge-monitor-test`. Wait until you've \
                 received all three event notifications and the watch has \
                 ended, then reply with the three lines you saw.",
            )
            .await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}
