//! `Workspace::get_agent_handle` integration tests — verify the
//! cross-crate plumbing from `forge.toml` through the account picker
//! into the spawned `AgentHandle`'s bound `config_dir`. No real
//! `claude` subprocesses are spawned; the test asserts up to the
//! `AgentHandle`/`Bridge` boundary, where the bridge's typed
//! `config_dir` field is the source of truth (read by every
//! in-process accessor and exported as `CLAUDE_CONFIG_DIR` to the
//! spawned subprocess).

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::fs;
use std::path::PathBuf;

use forge_workspace::{SessionKey, SessionLaunchSettings, SessionTarget, Workspace};
use tempfile::tempdir;

#[tokio::test]
async fn dual_account_spawns_bind_distinct_config_dirs() {
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
    assert_eq!(
        h1.config_dir_for_test(),
        PathBuf::from("/tmp/forge-test-granite"),
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
    assert_eq!(
        h2.config_dir_for_test(),
        PathBuf::from("/tmp/forge-test-subspace"),
        "second spawn binds to Subspace's config_dir",
    );
}

#[tokio::test]
async fn picker_display_name_reaches_bridge() {
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
config_dir = "/tmp/forge-test-display-subspace"

[[accounts]]
display_name = "Granite"
config_dir = "/tmp/forge-test-display-granite"
"#,
    )
    .expect("write forge.toml");

    let workspace = Workspace::new(dir.path().to_owned()).await.expect("new");

    // First spawn — LRU picks Granite (alphabetical tie-break, no
    // usage yet: Granite < Subspace). Bridge should carry Granite's
    // display_name.
    let h1 = workspace
        .get_agent_handle(SessionTarget::Default, SessionLaunchSettings::default())
        .await
        .expect("first spawn");
    assert_eq!(
        h1.display_name().as_deref(),
        Some("Granite"),
        "first spawn binds to Granite's display_name",
    );

    // Second spawn under a fresh SessionTarget — Subspace is now LRU.
    let other = SessionKey::from_str_for_test("display-name-other");
    let h2 = workspace
        .get_agent_handle(SessionTarget::Session(other), SessionLaunchSettings::default())
        .await
        .expect("second spawn");
    assert_eq!(
        h2.display_name().as_deref(),
        Some("Subspace"),
        "second spawn binds to Subspace's display_name",
    );
}
