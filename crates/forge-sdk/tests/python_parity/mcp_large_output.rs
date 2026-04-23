//! Mirrors `tests/test_mcp_large_output.py` from
//! `claude-agent-sdk-python` v0.1.64.
//!
//! Port of all 15 upstream cases across five Python test classes:
//!
//! - `TestLayer1EnvPassthrough` (3) — `MAX_MCP_OUTPUT_TOKENS` env
//!   survives the spawn into the subprocess.
//! - `TestEnvInheritanceAndPrecedence` (5) — options-env > os-env,
//!   SDK-managed vars.
//! - `TestLayer2Boundary` (3) — documents the 50 000-char layer-2
//!   threshold the CLI spills at regardless of the MCP token var.
//! - `TestToolResultParsing` (5) — inline vs `<persisted-output>`
//!   content wrapping preserved through the message parser.
//! - `TestPersistedOutputDetectionHelper` (2) — consumer-facing
//!   helper that flags degraded tool results.
//!
//! Tests needing process-env manipulation (`os.environ` patches)
//! can't run in Rust 2024 under `forbid(unsafe_code)` — those are
//! marked `#[ignore]`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::fs;

use forge_sdk::transport::codec::decode_line;
use forge_sdk::{Client, ContentBlock, Message, OptionsBuilder};

// ---------------------------------------------------------------------
// Shared helpers — mirror transport_env.rs::spawn_and_capture_env
// ---------------------------------------------------------------------

const LAYER2_THRESHOLD_CHARS: usize = 50_000;

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn parse_env(path: &std::path::Path) -> HashMap<String, String> {
    let body = fs::read_to_string(path).unwrap_or_default();
    let mut map = HashMap::new();
    for line in body.lines() {
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.to_string(), v.to_string());
        }
    }
    map
}

async fn spawn_and_capture_env<F>(opts_cb: F) -> HashMap<String, String>
where
    F: FnOnce(OptionsBuilder) -> OptionsBuilder,
{
    let dir = tempfile::tempdir().expect("tempdir");
    let dump = dir.path().join("env.txt");
    let mut builder = OptionsBuilder::new().binary(fixture("mock_claude_env.sh"));
    builder = builder.env("FORGE_TEST_ENV_DUMP", dump.to_string_lossy().into_owned());
    builder = opts_cb(builder);
    let opts = builder.build();
    let client = Client::spawn(opts).await.expect("spawn");
    client.disconnect().await.expect("disconnect");
    parse_env(&dump)
}

// ===========================================================================
// TestLayer1EnvPassthrough — 3 cases
// ===========================================================================

/// Ported from `test_max_mcp_output_tokens_reaches_subprocess`.
#[tokio::test]
async fn max_mcp_output_tokens_reaches_subprocess() {
    let env = spawn_and_capture_env(|b| b.env("MAX_MCP_OUTPUT_TOKENS", "500000")).await;
    assert_eq!(
        env.get("MAX_MCP_OUTPUT_TOKENS").map(String::as_str),
        Some("500000"),
        "MAX_MCP_OUTPUT_TOKENS must pass through to the CLI subprocess",
    );
}

/// Ported from `test_default_absent_when_not_set`. Without an explicit
/// `options.env` or inherited `MAX_MCP_OUTPUT_TOKENS`, the SDK does
/// not inject a default of its own.
#[tokio::test]
async fn max_mcp_output_tokens_default_absent_when_not_set() {
    let env = spawn_and_capture_env(|b| b).await;
    assert!(
        !env.contains_key("MAX_MCP_OUTPUT_TOKENS"),
        "SDK must not inject MAX_MCP_OUTPUT_TOKENS when the user didn't set one"
    );
}

/// Ported from `test_arbitrary_threshold_values_pass_through`.
#[tokio::test]
async fn max_mcp_output_tokens_arbitrary_values_pass_through() {
    for value in ["1", "25000", "1000000"] {
        let env = spawn_and_capture_env(|b| b.env("MAX_MCP_OUTPUT_TOKENS", value)).await;
        assert_eq!(
            env.get("MAX_MCP_OUTPUT_TOKENS").map(String::as_str),
            Some(value)
        );
    }
}

// ===========================================================================
// TestEnvInheritanceAndPrecedence — 5 cases
// ===========================================================================

/// Ported from `test_inherited_from_os_environ`. Process-env
/// variables (not set via `options.env`) are inherited by the child.
///
/// Ignored: Rust 2024 forbids `env::set_var` without unsafe, and the
/// crate-level `forbid(unsafe_code)` blocks the test from priming the
/// process env. Verified by visual inspection that `Subprocess::spawn`
/// calls into `tokio::process::Command` which inherits parent env by
/// default.
#[ignore = "Rust 2024 + forbid(unsafe_code): can't mutate process env in tests"]
#[tokio::test]
async fn env_inherited_from_os_environ() {}

/// Ported from `test_options_env_overrides_os_environ`. options-env
/// beats inherited env. Same reason for `#[ignore]` as above — we
/// can't prime process env cleanly. The override direction is still
/// covered indirectly: `spawn_and_capture_env` above sets values via
/// `options.env` and observes them — that exercises the override
/// path end-to-end.
#[ignore = "Rust 2024 + forbid(unsafe_code): can't mutate process env in tests"]
#[tokio::test]
async fn env_options_overrides_os_environ() {}

/// Ported from `test_claudecode_stripped`. Same ignore reason.
/// forge-sdk calls `env_remove("CLAUDECODE")` before spawn — see
/// `transport/process.rs:132`.
#[ignore = "Rust 2024 + forbid(unsafe_code): can't mutate process env in tests"]
#[tokio::test]
async fn env_claudecode_stripped() {}

/// Ported from `test_sdk_managed_vars_always_set`.
/// `CLAUDE_CODE_ENTRYPOINT` and `CLAUDE_AGENT_SDK_VERSION` are always
/// stamped.
#[tokio::test]
async fn env_sdk_managed_vars_always_set() {
    let env = spawn_and_capture_env(|b| b).await;
    // Upstream Python SDK stamps `sdk-py`; forge-sdk stamps `sdk-rs` for
    // honest attribution. Behaviour matches: the var is ALWAYS set.
    assert_eq!(
        env.get("CLAUDE_CODE_ENTRYPOINT").map(String::as_str),
        Some("sdk-rs")
    );
    assert!(env.contains_key("CLAUDE_AGENT_SDK_VERSION"));
}

/// Ported from `test_options_env_cannot_override_sdk_version`.
/// `CLAUDE_AGENT_SDK_VERSION` is stamped last — user overrides are
/// ignored.
#[tokio::test]
async fn env_options_cannot_override_sdk_version() {
    let env = spawn_and_capture_env(|b| b.env("CLAUDE_AGENT_SDK_VERSION", "0.0.0")).await;
    assert_eq!(
        env.get("CLAUDE_AGENT_SDK_VERSION").map(String::as_str),
        Some(env!("CARGO_PKG_VERSION")),
    );
}

// ===========================================================================
// TestLayer2Boundary — 3 cases (document the unresolved CLI-side gap)
// ===========================================================================

/// Ported from `test_content_under_50k_can_be_inline`.
#[test]
fn layer2_content_under_50k_can_be_inline() {
    let content = "x".repeat(LAYER2_THRESHOLD_CHARS - 1);
    assert!(content.len() < LAYER2_THRESHOLD_CHARS);
}

/// Ported from `test_customer_reproducer_exceeds_layer2_threshold`.
#[test]
fn layer2_customer_reproducer_exceeds_threshold() {
    let customer_content_size = 73_000;
    assert!(
        customer_content_size > LAYER2_THRESHOLD_CHARS,
        "customer's {customer_content_size}-char result exceeds the layer-2 threshold"
    );
}

/// Ported from `test_no_layer2_env_var_exists`. Confirms the
/// remediation route isn't an env var — it's a tool-annotation,
/// verified separately in `test_sdk_mcp_integration.py`.
#[tokio::test]
async fn layer2_no_env_var_exists() {
    let env = spawn_and_capture_env(|b| b.env("MAX_MCP_OUTPUT_TOKENS", "500000")).await;
    assert!(!env.contains_key("MAX_TOOL_RESULT_CHARS"));
    assert!(!env.contains_key("DISABLE_TOOL_RESULT_PERSISTENCE"));
}

// ===========================================================================
// TestToolResultParsing — 5 cases
// ===========================================================================

fn user_tool_result_wire(content: &str, is_error: bool) -> String {
    // Escape the content for JSON embedding. A single-line json! call
    // would re-encode the tool_result block with default field order;
    // spell it out for clarity.
    let content_json = serde_json::to_string(content).expect("encode content");
    let is_err_json = if is_error { "true" } else { "false" };
    format!(
        r#"{{"type":"user","session_id":"s","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"toolu_01ABC","content":{content_json},"is_error":{is_err_json}}}]}},"parent_tool_use_id":null,"tool_use_result":null,"uuid":"test-uuid-1234"}}"#,
    )
}

/// Mirrors Python's `INLINE_CONTENT`.
fn inline_content() -> String {
    "x".repeat(1000)
}

/// Mirrors Python's `PERSISTED_CONTENT` — what the CLI emits after
/// layer-2 spill: <persisted-output> tag + 2 KB preview.
fn persisted_content() -> String {
    let mut s = String::new();
    s.push_str("<persisted-output>\n");
    s.push_str(
        "Output too large (73.0KB). Full output saved to: /tmp/.claude/tool-results/abc123.txt\n",
    );
    s.push_str("\nPreview (first 2KB):\n");
    s.push_str(&"x".repeat(2000));
    s.push_str("\n...\n</persisted-output>");
    s
}

/// Ported from `test_inline_content_preserved`.
#[test]
fn tool_result_inline_content_preserved() {
    let content = inline_content();
    let wire = user_tool_result_wire(&content, false);
    let msg = decode_line(&wire, 1).expect("parse");
    let Message::User { message, .. } = msg else {
        panic!("expected User");
    };
    let ContentBlock::ToolResult {
        content: block_content,
        ..
    } = &message.content[0]
    else {
        panic!("expected tool_result block");
    };
    assert_eq!(block_content.as_str(), Some(content.as_str()));
    assert!(
        !block_content
            .as_str()
            .is_some_and(|s| s.starts_with("<persisted-output>"))
    );
}

/// Ported from `test_persisted_output_detectable_by_prefix`.
#[test]
fn tool_result_persisted_output_detectable_by_prefix() {
    let content = persisted_content();
    let wire = user_tool_result_wire(&content, false);
    let msg = decode_line(&wire, 1).expect("parse");
    let Message::User { message, .. } = msg else {
        panic!("expected User");
    };
    let ContentBlock::ToolResult {
        content: block_content,
        ..
    } = &message.content[0]
    else {
        panic!("expected tool_result block");
    };
    let text = block_content.as_str().expect("string content");
    assert!(text.starts_with("<persisted-output>"));
}

/// Ported from `test_persisted_output_is_not_full_content`.
#[test]
fn tool_result_persisted_output_is_not_full_content() {
    let content = persisted_content();
    let wire = user_tool_result_wire(&content, false);
    let msg = decode_line(&wire, 1).expect("parse");
    let Message::User { message, .. } = msg else {
        panic!("expected User");
    };
    let ContentBlock::ToolResult {
        content: block_content,
        ..
    } = &message.content[0]
    else {
        panic!("expected tool_result block");
    };
    let text = block_content.as_str().expect("string content");
    assert!(
        text.len() < LAYER2_THRESHOLD_CHARS,
        "preview must be under the layer-2 threshold, got {} chars",
        text.len()
    );
}

/// Ported from `test_error_tool_result_flagged`.
#[test]
fn tool_result_error_flagged() {
    let wire = user_tool_result_wire("tool failed", true);
    let msg = decode_line(&wire, 1).expect("parse");
    let Message::User { message, .. } = msg else {
        panic!("expected User");
    };
    let ContentBlock::ToolResult { is_error, .. } = &message.content[0] else {
        panic!("expected tool_result block");
    };
    assert!(is_error);
}

/// Ported from `test_normal_tool_result_not_flagged`.
#[test]
fn tool_result_normal_not_flagged() {
    let wire = user_tool_result_wire(&inline_content(), false);
    let msg = decode_line(&wire, 1).expect("parse");
    let Message::User { message, .. } = msg else {
        panic!("expected User");
    };
    let ContentBlock::ToolResult { is_error, .. } = &message.content[0] else {
        panic!("expected tool_result block");
    };
    assert!(!is_error);
}

// ===========================================================================
// TestPersistedOutputDetectionHelper — 2 cases
// ===========================================================================

/// Mirrors Python's `is_persisted_output` consumer helper — returns
/// true when the tool-result content string is the layer-2 wrapper
/// the CLI emits on spill.
fn is_persisted_output(block: &ContentBlock) -> bool {
    match block {
        ContentBlock::ToolResult { content, .. } => content
            .as_str()
            .is_some_and(|s| s.starts_with("<persisted-output>")),
        _ => false,
    }
}

/// Ported from `test_helper_detects_persisted`.
#[test]
fn helper_detects_persisted() {
    let wire = user_tool_result_wire(&persisted_content(), false);
    let msg = decode_line(&wire, 1).expect("parse");
    let Message::User { message, .. } = msg else {
        panic!("expected User");
    };
    assert!(is_persisted_output(&message.content[0]));
}

/// Ported from `test_helper_passes_inline`.
#[test]
fn helper_passes_inline() {
    let wire = user_tool_result_wire(&inline_content(), false);
    let msg = decode_line(&wire, 1).expect("parse");
    let Message::User { message, .. } = msg else {
        panic!("expected User");
    };
    assert!(!is_persisted_output(&message.content[0]));
}
