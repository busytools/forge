//! `Workspace::get_agent_handle` integration tests - verify the
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

/// Ensure `forge/` exists and return the production `forge/forge.toml`
/// path, so tests write where forge reads (not the legacy fallback).
fn forge_toml_path(config_dir: &std::path::Path) -> PathBuf {
    let forge = config_dir.join("forge");
    fs::create_dir_all(&forge).expect("forge/ dir");
    forge.join("forge.toml")
}

#[tokio::test]
async fn cold_cache_dual_spawns_rotate_across_allow_list() {
    // Round-robin cursor advances per pick - under a cold usage
    // cache (both accounts in tier 0 / Usable), the first spawn
    // picks Stargate (cursor=0 → first allow-list entry) and the
    // second rotates to Gateway (cursor=1). Spreads load across
    // healthy accounts instead of always hammering the first.
    let dir = tempdir().expect("tempdir");
    fs::write(
        forge_toml_path(dir.path()),
        r#"
[[orgs]]
name = "Default"
accounts = ["Stargate", "Gateway"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true

[[accounts]]
display_name = "Stargate"
config_dir = "/tmp/forge-test-stargate"

[[accounts]]
display_name = "Gateway"
config_dir = "/tmp/forge-test-gateway"
"#,
    )
    .expect("write forge.toml");

    let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));

    let h1 = workspace
        .get_agent_handle(SessionTarget::Default, SessionLaunchSettings::default())
        .expect("first spawn");
    assert_eq!(
        h1.config_dir(),
        PathBuf::from("/tmp/forge-test-stargate"),
        "first spawn (cursor=0) binds to Stargate's config_dir (first usable in allow-list)",
    );

    // Second spawn under a distinct SessionTarget - same allow-list,
    // cursor advances to 1, rotates to Gateway.
    let other = SessionKey::from_str_for_test("dual-account-other");
    let h2 = workspace
        .get_agent_handle(SessionTarget::Session(other), SessionLaunchSettings::default())
        .expect("second spawn");
    assert_eq!(
        h2.config_dir(),
        PathBuf::from("/tmp/forge-test-gateway"),
        "second spawn (cursor=1) rotates to Gateway's config_dir (round-robin)",
    );
}

#[tokio::test]
async fn picker_display_name_reaches_bridge() {
    let dir = tempdir().expect("tempdir");
    fs::write(
        forge_toml_path(dir.path()),
        r#"
[[orgs]]
name = "Default"
accounts = ["Stargate"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true

[[accounts]]
display_name = "Stargate"
config_dir = "/tmp/forge-test-display-stargate"

[[accounts]]
display_name = "Gateway"
config_dir = "/tmp/forge-test-display-gateway"
"#,
    )
    .expect("write forge.toml");

    let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));

    // Cold cache → both spawns pick Stargate (first in pin). The
    // important assertion here is that the bridge actually carries
    // a display_name through to the AgentHandle.
    let h1 = workspace
        .get_agent_handle(SessionTarget::Default, SessionLaunchSettings::default())
        .expect("first spawn");
    assert_eq!(
        h1.display_name().as_deref(),
        Some("Stargate"),
        "first spawn binds to Stargate's display_name (first in pin, cold cache)",
    );

    let other = SessionKey::from_str_for_test("display-name-other");
    let h2 = workspace
        .get_agent_handle(SessionTarget::Session(other), SessionLaunchSettings::default())
        .expect("second spawn");
    assert_eq!(
        h2.display_name().as_deref(),
        Some("Stargate"),
        "second spawn also binds to Stargate under cold cache",
    );
}
