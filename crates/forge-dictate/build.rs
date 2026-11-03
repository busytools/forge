//! Stages transcribe's shared libraries where `scripts/install.sh` finds them.

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
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
    if let Err(err) = stage(Path::new(&runtime_dir), &dest) {
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

fn stage(runtime_dir: &Path, dest: &Path) -> io::Result<()> {
    fs::create_dir_all(dest)?;
    let mut shipped = Vec::new();
    for entry in fs::read_dir(runtime_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if !is_library(&name) {
            continue;
        }
        let target = dest.join(&name);
        if entry.file_type()?.is_symlink() {
            remove_if_present(&target)?;
            // The installed tree names each dylib three ways (versioned file,
            // SONAME, link name); keeping the links keeps the loader's name
            // without three copies of every library.
            #[cfg(unix)]
            std::os::unix::fs::symlink(fs::read_link(entry.path())?, &target)?;
            #[cfg(not(unix))]
            fs::copy(entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
        shipped.push(name);
    }
    // Drop a library this build no longer produces: a stale copy beside the
    // binary would satisfy the loader with the wrong version.
    for entry in fs::read_dir(dest)? {
        let entry = entry?;
        let name = entry.file_name();
        if is_library(&name) && !shipped.contains(&name) {
            remove_if_present(&entry.path())?;
        }
    }
    Ok(())
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// The runtime files beside the shared object. The link manifest shares the
/// directory and is not one of them.
fn is_library(name: &OsStr) -> bool {
    let name = name.to_string_lossy();
    name.contains(".so") || name.ends_with(".dylib") || name.ends_with(".dll")
}
