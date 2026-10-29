//! Live-capture scenarios: the CLI's own repository-state frames.
//!
//! `system/vcs_state_changed` fires when a foreground Bash command mutates
//! repository state - a commit, push, merge or rebase - and carries the
//! mutation's kind, the branch it landed on and the directory the shell
//! finished in. `system/code_change_published` fires when the session
//! publishes a change for review or links itself to an existing one, and
//! carries the provider, URL, repository, identifier and the action that
//! produced it.
//!
//! Both reached the wire unmodelled, so each scenario records the payload
//! the CLI actually sends. Detection reads the command text and the tool
//! result, so a scratch repo and a `gh` shim stand in for a real forge:
//! neither frame needs GitHub to fire.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::Path;
use std::process::Command;

use forge_sdk::{OptionsBuilder, PermissionMode};
use forge_test_harness::sdk_wire::run_live_scenario;
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap_or_else(|e| panic!("git {args:?}: spawn failed: {e}"));
    assert!(status.success(), "git {args:?}: exit {status:?}");
}

/// A repo on `main` with one seed commit, pushed to a bare remote inside
/// the same tempdir. The seed is non-optional: the push detection needs a
/// real ref update, and the commit detection needs an existing branch.
fn init_repo(scratch: &Path) -> std::path::PathBuf {
    let remote = scratch.join("remote.git");
    git(scratch, &["init", "--bare", remote.to_str().expect("utf-8 path")]);

    let repo = scratch.join("repo");
    git(scratch, &["init", "--initial-branch=main", repo.to_str().expect("utf-8 path")]);
    git(&repo, &["config", "user.email", "harness@forge.test"]);
    git(&repo, &["config", "user.name", "forge-harness"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    git(&repo, &["remote", "add", "origin", remote.to_str().expect("utf-8 path")]);
    git(&repo, &["commit", "--allow-empty", "-m", "seed"]);
    git(&repo, &["push", "-u", "origin", "main"]);
    repo
}

/// A `gh` that answers `pr create` with a PR-shaped URL and nothing else.
/// `code_change_published` is derived from the command text plus the
/// printed URL, so this exercises the CLI's own detection without
/// touching a real forge.
fn install_gh_shim(scratch: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let bin = scratch.join("bin");
    std::fs::create_dir(&bin).expect("create shim dir");
    let shim = bin.join("gh");
    std::fs::write(
        &shim,
        "#!/bin/sh\n\
         if [ \"$1 $2\" = \"pr create\" ]; then\n\
         \x20 echo 'https://github.com/example/example/pull/123'\n\
         \x20 exit 0\n\
         fi\n\
         exit 1\n",
    )
    .expect("write gh shim");
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).expect("chmod gh shim");
    bin
}

/// Records every kind the frame declares: commit, push, merge and rebase.
///
/// The frame is emitted only for foreground Bash calls: a backgrounded
/// command whose output had not landed at capture time emits nothing. One
/// command can carry several mutations, which is why the three calls below
/// are compounds rather than one call per git verb.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_vcs_state_changed() {
    let scratch = TempDir::new().expect("tempdir");
    let repo = init_repo(scratch.path());

    let opts = OptionsBuilder::new()
        .cwd(&repo)
        .max_turns(8)
        .permission_mode(PermissionMode::AcceptEdits)
        .allowed_tools(vec!["Bash".to_string()])
        .build();

    run_live_scenario("vcs_state_changed", opts, |client, events| async move {
        client
            .send_user_message(
                "Use the Bash tool for exactly three tool calls, one per step, in this order. \
                 Do not run any other command.\n\
                 Step 1: run `git commit --allow-empty -m 'capture probe' && git push origin main`.\n\
                 Step 2: run `git checkout -b topic && git commit --allow-empty -m 'topic work' \
                 && git checkout main && git merge --no-edit topic`.\n\
                 Step 3: run `git checkout -b topic2 && git commit --allow-empty -m 'topic2 work' \
                 && git checkout main && git commit --allow-empty -m 'main moves' \
                 && git checkout topic2 && git rebase main`.\n\
                 Then reply with the single word DONE.",
            )
            .await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}

/// Records `action: created` from a `gh pr create`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_code_change_published() {
    let scratch = TempDir::new().expect("tempdir");
    let repo = init_repo(scratch.path());
    let bin = install_gh_shim(scratch.path());
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());

    let opts = OptionsBuilder::new()
        .cwd(&repo)
        .max_turns(4)
        .permission_mode(PermissionMode::AcceptEdits)
        .allowed_tools(vec!["Bash".to_string()])
        .env("PATH", path)
        .build();

    run_live_scenario("code_change_published", opts, |client, events| async move {
        client
            .send_user_message(
                "Run exactly one Bash command: \
                 `gh pr create --title 'capture probe' --body 'probe'`. \
                 Then reply with the URL it printed.",
            )
            .await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}
