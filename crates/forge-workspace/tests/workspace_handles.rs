//! `Workspace::get_agent_handle` integration tests — verify the
//! cross-crate plumbing from `forge.toml` through the account picker
//! into the spawned `AgentHandle`'s bridge env. No real `claude`
//! subprocesses are spawned; the test asserts up to the
//! `AgentHandle`/`Bridge` boundary, where `forge-sdk`'s already-
//! tested machinery (`process.rs:198` calling `cmd.env(k, v)`) takes
//! over.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::fs;

use forge_workspace::{SessionKey, SessionLaunchSettings, SessionTarget, Workspace};
use tempfile::tempdir;

#[tokio::test]
async fn dual_account_spawns_inject_distinct_claude_config_dir() {
    let dir = tempdir().expect("tempdir");
    fs::write(
        dir.path().join("forge.toml"),
        r#"
[[projects]]
name = "forge"
path = "~/Projects/forge"
default = true

[[accounts]]
display_name = "Subspace"
config_dir = "/tmp/forge-test-subspace"

[[accounts]]
display_name = "Granite"
config_dir = "/tmp/forge-test-granite"
"#,
    )
    .expect("write forge.toml");

    let workspace = Workspace::new(dir.path().to_owned()).await.expect("new");

    // First spawn — LRU picks Granite (alphabetical tie-break, no
    // usage yet: Granite < Subspace).
    let h1 = workspace
        .get_agent_handle(SessionTarget::Default, SessionLaunchSettings::default())
        .await
        .expect("first spawn");
    let env1 = h1.extra_env_for_test();
    assert_eq!(
        env1.get("CLAUDE_CONFIG_DIR").map(String::as_str),
        Some("/tmp/forge-test-granite"),
        "first spawn binds to Granite's config_dir",
    );

    // Second spawn — Subspace is now LRU since Granite was just used.
    // Use a distinct SessionTarget so the pool key differs and a fresh
    // spawn actually happens.
    let other = SessionKey::from_str_for_test("dual-account-other");
    let h2 = workspace
        .get_agent_handle(SessionTarget::Session(other), SessionLaunchSettings::default())
        .await
        .expect("second spawn");
    let env2 = h2.extra_env_for_test();
    assert_eq!(
        env2.get("CLAUDE_CONFIG_DIR").map(String::as_str),
        Some("/tmp/forge-test-subspace"),
        "second spawn binds to Subspace's config_dir",
    );
}
