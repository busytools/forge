//! Live-capture scenarios: remaining outbound `control_request` subtypes.
//!
//! One scenario per subtype, each producing an outbound
//! `control_request` + inbound `control_response` round trip the replay
//! harness can exercise.



#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use forge_sdk::{OptionsBuilder, PermissionMode};
use forge_test_harness::sdk_wire::run_live_scenario;

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_set_model() {
    let opts =
        OptionsBuilder::new().max_turns(1).permission_mode(PermissionMode::AcceptEdits).build();

    run_live_scenario("set_model", opts, |client, events| async move {
        client.set_model(Some("claude-sonnet-4-6")).await?;
        client.send_user_message("Reply with only the word OK.").await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_mcp_reconnect() {
    // Need at least one MCP server in the init set — use whatever the
    // user's profile reports from `mcp_status`. If none, the scenario
    // skips inside drive.
    let opts =
        OptionsBuilder::new().max_turns(1).permission_mode(PermissionMode::AcceptEdits).build();

    run_live_scenario("mcp_reconnect", opts, |client, events| async move {
        let status = client.mcp_status().await?;
        let Some(server) = status.mcp_servers.first() else {
            eprintln!("mcp_reconnect: no MCP servers in profile; still capturing init + skip");
            client.send_user_message("Reply with only OK.").await?;
            return Ok((client, events));
        };
        let name = server.name.clone();
        if let Err(e) = client.mcp_reconnect(&name).await {
            eprintln!("mcp_reconnect({name}): {e} — continuing");
        }
        client.send_user_message("Reply with only OK.").await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_mcp_toggle() {
    let opts =
        OptionsBuilder::new().max_turns(1).permission_mode(PermissionMode::AcceptEdits).build();

    run_live_scenario("mcp_toggle", opts, |client, events| async move {
        let status = client.mcp_status().await?;
        let Some(server) = status.mcp_servers.first() else {
            eprintln!("mcp_toggle: no MCP servers in profile; still capturing init + skip");
            client.send_user_message("Reply with only OK.").await?;
            return Ok((client, events));
        };
        let name = server.name.clone();
        // Toggle off then back on — exercises both payloads.
        if let Err(e) = client.mcp_toggle(&name, false).await {
            eprintln!("mcp_toggle({name}, false): {e}");
        }
        if let Err(e) = client.mcp_toggle(&name, true).await {
            eprintln!("mcp_toggle({name}, true): {e}");
        }
        client.send_user_message("Reply with only OK.").await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_rewind_files() {
    // `rewind_files` takes a user_message_id which the CLI only emits
    // when the `replay-user-messages` CLI flag is on (docs:
    // `UserMessage.uuid` is `None` unless the CLI is configured via
    // `extra_args={"replay-user-messages": None}`). Combined with
    // `enable_file_checkpointing(true)` to actually make checkpointing
    // available on the backend.
    let opts = OptionsBuilder::new()
        .max_turns(1)
        .permission_mode(PermissionMode::AcceptEdits)
        .enable_file_checkpointing(true)
        .extra_arg("replay-user-messages", None)
        .build();

    run_live_scenario("rewind_files", opts, |client, mut events| async move {
        client.send_user_message("Reply with only the word OK.").await?;

        // Drain until Result, capturing the first user_message uuid.
        let mut rewind_id: Option<String> = None;
        loop {
            match events.recv().await {
                Some(Ok(forge_sdk::Message::User { uuid: Some(id), .. }))
                    if rewind_id.is_none() =>
                {
                    rewind_id = Some(id);
                }
                Some(Ok(forge_sdk::Message::Result { .. })) | None => break,
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e),
            }
        }
        if let Some(id) = rewind_id {
            if let Err(e) = client.rewind_files(&id).await {
                eprintln!("rewind_files({id}): {e}");
            }
        } else {
            eprintln!("rewind_files: no user_message_id captured; outbound control skipped");
        }
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_stop_task() {
    let opts = OptionsBuilder::new()
        .max_turns(6)
        .permission_mode(PermissionMode::AcceptEdits)
        .allowed_tools(vec!["Task".to_string(), "Bash".to_string()])
        .build();

    run_live_scenario("stop_task", opts, |client, mut events| async move {
        client
            .send_user_message(
                "Use the Task tool with subagent_type=\"general-purpose\" \
                 to run a Bash loop that counts slowly.",
            )
            .await?;

        // Drain until we see a task_started, grab its id, stop it.
        let mut task_id: Option<String> = None;
        while task_id.is_none() {
            match events.recv().await {
                Some(Ok(forge_sdk::Message::TaskStarted { task_id: id, .. })) => {
                    task_id = Some(id);
                    break;
                }
                Some(Ok(forge_sdk::Message::Result { .. })) | None => break,
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e),
            }
        }
        if let Some(id) = task_id {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            if let Err(e) = client.stop_task(&id).await {
                eprintln!("stop_task({id}): {e}");
            }
        } else {
            eprintln!("stop_task: no task_started seen; outbound control skipped");
        }
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_interrupt() {
    let opts =
        OptionsBuilder::new().max_turns(1).permission_mode(PermissionMode::AcceptEdits).build();

    run_live_scenario("interrupt", opts, |client, events| async move {
        client
            .send_user_message("Count from 1 to 500 slowly, one number per line. Take your time.")
            .await?;

        // Wait a moment to let the turn start, then interrupt. The
        // 800ms delay balances reliably catching mid-turn with not
        // needing tokens for a full count.
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        if let Err(e) = client.interrupt().await {
            eprintln!("interrupt: {e}");
        }
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}
