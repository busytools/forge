//! Stamp the build with the current git commit + branch.
//!
//! Emitted unconditionally - every binary surfaces its short SHA so a
//! screenshot is enough to identify the running commit. Short form
//! (Projects pane bottom row, launchpad version line) is
//! `0.15.1+<sha>`; full form (welcome banner, status panel) is
//! `0.15.1 · <sha>` on `main` and `0.15.1 · <sha> (<branch>)` off
//! `main`. Detached HEAD elides the branch parenthetical.
//!
//! Tolerant of missing `.git` (e.g. `cargo install` from crates.io)
//! and any other git failure - emits empty suffixes in that case.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // Rerun when HEAD moves. `.git/HEAD` only updates on branch
    // switch (its content is `ref: refs/heads/<branch>`, which
    // doesn't change when you commit on the same branch). To pick
    // up the commit-on-same-branch case we additionally watch the
    // refs + logs directories - both get content writes on
    // `git commit` / `checkout` / `reset` / etc., and cargo's
    // rerun-if-changed on a directory walks the tree.
    //
    // Without this, building forge-tui after `git commit` on a
    // feature branch keeps re-using the previous build's
    // FORGE_BUILD_SUFFIX_SHORT - the embedded version stamp goes
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
    let Some(sha) = run_git(&["rev-parse", "--short", "HEAD"]) else {
        return (String::new(), String::new());
    };
    let short = format!("+{sha}");
    let full = match branch.as_str() {
        // On `main` (release-style) the branch parenthetical is
        // redundant; the bare sha is enough to identify the build.
        // Detached HEAD (e.g. tag checkout) reports the literal
        // sentinel `"HEAD"` here - same treatment.
        "main" | "HEAD" => format!(" · {sha}"),
        _ => format!(" · {sha} ({branch})"),
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
