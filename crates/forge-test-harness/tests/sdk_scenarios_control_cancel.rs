//! Live-capture scenario: `control_cancel_request` from the CLI.
//!
//! Captures the rare frame the CLI emits when it gives up waiting on a
//! `hook_callback` reply past its timeout. Produces the only wire shape
//! our decoder had no live coverage for until now.
//!
//! Mechanism:
//! 1. Register a `PreToolUse` hook with `default_timeout_secs(1)` — the
//!    initialize payload will carry `timeout: 1` per hook matcher.
//! 2. Make the hook callback sleep 3 seconds before returning.
//! 3. Send a prompt that invokes Bash.
//! 4. CLI fires `hook_callback` `control_request`, waits 1s, then emits
//!    `control_cancel_request` with the matching `request_id`.
//! 5. Our handler (still running) finally writes a `control_response`,
//!    which the CLI may discard — that's fine, the cancel frame is
//!    the capture target.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::time::Duration;

use forge_sdk::{
    HookContext, HookDecision, HooksBuilder, OptionsBuilder, PermissionMode, PreToolUseInput,
};
use forge_test_harness::sdk_wire::run_live_scenario;

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_control_cancel() {
    let hooks = HooksBuilder::new()
        .default_timeout_secs(1)
        .pre_tool_use("Bash", |_input: PreToolUseInput, _ctx: HookContext| async move {
            // Sleep past the 1s CLI timeout so the CLI emits a
            // control_cancel_request for this hook_callback.
            tokio::time::sleep(Duration::from_secs(3)).await;
            HookDecision::passthrough()
        })
        .build();

    let opts = OptionsBuilder::new()
        .max_turns(3)
        .permission_mode(PermissionMode::AcceptEdits)
        .allowed_tools(vec!["Bash".to_string()])
        .hooks(hooks)
        .build();

    run_live_scenario("control_cancel", opts, |client, events| async move {
        client
            .send_user_message(
                "Run `echo forge-cancel-scenario` with the Bash tool and \
                 then reply with exactly what the command printed.",
            )
            .await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}
