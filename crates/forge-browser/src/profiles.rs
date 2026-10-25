//! The server's profile store: one directory per org profile.
//!
//! **The key is two directory levels** - `profiles/<org>/<name>` - so two
//! orgs can both have `work` without sharing a byte of cache. The legacy
//! trees are deleted once on the first boot that finds no marker: the
//! server's own pre-org `shared/` and per-seat `profiles/<name>/cef/`, and
//! the client's old caches under its bundle identifier (`browser/cef-profile`
//! and `browser/profiles/<name>/cef`). **Those caches hold the client's
//! logins, and they are not carried over**: an org profile starts empty and
//! each org signs in again, which is what one login store per org means. A
//! marker beside the root keeps a later boot from touching anything.

use std::path::{Path, PathBuf};

use forge_primitives::browser::ProfileKey;

/// The marker file, inside the server's browser root, that says the legacy
/// deletion ran.
const MARKER: &str = "legacy-profiles-deleted";

/// The client's bundle identifier: its old in-process CEF kept its cache
/// trees under this app-support directory.
const CLIENT_BUNDLE: &str = "dev.vedhavyas.forge";

/// The server's profile tree, under forge's app-support `browser/` root.
pub struct Store {
    root: PathBuf,
}

/// What one boot's legacy deletion removed: **the paths are named**, so the
/// one INFO line can say what went rather than only how many.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Legacy {
    pub removed: Vec<PathBuf>,
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

    /// The client's own browser tree on this machine, when there is one: the
    /// app-support directory its bundle identifier owned. **The one path the
    /// server derives outside its own app-support**, and only to delete the
    /// client's dead caches.
    pub fn client_root() -> Option<PathBuf> {
        let support = forge_sdk::app_support_dir().ok()?.parent()?.to_owned();
        Some(client_root_at(&support))
    }

    /// Delete the legacy trees once: the server's own pre-org store
    /// (`shared/` and `profiles/<name>/cef/`) and the client's old CEF
    /// caches (`browser/cef-profile` and `browser/profiles/<name>/cef`).
    /// **The client's logins go with them and are entered again per org**;
    /// a copy would preserve the single-profile tree the org model replaces.
    /// **A stray file is not a profile**; the new org directories hold their
    /// `cef/` one level deeper and are never touched. Guarded by the marker.
    pub fn delete_legacy(&self) -> Result<Legacy, String> {
        self.delete_legacy_at(Self::client_root().as_deref())
    }

    /// The deletion with the client tree given, so a test never touches the
    /// machine's real client data.
    pub fn delete_legacy_at(&self, client: Option<&Path>) -> Result<Legacy, String> {
        let marker = self.root.join(MARKER);
        if marker.exists() {
            return Ok(Legacy { removed: Vec::new(), ran: false });
        }
        let mut removed = Vec::new();
        let shared = self.root.join("shared");
        if shared.is_dir() {
            remove_tree(&shared)?;
            removed.push(shared);
        }
        removed.extend(remove_named_profiles(&self.profiles())?);
        if let Some(client) = client.filter(|client| client.is_dir()) {
            // **The client's shared cache is `cef-profile`**, not a
            // directory named `shared`: the old in-process host kept it
            // beside `profiles/`, not inside it.
            let shared = client.join("cef-profile");
            if shared.is_dir() {
                remove_tree(&shared)?;
                removed.push(shared);
            }
            removed.extend(remove_named_profiles(&client.join("profiles"))?);
        }
        std::fs::create_dir_all(&self.root)
            .map_err(|why| format!("the browser root cannot be made: {why}"))?;
        std::fs::write(&marker, b"1")
            .map_err(|why| format!("the marker cannot be written: {why}"))?;
        Ok(Legacy { removed, ran: true })
    }
}

/// Delete every direct child that holds a `cef/` cache, answering the paths
/// it removed. A missing directory is nothing to delete; an entry that cannot
/// be read fails before the marker is written.
fn remove_named_profiles(profiles: &Path) -> Result<Vec<PathBuf>, String> {
    let mut removed = Vec::new();
    match std::fs::read_dir(profiles) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.map_err(|why| format!("the profiles cannot be read: {why}"))?;
                let path = entry.path();
                if path.join("cef").is_dir() && path.is_dir() {
                    remove_tree(&path)?;
                    removed.push(path);
                }
            }
        }
        Err(why) if why.kind() == std::io::ErrorKind::NotFound => {}
        Err(why) => return Err(format!("the profiles cannot be read: {why}")),
    }
    Ok(removed)
}

fn remove_tree(path: &Path) -> Result<(), String> {
    std::fs::remove_dir_all(path)
        .map_err(|why| format!("{} could not be deleted: {why}", path.display()))
}

/// The client's browser tree under an app-support root. **`dev.vedhavyas.forge`
/// is the client's bundle identifier**, and `browser` is the in-process CEF
/// cache it kept there before the server owned the browser.
fn client_root_at(support: &Path) -> PathBuf {
    support.join(CLIENT_BUNDLE).join("browser")
}

#[cfg(test)]
mod tests {
    use super::Store;
    use forge_primitives::browser::ProfileKey;

    /// **The one path derived outside the server's own app-support** is the
    /// client's bundle directory plus its `browser` tree.
    #[test]
    fn the_client_root_is_the_clients_bundle_browser_tree() {
        assert_eq!(
            super::client_root_at(std::path::Path::new("/support")),
            std::path::Path::new("/support/dev.vedhavyas.forge/browser"),
        );
    }

    #[test]
    fn an_org_profile_is_one_directory_deeper() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = Store::at(dir.path());
        let key = ProfileKey::of("Busytools", "work").expect("a key");
        assert_eq!(store.org_profile(&key), dir.path().join("profiles/Busytools/work"));
    }

    #[test]
    fn the_legacy_server_and_client_trees_go_once() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let client = dir.path().join("client/browser");
        std::fs::create_dir_all(dir.path().join("shared/cef")).expect("the shared tree");
        std::fs::create_dir_all(dir.path().join("profiles/lead/cef")).expect("a named tree");
        std::fs::write(dir.path().join("profiles/owner.json"), b"{}").expect("a stray file");
        std::fs::create_dir_all(dir.path().join("profiles/Busytools/work/cef"))
            .expect("an org tree");
        // **The real client layout**: its shared cache is `cef-profile`,
        // beside `profiles/`, not a directory named `shared`.
        std::fs::create_dir_all(client.join("cef-profile")).expect("the client's shared cache");
        std::fs::create_dir_all(client.join("profiles/lead/cef")).expect("a client profile");
        std::fs::create_dir_all(client.join("profiles/notes")).expect("a client dir, no cache");

        let store = Store::at(dir.path());
        let first = store.delete_legacy_at(Some(&client)).expect("the first boot deletes");
        assert_eq!(first.removed.len(), 4, "the server's two legacy trees and the client's two go");
        assert!(!dir.path().join("shared").exists());
        assert!(!dir.path().join("profiles/lead").exists());
        assert!(dir.path().join("profiles/Busytools/work/cef").exists(), "an org tree stays");
        assert!(dir.path().join("profiles/owner.json").exists(), "a stray file is not a profile");
        assert!(!client.join("cef-profile").exists(), "the client's shared cache goes");
        assert!(!client.join("profiles/lead").exists(), "its per-profile cache goes");
        assert!(client.join("profiles/notes").exists(), "a directory with no cef cache stays");

        std::fs::create_dir_all(client.join("cef-profile")).expect("a client cache returns");
        let second = store.delete_legacy_at(Some(&client)).expect("the second boot is a no-op");
        assert_eq!(second.removed.len(), 0, "the deletion never runs twice");
        assert!(client.join("cef-profile").exists());
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
            store.delete_legacy_at(None).is_err(),
            "a legacy tree that cannot be removed fails the boot"
        );
        assert!(
            !dir.path().join("legacy-profiles-deleted").exists(),
            "the marker is not written when the deletion failed"
        );

        std::fs::set_permissions(&profiles, std::fs::Permissions::from_mode(0o700))
            .expect("profiles writable again");
    }

    /// **A tree that cannot even be LISTED fails the boot before the
    /// marker.** The deletion reads every entry to decide what is legacy; a
    /// read that fails must not leave a marker claiming the work is done.
    #[cfg(unix)]
    #[test]
    fn a_legacy_tree_that_cannot_be_listed_fails_before_the_marker() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("a temp dir");
        let profiles = dir.path().join("profiles");
        std::fs::create_dir_all(&profiles).expect("the profiles dir");
        std::fs::set_permissions(&profiles, std::fs::Permissions::from_mode(0o000))
            .expect("profiles closed");

        // A root user ignores the mode bits, so there is no refusal to see.
        let probe = profiles.join("permission-probe");
        if std::fs::write(&probe, b"").is_ok() {
            std::fs::remove_file(&probe).expect("the probe goes");
            std::fs::set_permissions(&profiles, std::fs::Permissions::from_mode(0o700))
                .expect("profiles readable again");
            println!("skipping: the mode bits are not enforced (running as root?)");
            return;
        }

        let store = Store::at(dir.path());
        assert!(
            store.delete_legacy_at(None).is_err(),
            "a tree that cannot be listed fails the boot"
        );
        assert!(
            !dir.path().join("legacy-profiles-deleted").exists(),
            "the marker is not written when the listing failed"
        );

        std::fs::set_permissions(&profiles, std::fs::Permissions::from_mode(0o700))
            .expect("profiles readable again");
    }
}
