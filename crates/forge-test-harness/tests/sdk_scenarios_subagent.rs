//! Live-capture scenario: subagent via the `Task` tool.
//!
//! Drives the model through a `Task` tool invocation so the CLI emits
//! the `task_started` / `task_progress` / `task_notification` message
//! family. Exercises forge-sdk's decoder for sub-agent lifecycle frames
//! (`TaskStartedMessage` / `TaskProgressMessage` /
//! `TaskNotificationMessage`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forge_sdk::{OptionsBuilder, PermissionMode};
use forge_test_harness::sdk_wire::run_live_scenario;

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_subagent() {
    let opts = OptionsBuilder::new()
        .max_turns(6)
        .permission_mode(PermissionMode::AcceptEdits)
        .allowed_tools(vec!["Task".to_string(), "Bash".to_string()])
        .build();

    run_live_scenario("subagent", opts, |client, events| async move {
        client
            .send_user_message(
                "Use the Task tool with subagent_type=\"general-purpose\" \
                 to dispatch a subagent that runs `echo forge-subagent-ok` \
                 via Bash and reports the output back. When the subagent \
                 replies, return its output as your final answer.",
            )
            .await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}
