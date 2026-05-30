//! End-to-end: forge.toml with `team = [...]` parses, project lead
//! Connected programmatically dispatches one `Command::SpawnWorker`
//! per configured label, where each label's charter + initial kick is
//! loaded from `~/.claude/forge-team/<label>/{charter,kick}.md`.
//!
//! Exercises the file-driven charters path end-to-end: forge.toml
//! `team = [...]` parsing (validate_label), the per-label
//! load_charter / load_initial_kick disk reads, the lead-charter
//! stamping at `apply_lead_charter_if_team`, and the programmatic
//! Connected -> SpawnWorker dispatch.
//!
//! Uses `set_forge_team_root_for_test` to point the loader at a
//! tempdir so the test doesn't depend on the user's real
//! `~/.claude/forge-team/`.

#![cfg(feature = "testing")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use forge_workspace::protocol::Command;
use forge_workspace::team::set_forge_team_root_for_test;
use forge_workspace::{SessionKey, Workspace, on_connected_for_test};
use std::sync::Arc;
use tempfile::tempdir;

fn write_team_config(dir: &std::path::Path) {
    std::fs::write(
        dir.join("forge.toml"),
        r#"
[[orgs]]
name = "TestOrg"
accounts = ["acct-a"]
[[orgs.projects]]
name = "demo"
path = "/tmp/demo"
team = ["planner", "implementer", "reviewer"]

[[accounts]]
display_name = "acct-a"
config_dir = "/tmp/acct-a"
"#,
    )
    .expect("write forge.toml");
}

/// Populate `<root>/<label>/{charter,kick}.md` for each label so the
/// loader has files to read. Used to seed the tempdir fixture in lieu
/// of the user's real `~/.claude/forge-team/` directory.
fn seed_charter(root: &std::path::Path, label: &str) {
    let dir = root.join(label);
    std::fs::create_dir_all(&dir).expect("create role dir");
    std::fs::write(dir.join("charter.md"), format!("test charter for {label}"))
        .expect("write charter.md");
    std::fs::write(dir.join("kick.md"), format!("test kick for {label}")).expect("write kick.md");
}

#[tokio::test]
async fn config_load_through_team_dispatch() {
    let tmp = tempdir().expect("tempdir");
    write_team_config(tmp.path());

    // Seed the forge-team root with fixture charters for the three
    // labels the forge.toml declares. The test hook redirects
    // `forge_team_root()` to this tempdir for the duration of the
    // test so the production loader paths read these fixtures.
    let team_root = tmp.path().join("forge-team");
    for label in ["planner", "implementer", "reviewer", "lead"] {
        seed_charter(&team_root, label);
    }
    let prior = set_forge_team_root_for_test(Some(team_root.clone()));

    let result = {
        // Boot a real Workspace from the on-disk forge.toml  -  this
        // drives the full parse path through `LoadedProject::team`.
        let workspace =
            Arc::new(Workspace::new(tmp.path().to_owned()).await.expect("workspace boot"));

        // Verify the project loaded and carries the team list in the
        // public `ProjectView` as string labels (post #220 the team
        // field is `Vec<String>` rather than `Vec<Role>`).
        let projects = workspace.list_projects();
        let demo = projects
            .iter()
            .find(|p| p.name == "demo")
            .expect("demo project visible via list_projects");
        assert_eq!(
            demo.team,
            vec!["planner".to_owned(), "implementer".to_owned(), "reviewer".to_owned()],
            "team list parsed in declaration order"
        );

        workspace.enable_test_dispatch_intercept();

        let synth_key = SessionKey::from_session_id("__spawn_demo__");
        on_connected_for_test(&workspace, &synth_key, "lead-uuid");

        // Team-spawn under a tokio runtime goes through
        // `spawn_team_for_lead_with_catalog_scan` which dispatches the
        // SpawnWorker commands from a tokio::spawn after an async
        // catalog scan + the file-driven charter load. Poll the
        // dispatch buffer briefly to let that async task land.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut dispatched: Vec<Command> = Vec::new();
        while std::time::Instant::now() < deadline {
            dispatched = workspace.drain_test_dispatch_buffer();
            if !dispatched.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let labels: Vec<String> = dispatched
            .iter()
            .filter_map(|c| match c {
                Command::SpawnWorker { label, spawned_by_session_id, charter, .. } => {
                    assert_eq!(
                        spawned_by_session_id, "lead-uuid",
                        "every SpawnWorker is parented to the lead's real session_id"
                    );
                    assert!(
                        !charter.trim().is_empty(),
                        "loaded charter must be non-empty: label={label}, charter={charter:?}"
                    );
                    Some(label.clone())
                }
                _ => None,
            })
            .collect();

        assert_eq!(labels.len(), 3, "one SpawnWorker per configured label");
        assert_eq!(
            labels,
            vec!["planner".to_owned(), "implementer".to_owned(), "reviewer".to_owned()],
            "dispatch order matches team = [...] order in forge.toml"
        );
        Ok::<(), &'static str>(())
    };

    // Restore the prior override on teardown so subsequent tests in
    // the same binary don't inherit our tempdir.
    set_forge_team_root_for_test(prior);
    result.expect("e2e succeeds");
}
