//! Verifies `Client::spawn` injects the env vars Python SDK v0.1.64
//! stamps on every subprocess launch
//! (`_internal/transport/subprocess_cli.py:395-437`):
//!
//! - `CLAUDE_CODE_ENTRYPOINT=sdk-rs` (forge-sdk's own attribution —
//!   upstream Python SDK stamps `sdk-py` here; we identify as Rust).
//! - `CLAUDE_AGENT_SDK_VERSION=<crate version>`
//! - `PWD=<cwd>` (when `Options::cwd`).
//!
//! Filtering of `CLAUDECODE` (upstream #573) is not covered by a
//! Rust-side unit test — `forbid(unsafe_code)` blocks the env mutation
//! a faithful test would need. The call is visually asserted against
//! `env_remove` in `transport/process.rs`.
//!
//! The mock fixture dumps its env to a file; we parse and assert.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::HashMap;
use std::fs;

use forge_sdk::{Client, OptionsBuilder};

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

async fn spawn_and_capture_env(
    opts_cb: impl FnOnce(OptionsBuilder) -> OptionsBuilder,
) -> HashMap<String, String> {
    let dir = tempfile::tempdir().expect("tempdir");
    let dump = dir.path().join("env.txt");
    // `env!("FORGE_TEST_ENV_DUMP")` is read by the mock; set it via
    // options.env so our spawn path sees it.
    let mut builder = OptionsBuilder::new().binary(fixture("mock_claude_env.sh"));
    builder = builder.env("FORGE_TEST_ENV_DUMP", dump.to_string_lossy().into_owned());
    builder = opts_cb(builder);
    let opts = builder.build();
    let (client, _events) = Client::spawn(opts).await.expect("spawn");
    client.disconnect().await.expect("disconnect");
    parse_env(&dump)
}

#[tokio::test]
async fn spawn_sets_entrypoint_and_version_envs() {
    let env = spawn_and_capture_env(|b| b).await;
    assert_eq!(
        env.get("CLAUDE_CODE_ENTRYPOINT").map(String::as_str),
        Some("sdk-rs"),
        "CLAUDE_CODE_ENTRYPOINT must be stamped to identify forge-sdk (Rust) to the CLI — upstream Python SDK stamps sdk-py here"
    );
    let version =
        env.get("CLAUDE_AGENT_SDK_VERSION").expect("CLAUDE_AGENT_SDK_VERSION must be stamped");
    assert_eq!(version, env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
async fn spawn_sets_pwd_to_cwd_when_present() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_path_buf();
    let env = spawn_and_capture_env(|b| b.cwd(cwd.clone())).await;
    assert_eq!(
        env.get("PWD").map(String::as_str),
        Some(cwd.to_string_lossy().as_ref()),
        "PWD must match the Options::cwd (Python subprocess_cli.py:440-441)"
    );
}

#[tokio::test]
async fn options_env_can_override_entrypoint_but_not_sdk_version() {
    // Python: options.env is applied BEFORE CLAUDE_AGENT_SDK_VERSION
    // stamping, so callers can re-label ENTRYPOINT but cannot spoof
    // the SDK version.
    let env = spawn_and_capture_env(|b| {
        b.env("CLAUDE_CODE_ENTRYPOINT", "custom-entry")
            .env("CLAUDE_AGENT_SDK_VERSION", "999.999.999")
    })
    .await;
    assert_eq!(
        env.get("CLAUDE_CODE_ENTRYPOINT").map(String::as_str),
        Some("custom-entry"),
        "options.env must override CLAUDE_CODE_ENTRYPOINT"
    );
    assert_eq!(
        env.get("CLAUDE_AGENT_SDK_VERSION").map(String::as_str),
        Some(env!("CARGO_PKG_VERSION")),
        "SDK version must NOT be overridable from options.env"
    );
}
