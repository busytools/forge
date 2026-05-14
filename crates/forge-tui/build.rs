//! Stamp the build with the current git commit + branch when this is
//! a development build (i.e. branch != `main`). Release builds on
//! `main` get empty suffixes so the welcome banner shows just
//! `0.15.1`; dev builds get `0.15.1+sha` (short form, for the
//! Projects pane bottom row) and `0.15.1 · sha (branch)` (full form,
//! for the welcome banner + status panel).
//!
//! Tolerant of missing `.git` (e.g. `cargo install` from crates.io)
//! and any other git failure — emits empty suffixes in that case.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // Rerun when HEAD moves. `.git/HEAD` only updates on branch
    // switch (its content is `ref: refs/heads/<branch>`, which
    // doesn't change when you commit on the same branch). To pick
    // up the commit-on-same-branch case we additionally watch the
    // refs + logs directories — both get content writes on
    // `git commit` / `checkout` / `reset` / etc., and cargo's
    // rerun-if-changed on a directory walks the tree.
    //
    // Without this, building forge-tui after `git commit` on a
    // feature branch keeps re-using the previous build's
    // FORGE_BUILD_SUFFIX_SHORT — the embedded version stamp goes
    // stale and the binary reports an older commit hash than it
    // was built from.
    if std::path::Path::new("../../.git/HEAD").exists() {
        println!("cargo:rerun-if-changed=../../.git/HEAD");
    }
    if std::path::Path::new("../../.git/refs/heads").exists() {
        println!("cargo:rerun-if-changed=../../.git/refs/heads");
    }
    if std::path::Path::new("../../.git/logs/HEAD").exists() {
        println!("cargo:rerun-if-changed=../../.git/logs/HEAD");
    }

    let (short, full) = compute_suffixes();
    println!("cargo:rustc-env=FORGE_BUILD_SUFFIX_SHORT={short}");
    println!("cargo:rustc-env=FORGE_BUILD_SUFFIX_FULL={full}");
}

fn compute_suffixes() -> (String, String) {
    let Some(branch) = run_git(&["rev-parse", "--abbrev-ref", "HEAD"]) else {
        return (String::new(), String::new());
    };
    if branch == "main" {
        // Release-style build on `main`. No suffix.
        return (String::new(), String::new());
    }
    let Some(sha) = run_git(&["rev-parse", "--short", "HEAD"]) else {
        return (String::new(), String::new());
    };
    let short = format!("+{sha}");
    let full = if branch == "HEAD" {
        // Detached HEAD (e.g. checked out a tag) — branch name is the
        // sentinel string `"HEAD"`. Drop the parenthetical.
        format!(" · {sha}")
    } else {
        format!(" · {sha} ({branch})")
    };
    (short, full)
}

fn run_git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}
