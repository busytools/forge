//! The server's profile store: one directory per org profile.
//!
//! **The key is two directory levels** - `profiles/<org>/<name>` - so two
//! orgs can both have `work` without sharing a byte of cache. The legacy
//! `shared/` and per-seat `profiles/<name>/` trees are deleted once on the
//! first boot that finds no marker; a marker beside the root keeps a later
//! boot from touching anything.

use std::path::{Path, PathBuf};

use forge_primitives::browser::ProfileKey;

/// The marker file, inside the server's browser root, that says the legacy
/// deletion ran.
const MARKER: &str = "legacy-profiles-deleted";

/// The server's profile tree, under forge's app-support `browser/` root.
pub struct Store {
    root: PathBuf,
}

/// What one boot's legacy deletion removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Legacy {
    pub removed: usize,
    pub ran: bool,
}

impl Store {
    pub fn at(root: &Path) -> Self {
        Self { root: root.to_owned() }
    }

    /// The tree every org profile hangs off.
    pub fn profiles(&self) -> PathBuf {
        self.root.join("profiles")
    }

    /// One org profile's directory: its `cef` cache and its `uploads`.
    pub fn org_profile(&self, key: &ProfileKey) -> PathBuf {
        self.profiles().join(key.org()).join(key.name())
    }

    /// Delete the old `shared/` tree and every direct child of `profiles/`
    /// that holds its own `cef/`. **A stray file is not a profile**; the new
    /// org directories hold their `cef/` one level deeper and are never
    /// touched. Runs once, guarded by the marker.
    pub fn delete_legacy(&self) -> Result<Legacy, String> {
        let marker = self.root.join(MARKER);
        if marker.exists() {
            return Ok(Legacy { removed: 0, ran: false });
        }
        let mut removed = 0;
        let shared = self.root.join("shared");
        if shared.is_dir() {
            remove_tree(&shared)?;
            removed += 1;
        }
        match std::fs::read_dir(self.profiles()) {
            Ok(entries) => {
                for entry in entries {
                    let entry =
                        entry.map_err(|why| format!("the profiles cannot be read: {why}"))?;
                    let path = entry.path();
                    if path.join("cef").is_dir() && path.is_dir() {
                        remove_tree(&path)?;
                        removed += 1;
                    }
                }
            }
            Err(why) if why.kind() == std::io::ErrorKind::NotFound => {}
            Err(why) => return Err(format!("the profiles cannot be read: {why}")),
        }
        std::fs::create_dir_all(&self.root)
            .map_err(|why| format!("the browser root cannot be made: {why}"))?;
        std::fs::write(&marker, b"1")
            .map_err(|why| format!("the marker cannot be written: {why}"))?;
        Ok(Legacy { removed, ran: true })
    }
}

fn remove_tree(path: &Path) -> Result<(), String> {
    std::fs::remove_dir_all(path)
        .map_err(|why| format!("{} could not be deleted: {why}", path.display()))
}

/// Copy one directory tree, files and permissions included. **The live A/B
/// test is its only caller**, so it exists only in a test build.
#[cfg(test)]
pub(crate) fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|why| format!("{} cannot be made: {why}", to.display()))?;
    for (entry, kind) in entries(from)? {
        copy_entry(&entry, kind, &to.join(entry.file_name()))?;
    }
    Ok(())
}

/// One directory's entries with their kinds, read as a pair so a caller
/// cannot forget to check the type before touching the path.
#[cfg(test)]
fn entries(from: &Path) -> Result<Vec<(std::fs::DirEntry, std::fs::FileType)>, String> {
    let entries = std::fs::read_dir(from)
        .map_err(|why| format!("{} cannot be read: {why}", from.display()))?;
    let mut read = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|why| format!("{} cannot be read: {why}", from.display()))?;
        let kind = entry
            .file_type()
            .map_err(|why| format!("{} cannot be read: {why}", entry.path().display()))?;
        read.push((entry, kind));
    }
    Ok(read)
}

#[cfg(test)]
fn copy_entry(
    entry: &std::fs::DirEntry,
    kind: std::fs::FileType,
    target: &Path,
) -> Result<(), String> {
    if kind.is_dir() {
        copy_tree(&entry.path(), target)
    } else if kind.is_symlink() {
        let link = std::fs::read_link(entry.path())
            .map_err(|why| format!("{} cannot be read: {why}", entry.path().display()))?;
        std::os::unix::fs::symlink(&link, target)
            .map_err(|why| format!("{} cannot be copied: {why}", entry.path().display()))
    } else {
        std::fs::copy(entry.path(), target)
            .map_err(|why| format!("{} cannot be copied: {why}", entry.path().display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Store;
    use forge_primitives::browser::ProfileKey;

    #[test]
    fn an_org_profile_is_one_directory_deeper() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = Store::at(dir.path());
        let key = ProfileKey::of("Busytools", "work").expect("a key");
        assert_eq!(store.org_profile(&key), dir.path().join("profiles/Busytools/work"));
    }

    #[test]
    fn the_legacy_shared_and_named_profiles_go_once() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::create_dir_all(dir.path().join("shared/cef")).expect("the shared tree");
        std::fs::create_dir_all(dir.path().join("profiles/lead/cef")).expect("a named tree");
        std::fs::write(dir.path().join("profiles/owner.json"), b"{}").expect("a stray file");
        std::fs::create_dir_all(dir.path().join("profiles/Busytools/work/cef"))
            .expect("an org tree");

        let store = Store::at(dir.path());
        let first = store.delete_legacy().expect("the first boot deletes");
        assert_eq!(first.removed, 2, "the shared tree and the per-seat profile go");
        assert!(!dir.path().join("shared").exists());
        assert!(!dir.path().join("profiles/lead").exists());
        assert!(dir.path().join("profiles/Busytools/work/cef").exists(), "an org tree stays");
        assert!(dir.path().join("profiles/owner.json").exists(), "a stray file is not a profile");

        std::fs::create_dir_all(dir.path().join("shared/cef")).expect("a shared tree returns");
        let second = store.delete_legacy().expect("the second boot is a no-op");
        assert_eq!(second.removed, 0, "the deletion never runs twice");
        assert!(dir.path().join("shared").exists());
    }

    /// A deletion that cannot finish must not leave the marker behind, so
    /// the next boot retries the whole thing.
    #[cfg(unix)]
    #[test]
    fn a_legacy_tree_that_cannot_be_removed_fails_the_boot_before_the_marker() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("a temp dir");
        let profiles = dir.path().join("profiles");
        std::fs::create_dir_all(profiles.join("lead/cef")).expect("a legacy tree");
        std::fs::set_permissions(&profiles, std::fs::Permissions::from_mode(0o500))
            .expect("profiles made read-only");

        // A root user ignores the mode bits, so there is no refusal to see.
        let probe = profiles.join("permission-probe");
        if std::fs::write(&probe, b"").is_ok() {
            std::fs::remove_file(&probe).expect("the probe goes");
            std::fs::set_permissions(&profiles, std::fs::Permissions::from_mode(0o700))
                .expect("profiles writable again");
            println!("skipping: the mode bits are not enforced (running as root?)");
            return;
        }

        let store = Store::at(dir.path());
        assert!(
            store.delete_legacy().is_err(),
            "a legacy tree that cannot be removed fails the boot"
        );
        assert!(
            !dir.path().join("legacy-profiles-deleted").exists(),
            "the marker is not written when the deletion failed"
        );

        std::fs::set_permissions(&profiles, std::fs::Permissions::from_mode(0o700))
            .expect("profiles writable again");
    }
}
