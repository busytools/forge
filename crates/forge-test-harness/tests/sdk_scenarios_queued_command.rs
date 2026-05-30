//! Live-capture scenario: mid-turn `Command::Prompt` while a turn is
//! still streaming. Records what claude actually emits on
//! stream-json stdout when the user submits a second prompt before
//! the first has resolved.
//!
//! **Empirical finding from live capture 2026-05-13** (this is the
//! whole point of the scenario):
//!
//! 1. Forge writes the first user prompt to claude's stdin.
//! 2. Claude returns an `assistant` envelope carrying a `tool_use`.
//! 3. Before forge replies with the tool_result, forge writes a
//!    second user prompt to stdin. Claude internally buffers it
//!    (the `gO6` queue in the bundled JS).
//! 4. Claude bundles the buffered prompt with the next outbound
//!    user-message envelope **to the model API**, persisting it to
//!    the session JSONL as `{"type":"attachment","attachment":
//!    {"type":"queued_command",...}}` for resume.
//! 5. **Nothing about the buffered prompt is echoed back on
//!    stream-json stdout to forge.** The wire only shows: forge's
//!    tool_result reply (user envelope) and claude's final
//!    `assistant: text` response - which incorporates BOTH prompts
//!    in its content (single merged turn).
//!
//! Implication for forge-tui: there is NO live wire signal that
//! claude has consumed a buffered prompt. The mid-turn dim → un-dim
//! handshake must rely on the turn boundary (`SessionUpdate::
//! TurnComplete`) as its only reliable trigger. The JSONL attachment
//! row is what lets session-resume reconstruct the queued bubble
//! (handled by `forge_agent::userdata::catalog::scan`).
//!
//! This scenario locks in the *absence* of a wire echo: the captured
//! baseline contains zero `queued_command` content blocks and zero
//! `attachment` envelopes. Any future regression where claude starts
//! emitting one will surface here.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::time::Duration;

use forge_sdk::{OptionsBuilder, PermissionMode};
use forge_test_harness::sdk_wire::run_live_scenario;

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_queued_command() {
    let opts = OptionsBuilder::new()
        .max_turns(3)
        .permission_mode(PermissionMode::AcceptEdits)
        .allowed_tools(vec!["Bash".to_string()])
        .build();

    run_live_scenario("queued_command", opts, |client, events| async move {
        // First user message: ask claude to run a Bash echo so the
        // first turn enters a tool_use → tool_result loop. That
        // gives us a definite in-flight window during which to
        // submit the second prompt.
        client
            .send_user_message(
                "Run `sleep 1 && echo forge-queued-command-scenario` with the Bash \
                 tool and report what the command printed.",
            )
            .await?;

        // Brief pause so the first turn is mid-stream when we
        // submit the second prompt. Without this the CLI sometimes
        // delivers both prompts inside a single user envelope to
        // the model and the queued-command path never fires.
        tokio::time::sleep(Duration::from_millis(400)).await;

        // Submit a second user message WHILE the first turn is
        // still in flight. Claude buffers this internally. The
        // captured trace must show no `queued_command` /
        // `attachment` envelopes on stdout - that absence is the
        // artifact this scenario locks in.
        client.send_user_message("Also, briefly summarise the output in one sentence.").await?;

        Ok((client, events))
    })
    .await
    .expect("scenario run");
}
