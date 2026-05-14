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
    // when the usage cache is cold. Both spawns hit Stargate because
    // it's first in the pin; the `Gateway` entry is eligible but
    // loses on idx tie-break (unknown-first sorts by enumerate order
    // in `allowed`). Under live usage data, the choice would
    // depend on remaining budget per account.
    let dir = tempdir().expect("tempdir");
    fs::write(
        dir.path().join("forge.toml"),
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
        .await
        .expect("first spawn");
    assert_eq!(
        h1.config_dir_for_test(),
        PathBuf::from("/tmp/forge-test-stargate"),
        "first spawn binds to Stargate's config_dir (first in pin)",
    );

    // Second spawn under a distinct SessionTarget — same pin, same
    // cold cache, picks Stargate again deterministically.
    let other = SessionKey::from_str_for_test("dual-account-other");
    let h2 = workspace
        .get_agent_handle(SessionTarget::Session(other), SessionLaunchSettings::default())
        .await
        .expect("second spawn");
    assert_eq!(
        h2.config_dir_for_test(),
        PathBuf::from("/tmp/forge-test-stargate"),
        "second spawn also binds to Stargate's config_dir under cold cache",
    );
}

#[tokio::test]
async fn projects_pane_visibility_round_trips_through_forge_state() {
    let dir = tempdir().expect("tempdir");
    fs::write(
        dir.path().join("forge.toml"),
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
config_dir = "/tmp/forge-test-pane-vis"
"#,
    )
    .expect("write forge.toml");

    // Default on first launch is true (no forge-state.toml yet).
    let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
    assert!(workspace.projects_pane_visible(), "default visibility is true");

    // Flip to false, drop, reload — the preference must survive.
    workspace.set_projects_pane_visible(false);
    drop(workspace);

    let workspace2 = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("re-load false"));
    assert!(!workspace2.projects_pane_visible(), "false survives round trip");

    // Flip back to true, reload — same again.
    workspace2.set_projects_pane_visible(true);
    drop(workspace2);

    let workspace3 = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("re-load true"));
    assert!(workspace3.projects_pane_visible(), "true survives round trip");
}

#[tokio::test]
async fn ui_toggle_writes_state_file() {
    // Toggling the Projects-pane visibility writes the full
    // forge-state.toml. The selection-state sections that older
    // versions persisted are gone — the account picker now drives
    // off the in-memory usage cache, so there's no per-spawn
    // persistence to preserve. Just verify the UI section lands.
    let dir = tempdir().expect("tempdir");
    fs::write(
        dir.path().join("forge.toml"),
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
config_dir = "/tmp/forge-test-ui-toggle-stargate"
"#,
    )
    .expect("write forge.toml");

    let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
    workspace.set_projects_pane_visible(false);

    let state_text =
        std::fs::read_to_string(dir.path().join("forge-state.toml")).expect("read state");
    assert!(state_text.contains("projects_pane_visible"), "ui section written");
}

#[tokio::test]
async fn ui_state_round_trips_across_spawns() {
    // Verify a Projects-pane visibility set BEFORE a spawn survives
    // the spawn (which used to also write the file). Picker writes
    // are gone but the UI state still round-trips through the
    // shared `persist_state` path.
    let dir = tempdir().expect("tempdir");
    fs::write(
        dir.path().join("forge.toml"),
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
config_dir = "/tmp/forge-test-picker-preserves-ui"
"#,
    )
    .expect("write forge.toml");

    let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
    workspace.set_projects_pane_visible(false);

    let _ = workspace
        .get_agent_handle(SessionTarget::Default, SessionLaunchSettings::default())
        .await
        .expect("spawn");

    drop(workspace);
    let reloaded = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("re-load"));
    assert!(!reloaded.projects_pane_visible(), "UI state survives reload");
}

#[tokio::test]
async fn picker_display_name_reaches_bridge() {
    let dir = tempdir().expect("tempdir");
    fs::write(
        dir.path().join("forge.toml"),
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
        .await
        .expect("first spawn");
    assert_eq!(
        h1.display_name().as_deref(),
        Some("Stargate"),
        "first spawn binds to Stargate's display_name (first in pin, cold cache)",
    );

    let other = SessionKey::from_str_for_test("display-name-other");
    let h2 = workspace
        .get_agent_handle(SessionTarget::Session(other), SessionLaunchSettings::default())
        .await
        .expect("second spawn");
    assert_eq!(
        h2.display_name().as_deref(),
        Some("Stargate"),
        "second spawn also binds to Stargate under cold cache",
    );
}
