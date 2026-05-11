//! Smoke tests for the forge-tui ↔ forge-workspace handshake at
//! startup. These confirm forge-tui can construct a workspace, get
//! its default agent handle, and surface the missing-config error
//! path cleanly. The TUI event loop itself isn't driven (that needs
//! a live `claude` subprocess); the workspace handshake is the
//! relevant integration boundary.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::fs;

use forge_workspace::{SessionLaunchSettings, SessionTarget, Workspace, WorkspaceError};
use tempfile::tempdir;

fn write_default_config(dir: &std::path::Path) {
    fs::write(
        dir.join("forge.toml"),
        r#"
[[projects]]
name = "forge"
path = "~/Projects/forge"
default = true

[[accounts]]
display_name = "Stargate"
config_dir = "~/.claude-stargate"
"#,
    )
    .expect("write forge.toml");
}

#[tokio::test]
async fn forge_tui_starts_against_fixture_default_project() {
    let dir = tempdir().expect("tempdir");
    write_default_config(dir.path());

    let workspace = Workspace::new(dir.path().to_owned())
        .await
        .expect("workspace constructs against fixture forge.toml");

    let handle = workspace
        .get_agent_handle(SessionTarget::Default, SessionLaunchSettings::default())
        .await
        .expect("default handle resolves");

    assert!(handle.take_events().is_some(), "fresh handle should own its event receiver");
}

#[tokio::test]
async fn missing_forge_toml_fails_workspace_new() {
    let dir = tempdir().expect("tempdir");
    let result = Workspace::new(dir.path().to_owned()).await;
    assert!(
        matches!(result, Err(WorkspaceError::ConfigMissing { .. })),
        "missing forge.toml should produce ConfigMissing",
    );
}
