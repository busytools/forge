//! Live-capture scenario: outbound `generate_session_title`
//! `control_request`.
//!
//! Exercises the title-generation path: forge-sdk asks the CLI to
//! produce a short title for the session from a free-form description.
//! Captures the request shape (`{subtype: "generate_session_title",
//! description: ..., persist: true}`) and the CLI's response.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forge_sdk::{OptionsBuilder, PermissionMode};
use forge_test_harness::sdk_wire::run_live_scenario;

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_generate_session_title() {
    let opts = OptionsBuilder::new()
        .max_turns(1)
        .permission_mode(PermissionMode::AcceptEdits)
        .build();

    run_live_scenario("generate_session_title", opts, |client, events| async move {
        // Drive a trivial conversation first so the CLI has context to
        // summarise. The title generator typically uses the first user
        // turn as input.
        client
            .send_user_message("Respond with only the word DONE.")
            .await?;
        // The harness drains until a Result frame; we issue
        // generate_session_title after the conversation completes so
        // the control round trip lands at the tail of the trace.
        let title = client
            .generate_session_title("test description for title generation")
            .await?;
        eprintln!("generate_session_title captured: {title:?}");
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}
