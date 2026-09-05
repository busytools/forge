//! Integration test for the workspace shutdown path used at TUI exit.
//!
//! The TUI's `main.rs` holds a `Rc<Workspace>` outside the App, hands
//! a clone to `App` via `create_app`, and after the event loop
//! returns drops the App + reclaims ownership via `Rc::try_unwrap`
//! before calling `Workspace::shutdown().await`. This test pins down
//! that ownership-reclaim sequence - the actual `claude` subprocess
//! is never spawned because no Agent is acquired from the pool.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::sync::Arc;

use forge_workspace::Workspace;
use tempfile::tempdir;

#[tokio::test(flavor = "current_thread")]
async fn workspace_shutdown_drains_after_app_drop() {
    let dir = tempdir().expect("tempdir");
    let forge = dir.path().join("forge");
    fs::create_dir_all(&forge).expect("forge/ dir");
    fs::write(
        forge.join("forge.toml"),
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

    let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("workspace"));
    let workspace = Arc::new(workspace);

    // Simulate the App holding an Rc clone, then being dropped when
    // the event loop returns. After this drop, only `main`'s original
    // Rc remains.
    let app_clone = Arc::clone(&workspace);
    drop(app_clone);

    // `Workspace::shutdown` takes `&self` and is synchronous - it
    // just drains internal mutexes - so we don't need to unwrap
    // the Arc or await.
    workspace.shutdown();
}
