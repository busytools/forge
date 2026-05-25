//! Live-capture scenario: spawn `claude` with `--worktree <name>` and
//! capture the init envelope's `worktree: {...}` field.
//!
//! The CLI's `--worktree <name>` flag tells `claude` to fork a fresh
//! git worktree at `<repo>/.claude/worktrees/<name>/` on branch
//! `worktree-<name>`, run the session inside it, and stamp a
//! `worktree: {name, path, branch, original_cwd, original_branch}`
//! object onto the init / init-replace messages so the SDK knows
//! which worktree context it's in.
//!
//! This baseline locks down that wire shape. The forge-sdk decoder
//! (Task 3.2 of the worktrees-v1 plan) will replay against the
//! captured JSONL and verify the `WorktreeInfo` field deserialises
//! cleanly without `DecodedLine::Unknown`.
//!
//! Pre-session setup happens in the test body before
//! `run_live_scenario` is called: a tempdir is initialised as a
//! git repo with one seed commit on `main`, then handed to the
//! subprocess via `OptionsBuilder::cwd(...)`. The `claude` binary
//! then creates the worktree subdirectory on its own.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::process::Command;

use forge_sdk::OptionsBuilder;
use forge_test_harness::sdk_wire::run_live_scenario;
use tempfile::TempDir;

/// Initialise `dir` as a git repo on branch `main` with one seed
/// commit. `claude --worktree` refuses to fork off a repo with no
/// commits, so the seed is non-optional.
fn init_seed_repo(dir: &std::path::Path) {
    let run = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap_or_else(|e| panic!("git {args:?}: spawn failed: {e}"));
        assert!(status.success(), "git {args:?}: exit {status:?}");
    };
    run(&["init", "--initial-branch=main"]);
    run(&["config", "user.email", "harness@forge.test"]);
    run(&["config", "user.name", "forge-harness"]);
    std::fs::write(dir.join("seed.txt"), "seed\n").expect("write seed.txt");
    run(&["add", "seed.txt"]);
    run(&["commit", "-m", "seed"]);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn worktree_spawn_scenario() {
    // Pre-session setup: real git repo in a fresh tempdir. The CLI
    // refuses to spawn `--worktree` from a non-repo cwd.
    let repo = TempDir::new().expect("tempdir");
    init_seed_repo(repo.path());

    let opts = OptionsBuilder::new()
        .max_turns(1)
        .cwd(repo.path())
        .extra_arg("worktree", Some("harness_worktree_spawn".to_string()))
        .build();

    run_live_scenario("worktree_spawn", opts, |client, events| async move {
        client.send_user_message("Respond with just the word OK. Do not call any tools.").await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");

    // Keep `repo` alive until after the scenario drained so the
    // worktree the CLI created under `.claude/worktrees/` doesn't
    // get yanked mid-run.
    drop(repo);
}
