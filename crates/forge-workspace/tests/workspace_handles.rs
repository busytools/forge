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
use std::sync::Arc;

use forge_workspace::{SessionKey, SessionLaunchSettings, SessionTarget, Workspace};
use tempfile::tempdir;

#[tokio::test]
async fn cold_cache_dual_spawns_pin_to_first_in_allow_list() {
    // The pinned `accounts = [...]` order is the determinism source
    // when the usage cache is cold. Both spawns hit Subspace because
    // it's first in the pin; the `Granite` entry is eligible but
    // loses on idx tie-break (unknown-first sorts by enumerate order
    // in `allowed`). Under live usage data, the choice would
    // depend on remaining budget per account.
    let dir = tempdir().expect("tempdir");
    fs::write(
        dir.path().join("forge.toml"),
        r#"
[[orgs]]
name = "Default"
accounts = ["Subspace", "Granite"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true

[[accounts]]
display_name = "Subspace"
config_dir = "/tmp/forge-test-subspace"

[[accounts]]
display_name = "Granite"
config_dir = "/tmp/forge-test-granite"
"#,
    )
    .expect("write forge.toml");

    let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));

    let h1 = workspace
        .get_agent_handle(SessionTarget::Default, SessionLaunchSettings::default())
        .expect("first spawn");
    assert_eq!(
        h1.config_dir(),
        PathBuf::from("/tmp/forge-test-subspace"),
        "first spawn binds to Subspace's config_dir (first in pin)",
    );

    // Second spawn under a distinct SessionTarget — same pin, same
    // cold cache, picks Subspace again deterministically.
    let other = SessionKey::from_str_for_test("dual-account-other");
    let h2 = workspace
        .get_agent_handle(SessionTarget::Session(other), SessionLaunchSettings::default())
        .expect("second spawn");
    assert_eq!(
        h2.config_dir(),
        PathBuf::from("/tmp/forge-test-subspace"),
        "second spawn also binds to Subspace's config_dir under cold cache",
    );
}

#[tokio::test]
async fn picker_display_name_reaches_bridge() {
    let dir = tempdir().expect("tempdir");
    fs::write(
        dir.path().join("forge.toml"),
        r#"
[[orgs]]
name = "Default"
accounts = ["Subspace"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true

[[accounts]]
display_name = "Subspace"
config_dir = "/tmp/forge-test-display-subspace"

[[accounts]]
display_name = "Granite"
config_dir = "/tmp/forge-test-display-granite"
"#,
    )
    .expect("write forge.toml");

    let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));

    // Cold cache → both spawns pick Subspace (first in pin). The
    // important assertion here is that the bridge actually carries
    // a display_name through to the AgentHandle.
    let h1 = workspace
        .get_agent_handle(SessionTarget::Default, SessionLaunchSettings::default())
        .expect("first spawn");
    assert_eq!(
        h1.display_name().as_deref(),
        Some("Subspace"),
        "first spawn binds to Subspace's display_name (first in pin, cold cache)",
    );

    let other = SessionKey::from_str_for_test("display-name-other");
    let h2 = workspace
        .get_agent_handle(SessionTarget::Session(other), SessionLaunchSettings::default())
        .expect("second spawn");
    assert_eq!(
        h2.display_name().as_deref(),
        Some("Subspace"),
        "second spawn also binds to Subspace under cold cache",
    );
}
