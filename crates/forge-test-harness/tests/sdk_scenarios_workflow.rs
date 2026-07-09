//! Live-capture scenario: multi-agent orchestration via the `Workflow` tool.
//!
//! Drives the model to author + run a minimal `Workflow` so the CLI emits
//! the `Workflow` tool_use plus the workflow / background-task lifecycle
//! frames (`background_tasks_changed`, `task_progress` carrying
//! `workflow_progress`, `task_notification`). Exercises forge-sdk's decode
//! of the workflow orchestration wire surface.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use forge_sdk::{OptionsBuilder, PermissionMode};
use forge_test_harness::sdk_wire::run_live_scenario;

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_workflow() {
    let opts = OptionsBuilder::new()
        .max_turns(12)
        .permission_mode(PermissionMode::AcceptEdits)
        .allowed_tools(vec!["Workflow".to_string(), "Task".to_string(), "Bash".to_string()])
        .build();

    run_live_scenario("workflow", opts, |client, events| async move {
        client
            .send_user_message(
                "Run a workflow (use the Workflow tool). Keep it minimal: fan \
                 out exactly two agents in parallel, each with a trivial task - \
                 agent one returns the single word \"ping\", agent two returns \
                 the single word \"pong\". No schema, no extra phases. After the \
                 workflow finishes, report the two words as your final answer.",
            )
            .await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}
