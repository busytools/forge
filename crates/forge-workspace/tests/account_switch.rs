//! `/account` mid-session switch integration test - verifies
//! `Command::SwitchAccount` re-spawns the SAME session key under the
//! picked account's `config_dir`, forcing the account rather than
//! letting the round-robin picker choose. Mirrors `workspace_handles`:
//! asserts at the `AgentHandle`/bridge `config_dir` boundary, no real
//! `claude` conversation required.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use forge_workspace::{Command, SessionKey, SessionLaunchSettings, SessionTarget, Workspace};
use tempfile::tempdir;

fn forge_toml_path(config_dir: &std::path::Path) -> PathBuf {
    let forge = config_dir.join("forge");
    fs::create_dir_all(&forge).expect("forge/ dir");
    forge.join("forge.toml")
}

#[tokio::test]
async fn switch_account_respawns_same_session_under_forced_config_dir() {
    // Three accounts. A cold-cache initial spawn takes cursor=0 (Aacct);
    // an UNforced re-spawn would advance the round-robin to cursor=1
    // (Bacct). The switch targets Cacct, so landing on Cacct's
    // config_dir proves the account was FORCED, not merely rotated to.
    let dir = tempdir().expect("tempdir");
    fs::write(
        forge_toml_path(dir.path()),
        r#"
[[orgs]]
name = "Default"
accounts = ["Aacct", "Bacct", "Cacct"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true

[[accounts]]
display_name = "Aacct"
config_dir = "/tmp/forge-switch-a"

[[accounts]]
display_name = "Bacct"
config_dir = "/tmp/forge-switch-b"

[[accounts]]
display_name = "Cacct"
config_dir = "/tmp/forge-switch-c"
"#,
    )
    .expect("write forge.toml");

    let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));

    // Initial spawn: cold cache -> first usable in the pin (Aacct).
    let key = SessionKey::from_str_for_test("switch-target");
    let handle = workspace
        .get_agent_handle(SessionTarget::Session(key.clone()), SessionLaunchSettings::default())
        .expect("initial spawn");
    assert_eq!(
        handle.config_dir(),
        PathBuf::from("/tmp/forge-switch-a"),
        "initial spawn binds to account A (first usable, cold cache)",
    );

    // Switch to Cacct - the account the round-robin would skip past.
    workspace
        .dispatch(Command::SwitchAccount {
            key: key.clone(),
            account_display_name: "Cacct".to_owned(),
            launch_settings: SessionLaunchSettings::default(),
        })
        .expect("dispatch switch");

    // Same key, re-spawned under the FORCED account C's config_dir.
    assert_eq!(
        workspace.config_dir_for(&key),
        Some(PathBuf::from("/tmp/forge-switch-c")),
        "switch re-spawns the SAME session key under the forced account C's config_dir",
    );
    assert!(
        workspace.has_agent_for(&key),
        "the re-spawned session keeps its live agent under the same key",
    );
}
