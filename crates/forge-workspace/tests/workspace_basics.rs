//! `Workspace::new` + `list_projects` integration tests.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::fs;
use std::sync::Arc;

use forge_workspace::Workspace;
use tempfile::tempdir;

fn write_default_config(dir: &std::path::Path) {
    fs::write(
        dir.join("forge.toml"),
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
config_dir = "~/.claude-subspace"
"#,
    )
    .expect("write forge.toml");
}

#[tokio::test]
async fn new_loads_config_and_lists_projects() {
    let dir = tempdir().expect("tempdir");
    write_default_config(dir.path());

    let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
    let projects = workspace.list_projects();
    assert_eq!(projects.len(), 1, "one [[orgs.projects]] entry should yield one ProjectView");
    let project = &projects[0];
    // The `display_path` retains the `~` form from forge.toml.
    assert_eq!(project.display_path, "~/Projects/forge");
}

#[tokio::test]
async fn new_returns_err_when_config_missing() {
    let dir = tempdir().expect("tempdir");
    let result = Workspace::new(dir.path().to_owned()).await;
    assert!(result.is_err(), "missing forge.toml should error");
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

    let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
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
