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

    // How this binary was built. `scripts/install.sh` exports the
    // marker; anything else - notably a hand-rolled `cargo install`,
    // which ignores Cargo.lock unless `--locked` is passed - leaves it
    // unset and the binary reports itself unguarded at startup.
    println!("cargo:rerun-if-env-changed=FORGE_BUILD_PROVENANCE");
    let provenance = std::env::var("FORGE_BUILD_PROVENANCE").unwrap_or_default();
    println!("cargo:rustc-env=FORGE_BUILD_PROVENANCE={provenance}");

    // Hash of the lockfile this build saw, which is a different signal
    // from provenance: it catches a guarded build that honoured a
    // locally-modified Cargo.lock. Empty when there is no lockfile to
    // read, e.g. a packaged build outside the workspace.
    println!("cargo:rerun-if-changed=../../Cargo.lock");
    println!("cargo:rustc-env=FORGE_LOCKFILE_SHA={}", lockfile_sha());
}

/// Short hash of the workspace lockfile, or empty when it cannot be
/// read. Uses `shasum`/`sha256sum` rather than pulling a hashing crate
/// into the build graph for one line of provenance.
fn lockfile_sha() -> String {
    let Ok(bytes) = std::fs::read("../../Cargo.lock") else {
        return String::new();
    };
    // FNV-1a: a dependency-free digest is enough here, because the
    // only question asked of it is "same lockfile or not".
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
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
