//! Mirrors `tests/test_client.py` from `claude-agent-sdk-python`
//! v0.1.64.
//!
//! Port of all 6 upstream cases from `TestQueryFunction`.
//!
//! Python's file leans heavily on `unittest.mock` to patch
//! `InternalClient`, `SubprocessCLITransport`, and the `Query`
//! coordinator so tests stay hermetic. forge-sdk doesn't have that
//! layering — `Client::spawn` drives `Subprocess` directly — so the
//! Rust port exercises the same behaviours via the shipped mock
//! binary (`tests/fixtures/mock_claude.sh`) rather than mocking Rust
//! internals. The three behavioural tests port cleanly; the three
//! Python-internal-state tests (`initialize_timeout` env, `spawn_task`
//! deadlock guard) are captured as `#[ignore]` with their forge-sdk
//! status.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use forge_sdk::{ContentBlock, Message, Options, OptionsBuilder, PermissionMode, query};

fn mock_binary_path() -> String {
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mock_claude.sh").into()
}

fn mock_options() -> Options {
    OptionsBuilder::new().binary(mock_binary_path()).build()
}

/// Ported from `test_query_single_prompt`. `query("What is 2+2?")`
/// returns the full message stream — at least one assistant turn +
/// one result frame.
#[tokio::test]
async fn query_single_prompt() {
    let messages = query("What is 2+2?", Some(mock_options()))
        .await
        .expect("query");
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::Assistant { .. })),
        "must see at least one Assistant message"
    );
    let assistant = messages
        .iter()
        .find(|m| matches!(m, Message::Assistant { .. }))
        .expect("assistant");
    let Message::Assistant { message, .. } = assistant else {
        unreachable!()
    };
    let ContentBlock::Text { text } = &message.content[0] else {
        panic!("expected text block")
    };
    assert!(!text.is_empty());
}

/// Ported from `test_query_with_options`. Options pass through to the
/// subprocess. Python asserts the `call_args` contain the user's options
/// dict; forge-sdk's equivalent: verify that options-driven argv flags
/// (`max_turns`, `system_prompt`, `allowed_tools`, `permission_mode`)
/// all survive the call-chain. Validation is handled in the dedicated
/// `argv_composition.rs` tests — here we just confirm `query()`
/// accepts and honours an options value without panicking.
#[tokio::test]
async fn query_with_options() {
    let opts = OptionsBuilder::new()
        .binary(mock_binary_path())
        .allowed_tools(["Read", "Write"])
        .system_prompt(forge_sdk::SystemPromptKind::Inline(
            "You are helpful".into(),
        ))
        .permission_mode(PermissionMode::AcceptEdits)
        .max_turns(5)
        .build();
    let messages = query("Hi", Some(opts)).await.expect("query");
    assert!(!messages.is_empty());
}

/// Ported from `test_query_with_cwd`. Custom cwd flows through to the
/// subprocess. Python verifies the mock transport constructor got the
/// cwd kwarg; forge-sdk's `transport_env.rs::spawn_sets_pwd_to_cwd_when_present`
/// owns the argv-level assertion. Here we smoke-test that cwd-set
/// `query()` returns without error when the mock happily ignores it.
#[tokio::test]
async fn query_with_cwd() {
    let tmp = tempfile::tempdir().expect("tmp");
    let opts = OptionsBuilder::new()
        .binary(mock_binary_path())
        .cwd(PathBuf::from(tmp.path()))
        .build();
    let messages = query("test", Some(opts)).await.expect("query");
    assert!(!messages.is_empty());
}

/// Ported from `test_query_passes_initialize_timeout_from_env`. Python
/// honours `CLAUDE_CODE_STREAM_CLOSE_TIMEOUT` as the initialize
/// timeout in milliseconds.
///
/// forge-sdk has no equivalent knob today — spawn + init-drain run to
/// completion without an explicit timeout. Parity gap; minor (the CLI
/// init line arrives fast enough in practice) but tracked.
#[ignore = "parity gap: forge-sdk has no CLAUDE_CODE_STREAM_CLOSE_TIMEOUT; spawn has no init timeout"]
#[tokio::test]
async fn query_passes_initialize_timeout_from_env() {}

/// Ported from `test_query_uses_default_initialize_timeout`. Python's
/// default is 60s when the env var is absent. See
/// `query_passes_initialize_timeout_from_env` above for the gap
/// status.
#[ignore = "parity gap: forge-sdk has no initialize-timeout default either"]
#[tokio::test]
async fn query_uses_default_initialize_timeout() {}

/// Ported from `test_string_prompt_spawns_wait_for_result_as_task`.
/// Python guards a deadlock where the >50-tool-call buffer fills up
/// if `wait_for_result_and_end_input` is awaited inline rather than
/// spawned as a background task.
///
/// forge-sdk's client drives the subprocess loop in a single task —
/// the deadlock shape doesn't apply because there's no separate
/// `spawn_task` layer. `client_mock.rs::disconnect_after_send_does_not_hang`
/// is the closest forge-sdk analogue; pass-through smoke.
#[ignore = "not applicable: forge-sdk has no spawn_task layer; see client_mock.rs::disconnect_after_send_does_not_hang"]
#[tokio::test]
async fn string_prompt_spawns_wait_for_result_as_task() {}
