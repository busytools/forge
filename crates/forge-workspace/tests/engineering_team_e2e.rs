//! End-to-end: forge.toml with `team = [...]` parses, project lead
//! Connected programmatically dispatches one `Command::SpawnWorker`
//! per configured role. Exercises Tasks 1-4 in a single integration
//! pass: Role enum + charters (T1), forge.toml `team` parsing (T2),
//! lead charter stamping (T3, indirectly via the same code path), and
//! programmatic Connected -> spawn dispatch (T4).

#![cfg(feature = "testing")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use forge_workspace::protocol::Command;
use forge_workspace::{SessionKey, Workspace, on_connected_for_test, team::Role};
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

#[tokio::test]
async fn config_load_through_team_dispatch() {
    let tmp = tempdir().expect("tempdir");
    write_team_config(tmp.path());

    // Boot a real Workspace from the on-disk forge.toml — this drives
    // the full parse path through `LoadedProject::team` (Task 2).
    let workspace = Arc::new(Workspace::new(tmp.path().to_owned()).await.expect("workspace boot"));

    // Verify the project loaded and carries the team list in the
    // public `ProjectView`.
    let projects = workspace.list_projects();
    let demo =
        projects.iter().find(|p| p.name == "demo").expect("demo project visible via list_projects");
    assert_eq!(
        demo.team,
        vec![Role::Planner, Role::Implementer, Role::Reviewer],
        "team list parsed in declaration order"
    );

    // Arm the dispatch intercept so the SpawnWorker commands the
    // Connected hook would route through `Workspace::dispatch` get
    // buffered instead.
    workspace.enable_test_dispatch_intercept();

    // Synthesize the Connected hook directly. The synth-key format
    // is `__spawn_<project_name>__` — same shape the production
    // spawn path uses for the project-lead session before the real
    // claude-issued session_id arrives.
    let synth_key = SessionKey::from_session_id("__spawn_demo__");
    on_connected_for_test(&workspace, &synth_key, "lead-uuid");

    // Team-spawn under a tokio runtime goes through
    // `spawn_team_for_lead_with_catalog_scan` which dispatches the
    // SpawnWorker commands from a tokio::spawn after an async catalog
    // scan. Poll the dispatch buffer briefly to let that async task
    // land before draining. The catalog scan is empty (tempdir has
    // no JSONLs) so the wait should be sub-100ms in practice; cap at
    // 5s for CI margin.
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
            Command::SpawnWorker { label, spawned_by_session_id, .. } => {
                assert_eq!(
                    spawned_by_session_id, "lead-uuid",
                    "every SpawnWorker is parented to the lead's real session_id"
                );
                Some(label.clone())
            }
            _ => None,
        })
        .collect();

    assert_eq!(labels.len(), 3, "one SpawnWorker per configured role");
    assert_eq!(
        labels,
        vec!["planner".to_owned(), "implementer".to_owned(), "reviewer".to_owned()],
        "dispatch order matches team = [...] order in forge.toml"
    );
}
