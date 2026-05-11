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

    let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));

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
async fn projects_pane_visibility_round_trips_through_forge_state() {
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
async fn ui_toggle_preserves_account_picker_state() {
    // Toggling the Projects-pane visibility writes the full
    // forge-state.toml; the [accounts]/[selection] sections that the
    // picker writes must NOT be wiped when the UI toggle fires.
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
config_dir = "/tmp/forge-test-ui-toggle-subspace"

[[accounts]]
display_name = "Granite"
config_dir = "/tmp/forge-test-ui-toggle-granite"
"#,
    )
    .expect("write forge.toml");

    let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));

    // Spawn so the picker writes [accounts].last_used_at.
    let _ = workspace
        .get_agent_handle(SessionTarget::Default, SessionLaunchSettings::default())
        .await
        .expect("spawn");

    // Now toggle the Projects pane — full state file gets rewritten.
    workspace.set_projects_pane_visible(false);

    let state_text =
        std::fs::read_to_string(dir.path().join("forge-state.toml")).expect("read state");
    assert!(state_text.contains("last_used_at"), "account picker state preserved on UI write");
    assert!(state_text.contains("projects_pane_visible"), "ui section written");
}

#[tokio::test]
async fn picker_writes_preserve_ui_state() {
    // Inverse direction: an account-picker write must NOT clobber a
    // previously-set Projects-pane visibility.
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
config_dir = "/tmp/forge-test-picker-preserves-ui"
"#,
    )
    .expect("write forge.toml");

    let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
    workspace.set_projects_pane_visible(false);

    // Now spawn — picker writes forge-state.toml.
    let _ = workspace
        .get_agent_handle(SessionTarget::Default, SessionLaunchSettings::default())
        .await
        .expect("spawn");

    drop(workspace);
    let reloaded = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("re-load"));
    assert!(!reloaded.projects_pane_visible(), "picker write did not clobber UI state");
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

    let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));

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
