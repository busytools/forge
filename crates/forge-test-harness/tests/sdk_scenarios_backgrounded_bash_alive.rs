//! Live-capture scenario: a backgrounded Bash still running when the
//! turn's `result` fires.
//!
//! `backgrounded_bash_lifecycle` polls `BashOutput` until the task exits,
//! so its terminal `task_updated` and the roster-drop both land before
//! `result`. This scenario instead backgrounds a long sleep and ends the
//! turn immediately, so `result` arrives while the task is still listed in
//! `background_tasks` with no terminal `task_updated` yet - the wire
//! ordering the cross-turn liveness fix relies on.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use forge_sdk::{OptionsBuilder, PermissionMode};
use forge_test_harness::sdk_wire::run_live_scenario;

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_backgrounded_bash_alive_at_result() {
    let opts = OptionsBuilder::new()
        .max_turns(4)
        .permission_mode(PermissionMode::AcceptEdits)
        .allowed_tools(vec!["Bash".to_string(), "BashOutput".to_string(), "KillBash".to_string()])
        .build();

    run_live_scenario("backgrounded_bash_alive_at_result", opts, |client, events| async move {
        client
            .send_user_message(
                "Use the Bash tool with `run_in_background: true` to run \
                 `sleep 30 && echo forge-bg-still-running`. The moment the \
                 tool returns a backgroundTaskId, reply with exactly the \
                 word `backgrounded` and end your turn. Do NOT call \
                 BashOutput and do NOT wait for the command to finish - it \
                 must still be running when you reply.",
            )
            .await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}
