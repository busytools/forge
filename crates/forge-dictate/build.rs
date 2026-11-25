//! Stages transcribe's shared libraries where `scripts/install.sh` finds them.

use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The staging code the crate's tests exercise too, compiled here with the
/// same source file rather than a copy of it.
mod staging {
    include!("src/staging.rs");
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // The staging itself lives beside the crate so the tests can compile the
    // same file; a change to it must rerun this build.
    println!("cargo:rerun-if-changed=src/staging.rs");
    // Present only when transcribe built its ggml shared, which is the posture
    // forge's binary needs; a static build has nothing to ship.
    println!("cargo:rerun-if-env-changed=DEP_TRANSCRIBE_CPP_RUNTIME_DIR");
    let Some(runtime_dir) = env::var_os("DEP_TRANSCRIBE_CPP_RUNTIME_DIR") else {
        // forge links the shared posture; absence means the sys crate changed
        // its metadata or a static transcribe was forced.
        println!(
            "cargo:warning=transcribe-cpp reported no runtime library directory; nothing is staged for the install"
        );
        return;
    };
    let Some(profile_dir) = profile_dir() else {
        println!(
            "cargo:warning=the target profile directory was not found; transcribe's libraries are not staged for the install"
        );
        return;
    };
    let dest = profile_dir.join("transcribe-libs");
    // Warn rather than fail: a plain `cargo build` is not an install, and
    // `scripts/install.sh` refuses an install whose binary cannot start.
    if let Err(err) = staging::stage(Path::new(&runtime_dir), &dest) {
        println!(
            "cargo:warning=staging transcribe's shared libraries into {} failed: {err}",
            dest.display()
        );
    }
}

/// `<target>/[<triple>/]<profile>`, matched by name: cargo's `OUT_DIR` nesting
/// has changed across releases, so a fixed ancestor count is not stable.
fn profile_dir() -> Option<PathBuf> {
    let profile = env::var("PROFILE").ok()?;
    let out_dir = env::var_os("OUT_DIR")?;
    PathBuf::from(out_dir)
        .ancestors()
        .find(|dir| dir.file_name().and_then(OsStr::to_str) == Some(profile.as_str()))
        .map(Path::to_path_buf)
}
