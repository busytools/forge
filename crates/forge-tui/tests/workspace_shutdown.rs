//! Integration test for the workspace shutdown path used at TUI exit.
//!
//! The TUI's `main.rs` holds a `Rc<Workspace>` outside the App, hands
//! a clone to `App` via `create_app`, and after the event loop
//! returns drops the App + reclaims ownership via `Rc::try_unwrap`
//! before calling `Workspace::shutdown().await`. This test pins down
//! that ownership-reclaim sequence — the actual `claude` subprocess
//! is never spawned because no Agent is acquired from the pool.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::rc::Rc;

use forge_workspace::Workspace;
use tempfile::tempdir;

#[tokio::test(flavor = "current_thread")]
async fn workspace_shutdown_drains_after_app_drop() {
    let dir = tempdir().expect("tempdir");
    fs::write(
        dir.path().join("forge.toml"),
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

    let workspace = Workspace::new(dir.path().to_owned()).await.expect("workspace");
    let workspace = Rc::new(workspace);

    // Simulate the App holding an Rc clone, then being dropped when
    // the event loop returns. After this drop, only `main`'s original
    // Rc remains.
    let app_clone = Rc::clone(&workspace);
    drop(app_clone);

    // Reclaim ownership of the workspace and shut it down. The
    // try_unwrap must succeed because we just dropped the only other
    // Rc clone.
    let workspace =
        Rc::try_unwrap(workspace).map_err(|_| ()).expect("Rc should be unique after App drops");
    workspace.shutdown().await;
}
