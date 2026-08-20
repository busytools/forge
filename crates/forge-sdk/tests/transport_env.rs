//! Verifies `Client::spawn` injects the right env vars on subprocess
//! launch.
//!
//! Post wire-classification-rewriter (2026-05-20): forge-sdk no longer
//! stamps `CLAUDE_CODE_ENTRYPOINT=sdk-rs`. The rewriter proxy handles
//! classification on the wire, so any leaked stamp here would defeat
//! the rewriter for surfaces it doesn't cover yet. The CLI is left
//! to self-classify (yielding `sdk-cli` for piped stdout); the proxy
//! rewrites that to `cli` shape.
//!
//! What forge-sdk DOES stamp:
//! - `CLAUDE_AGENT_SDK_VERSION=<crate version>` (always).
//! - `PWD=<cwd>` (when `Options::cwd`).
//! - `HTTPS_PROXY` / `HTTP_PROXY` / `NODE_EXTRA_CA_CERTS` (when
//!   `Options::proxy` is set).
//! - `options.env` entries (caller-controlled).
//!
//! Filtering of `CLAUDECODE` (upstream #573) is not covered by a
//! Rust-side unit test - `forbid(unsafe_code)` blocks the env mutation
//! a faithful test would need. The call is visually asserted against
//! `env_remove` in `transport/process.rs`.

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
    let mut builder = OptionsBuilder::new().binary(fixture("mock_claude_env.sh"));
    builder = builder.env("FORGE_TEST_ENV_DUMP", dump.to_string_lossy().into_owned());
    builder = opts_cb(builder);
    let opts = builder.build();
    let (client, _events) = Client::spawn(opts).await.expect("spawn");
    client.disconnect().await.expect("disconnect");
    parse_env(&dump)
}

#[tokio::test]
async fn spawn_does_not_stamp_entrypoint_by_default() {
    let env = spawn_and_capture_env(|b| b).await;
    assert!(
        !env.contains_key("CLAUDE_CODE_ENTRYPOINT"),
        "CLAUDE_CODE_ENTRYPOINT must NOT be stamped by default - let the CLI self-classify so the rewriter has a uniform source. Stamped: {:?}",
        env.get("CLAUDE_CODE_ENTRYPOINT")
    );
}

#[tokio::test]
async fn spawn_stamps_agent_sdk_version() {
    let env = spawn_and_capture_env(|b| b).await;
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
        "PWD must match the Options::cwd"
    );
}

#[tokio::test]
async fn options_env_can_set_entrypoint_and_is_preserved() {
    // Callers may still preset CLAUDE_CODE_ENTRYPOINT through
    // options.env if they want (used by integration tests). The
    // rewriter normalises whatever ends up on the wire regardless.
    let env = spawn_and_capture_env(|b| {
        b.env("CLAUDE_CODE_ENTRYPOINT", "custom-entry")
            .env("CLAUDE_AGENT_SDK_VERSION", "999.999.999")
    })
    .await;
    assert_eq!(
        env.get("CLAUDE_CODE_ENTRYPOINT").map(String::as_str),
        Some("custom-entry"),
        "options.env must be honored when caller explicitly sets ENTRYPOINT"
    );
    assert_eq!(
        env.get("CLAUDE_AGENT_SDK_VERSION").map(String::as_str),
        Some(env!("CARGO_PKG_VERSION")),
        "SDK version must NOT be overridable from options.env"
    );
}

#[tokio::test]
async fn spawn_does_not_set_proxy_env_when_proxy_absent() {
    // The SDK must not ADD or REWRITE proxy/CA env vars when
    // Options::proxy is None. The child inherits the parent's
    // environment by default - if the parent shell already exports
    // HTTPS_PROXY (e.g. when this test runs inside a forge session
    // whose own subprocess has those vars stamped), the child sees
    // them too. Assert the SDK didn't add them above the parent
    // baseline: the child's value must either be absent or equal to
    // the parent's value.
    let parent_https = std::env::var("HTTPS_PROXY").ok();
    let parent_http = std::env::var("HTTP_PROXY").ok();
    let parent_ca = std::env::var("NODE_EXTRA_CA_CERTS").ok();
    let env = spawn_and_capture_env(|b| b).await;
    assert_eq!(
        env.get("HTTPS_PROXY").cloned(),
        parent_https,
        "SDK must not add/rewrite HTTPS_PROXY without Options::proxy"
    );
    assert_eq!(
        env.get("HTTP_PROXY").cloned(),
        parent_http,
        "SDK must not add/rewrite HTTP_PROXY without Options::proxy"
    );
    assert_eq!(
        env.get("NODE_EXTRA_CA_CERTS").cloned(),
        parent_ca,
        "SDK must not add/rewrite NODE_EXTRA_CA_CERTS without Options::proxy"
    );
}

#[tokio::test]
async fn spawn_sets_https_proxy_and_ca_when_proxy_attached() {
    // Boot a real (but unused) rewriter proxy and pass its handle.
    // The mock child doesn't actually make HTTPS calls - we just want
    // the env vars stamped on it. The CA goes under a tempdir so the
    // run never touches the real app-support dir.
    let ca_base = tempfile::tempdir().expect("tempdir");
    let handle = forge_sdk::transport::proxy::start(Some(ca_base.path()))
        .await
        .expect("start rewriter proxy");
    let env = spawn_and_capture_env({
        let h = handle.clone();
        |b| b.proxy(h)
    })
    .await;
    let expected_url = handle.proxy_url();
    assert_eq!(
        env.get("HTTPS_PROXY").map(String::as_str),
        Some(expected_url.as_str()),
        "HTTPS_PROXY must point at the rewriter proxy"
    );
    assert_eq!(
        env.get("HTTP_PROXY").map(String::as_str),
        Some(expected_url.as_str()),
        "HTTP_PROXY mirrors HTTPS_PROXY for non-TLS endpoints"
    );
    assert_eq!(
        env.get("NODE_EXTRA_CA_CERTS").map(String::as_str),
        handle.ca_cert_path().to_str(),
        "NODE_EXTRA_CA_CERTS must point at the rewriter CA"
    );
    handle.shutdown();
}
