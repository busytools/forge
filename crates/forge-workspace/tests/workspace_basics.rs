//! `Workspace::new` + `list_projects` integration tests.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::fs;
use std::sync::Arc;

use forge_workspace::Workspace;
use tempfile::tempdir;

/// Ensure `forge/` exists and return the production `forge/forge.toml`
/// path, so tests write where forge reads (not the legacy fallback).
fn forge_toml_path(config_dir: &std::path::Path) -> std::path::PathBuf {
    let forge = config_dir.join("forge");
    fs::create_dir_all(&forge).expect("forge/ dir");
    forge.join("forge.toml")
}

fn write_default_config(dir: &std::path::Path) {
    fs::write(
        forge_toml_path(dir),
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
config_dir = "~/.claude-stargate"
provider = "anthropic"
"#,
    )
    .expect("write forge.toml");
}

#[tokio::test]
async fn new_loads_config_and_lists_projects() {
    let dir = tempdir().expect("tempdir");
    write_default_config(dir.path());

    let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
    let projects = workspace.list_projects();
    assert_eq!(projects.len(), 1, "one [[orgs.projects]] entry should yield one ProjectView");
    let project = &projects[0];
    // The `display_path` retains the `~` form from forge.toml.
    assert_eq!(project.display_path, "~/Projects/forge");
}

#[tokio::test]
async fn new_creates_forge_data_dir() {
    let dir = tempdir().expect("tempdir");
    write_default_config(dir.path());
    let _workspace = Workspace::new_for_test(dir.path().to_owned()).expect("new");
    assert!(dir.path().join("forge").is_dir(), "Workspace::new creates the forge/ subfolder");
}

#[tokio::test]
async fn new_returns_err_when_config_missing() {
    let dir = tempdir().expect("tempdir");
    let result = Workspace::new_for_test(dir.path().to_owned());
    assert!(result.is_err(), "missing forge.toml should error");
}

#[tokio::test]
async fn new_refuses_second_instance_on_same_config_dir() {
    let dir = tempdir().expect("tempdir");
    write_default_config(dir.path());

    // The first instance acquires the per-config-dir single-instance
    // lock and holds it for its lifetime.
    let _first = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("first new"));

    // A second forge on the SAME config dir is refused with the holder's
    // PID - a clean error, not a panic.
    match Workspace::new_for_test(dir.path().to_owned()) {
        Err(forge_workspace::WorkspaceError::AlreadyRunning { pid }) => {
            assert_eq!(pid, Some(std::process::id()), "refusal names the holder's PID");
        }
        Ok(_) => panic!("second Workspace::new on the same config dir must be refused"),
        Err(other) => panic!("expected AlreadyRunning, got {other:?}"),
    }
}

#[tokio::test]
async fn list_projects_includes_projects_with_no_catalog_entries() {
    // forge.toml lists a project whose path has no on-disk session
    // history (the tempdir's `~/Projects/forge` doesn't actually
    // exist, so the catalog has no entries for it). Workspace
    // should still surface it via list_projects with an empty
    // sessions Vec.
    let dir = tempdir().expect("tempdir");
    write_default_config(dir.path());

    let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
    let projects = workspace.list_projects();
    assert_eq!(projects.len(), 1, "forge.toml lists exactly one project");

    let project = &projects[0];
    // Sessions may be empty (catalog isolation in test env). Even
    // if the developer's real ~/.claude has entries for this path,
    // is_open must be false because no agents are spawned.
    for session in &project.sessions {
        assert!(!session.is_open, "no agents spawned -> is_open: false");
    }
    // The forge.toml project surfaces regardless of catalog content.
    assert_eq!(project.display_path, "~/Projects/forge");
}
