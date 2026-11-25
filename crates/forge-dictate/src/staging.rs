// Stage transcribe's shared libraries beside the binary.
//
// This file is compiled twice: `build.rs` includes it, so a build stages
// the libraries where the install finds them, and the crate's tests
// include it, so a test exercises the same code a build runs rather than
// a copy of it. It must stay standalone - std only, no crate items, no
// inner attributes - because the build script and the library have
// different roots.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::Path;

/// Copy the runtime libraries from `runtime_dir` into `dest`, keeping the
/// loader's names (each library arrives as a versioned file, its SONAME
/// and its link name), and drop a library this build no longer produces.
///
/// **Every file is staged beside the target and renamed over it.** The
/// renamed file is a new inode, so a process that mapped the previous
/// copy keeps its own, untouched one. Writing over the existing file
/// instead moves its mtime under that mapping, and macOS then rejects
/// the process's next executable page fault (`cs_invalid_page`,
/// `cs_mtime != mtime`) and kills it. A cargo invocation that restages
/// while a forge binary is running is exactly that, so the stage must
/// never rewrite an inode in place.
pub(crate) fn stage(runtime_dir: &Path, dest: &Path) -> io::Result<()> {
    fs::create_dir_all(dest)?;
    let mut shipped = Vec::new();
    for entry in fs::read_dir(runtime_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if !is_library(&name) {
            continue;
        }
        let target = dest.join(&name);
        // The temp name carries the process id: two cargo invocations can
        // stage into one target directory at the same time, and each must
        // write its own file before either rename lands.
        let incoming = dest.join(temp_name(&name));
        remove_if_present(&incoming)?;
        if let Err(err) = write_library(&entry, &incoming) {
            let _ = fs::remove_file(&incoming);
            return Err(err);
        }
        if let Err(err) = fs::rename(&incoming, &target) {
            let _ = fs::remove_file(&incoming);
            return Err(err);
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

fn temp_name(name: &OsStr) -> OsString {
    let mut temp = OsString::from(".");
    temp.push(name);
    temp.push(format!(".{}.tmp", std::process::id()));
    temp
}

/// One library at `incoming`, by copy or by link.
///
/// The installed tree names each dylib three ways (versioned file, SONAME,
/// link name); keeping the links keeps the loader's name without three
/// copies of every library.
fn write_library(entry: &fs::DirEntry, incoming: &Path) -> io::Result<()> {
    if entry.file_type()?.is_symlink() {
        #[cfg(unix)]
        std::os::unix::fs::symlink(fs::read_link(entry.path())?, incoming)?;
        #[cfg(not(unix))]
        fs::copy(entry.path(), incoming)?;
    } else {
        fs::copy(entry.path(), incoming)?;
    }
    Ok(())
}

pub(crate) fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// The runtime files beside the shared object. The link manifest shares the
/// directory and is not one of them.
pub(crate) fn is_library(name: &OsStr) -> bool {
    let name = name.to_string_lossy();
    name.contains(".so") || name.ends_with(".dylib") || name.ends_with(".dll")
}
