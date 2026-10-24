//! The server's profile store: where a profile's logins live, who owns the
//! name, and the one-time copy that takes the client's profiles over.
//!
//! **One marker, one copy.** The first boot that finds no marker copies the
//! client's own `browser/` tree across - every named profile and the shared
//! browser's cache - and writes the marker, so a second boot does nothing and
//! no later run ever syncs the two trees. A profile the server already has is
//! never overwritten: the copy fills in what is missing and leaves the rest.
//!
//! **Ownership travels with the directory**, in the `owner.json` the client
//! wrote beside it: the name, the logins and the seat that opened them move
//! together, so a server restart cannot turn an owned name into an open one.

use std::path::{Path, PathBuf};

use forge_primitives::SessionSlot;

/// The client's bundle identifier: the one-time copy's source.
const CLIENT_BUNDLE: &str = "dev.vedhavyas.forge";

/// The marker file, inside the server's browser root, that says the copy ran.
const MARKER: &str = "profiles-migrated";

/// The file inside a profile's directory that records the seat it belongs to.
const OWNER_FILE: &str = "owner.json";

/// The shared browser's directory name: every seat drives it, and no seat
/// owns it.
pub const SHARED: &str = "shared";

/// The name that addresses the browser's own profile where a call, an attach
/// or a raise names it explicitly.
///
/// **It cannot be a profile name**: `@` is outside the profile charset, so
/// the reserved word never collides with a profile a person or a seat made,
/// and the rule that every name is a directory stays true.
pub const SHARED_PROFILE: &str = "@shared";

/// Why a name cannot be a profile, or `None` when it can.
///
/// A profile's name becomes a directory name, so it is held to what is safe
/// there: a name carrying a separator is a name trying to write somewhere
/// else.
pub fn name_refusal(name: &str) -> Option<String> {
    let allowed = !name.is_empty()
        && name.len() <= 64
        && name != "."
        && name != ".."
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    (!allowed).then(|| {
        format!(
            "'{name}' cannot be a profile name: 1 to 64 characters of letters, digits, \
             dot, dash or underscore"
        )
    })
}

/// The name a seat's own profile takes: its project and label folded into
/// the profile charset.
///
/// **The label alone is not unique.** Every project's lead carries `lead`,
/// and two workers in different projects may share a label, so a
/// label-only name would let the first seat to reach the browser hold it
/// against every other seat of that label for good. The project qualifies
/// it.
///
/// **The fold is lossy, and a collision is a refusal, never a share.** The
/// project name may carry a space or reach past the length cap, so every
/// character outside the charset folds to a dash and the result is capped at
/// the name bound: "my app" and "my-app" both derive `my-app-lead`, and two
/// long projects can meet after the cap. The first seat to reach the name
/// holds it; the second seat's unnamed calls are refused with the owner's
/// name - loud, and never the shared browser. A hand-made profile that took
/// the name first is the same refusal.
pub fn auto_name(seat: &SessionSlot) -> String {
    let mut folded = String::with_capacity(seat.project().len() + seat.label().len() + 1);
    for ch in seat.project().chars().chain(std::iter::once('-')).chain(seat.label().chars()) {
        folded.push(if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            ch
        } else {
            '-'
        });
    }
    // The registry's own bound; a name past it is refused there, so the
    // tail is dropped rather than the whole derivation.
    folded.truncate(64);
    folded
}

/// The server's profile tree, under forge's app-support `browser/` root.
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn at(root: &Path) -> Self {
        Self { root: root.to_owned() }
    }

    /// The tree every named profile hangs off.
    pub fn profiles(&self) -> PathBuf {
        self.root.join("profiles")
    }

    /// A named profile's directory: its owner record and its `cef` cache.
    pub fn named(&self, name: &str) -> PathBuf {
        self.profiles().join(name)
    }

    /// The shared browser's directory.
    pub fn shared(&self) -> PathBuf {
        self.root.join(SHARED)
    }

    /// The client's own browser tree on this machine, when there is one to
    /// copy: the app-support directory its bundle identifier owned.
    pub fn client_source() -> Option<PathBuf> {
        let support = forge_sdk::app_support_dir().ok()?.parent()?.to_owned();
        let source = support.join(CLIENT_BUNDLE).join("browser");
        source.is_dir().then_some(source)
    }

    /// Take the client's profiles over, once.
    ///
    /// Answers how many directories came across and how many were already
    /// there. **A missing marker is the only condition**: with one present
    /// nothing is read, so a client tree that reappears later is not imported
    /// a second time. An absent source is not a failure - a machine that
    /// never ran the client has nothing to copy - and still takes the marker,
    /// because ownership has begun either way. **A source that exists but
    /// cannot be read is a failure**: the marker stays absent, the boot says
    /// which profile would not copy, and the next boot takes the rest.
    pub fn migrate(&self, source: Option<&Path>) -> Result<Migrated, String> {
        let marker = self.root.join(MARKER);
        if marker.exists() {
            return Ok(Migrated { ran: false, ..Migrated::default() });
        }
        let mut migrated = Migrated { ran: true, ..Migrated::default() };
        if let Some(source) = source.filter(|source| source.is_dir()) {
            copied(&source.join("profiles"), &self.root.join("profiles"), &mut migrated)?;
            // The client's shared browser is `<data>/browser/cef-profile`;
            // the server's is `<root>/shared/cef`. Copied whole, so the
            // carried page and the cache land together.
            let shared = source.join("cef-profile");
            let target = self.shared().join("cef");
            if shared.is_dir() && !target.exists() {
                copy_tree(&shared, &target)
                    .map_err(|why| format!("the shared browser could not be copied: {why}"))?;
                migrated.copied += 1;
            } else if shared.is_dir() {
                merge_tree(&shared, &target)
                    .map_err(|why| format!("the shared browser could not be completed: {why}"))?;
                migrated.kept += 1;
            }
        }
        std::fs::create_dir_all(&self.root)
            .map_err(|why| format!("the browser root cannot be made: {why}"))?;
        std::fs::write(&marker, "the client's profiles were copied once; edits do not sync\n")
            .map_err(|why| format!("the migration marker cannot be written: {why}"))?;
        Ok(migrated)
    }
}

/// What one migration did, for the boot line and the tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Migrated {
    /// Whether this boot was the one that took ownership.
    pub ran: bool,
    /// Directories copied in - named profiles and the shared cache.
    pub copied: usize,
    /// Directories already present on the server and left untouched.
    pub kept: usize,
}

/// Fill in the named profiles the server does not have. **A name the server
/// already owns keeps every entry it has** and gains the ones it lacks: a
/// stale client copy must not replace logins the server has been using, and
/// a copy a previous boot could not finish is completed here.
fn copied(source: &Path, target: &Path, migrated: &mut Migrated) -> Result<(), String> {
    let entries = match std::fs::read_dir(source) {
        Ok(entries) => entries,
        // **A machine that only ran the old shared browser has no named
        // profiles at all**: nothing to copy is not a failed migration, and
        // treating it as one kept the marker off every boot.
        Err(why) if why.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(why) => return Err(format!("{} cannot be read: {why}", source.display())),
    };
    for entry in entries {
        let entry = entry.map_err(|why| format!("the client's profiles cannot be read: {why}"))?;
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let to = target.join(&name);
        if to.exists() {
            merge_tree(&entry.path(), &to)
                .map_err(|why| format!("the profile '{name}' could not be completed: {why}"))?;
            migrated.kept += 1;
            continue;
        }
        copy_tree(&entry.path(), &to)
            .map_err(|why| format!("the profile '{name}' could not be copied: {why}"))?;
        migrated.copied += 1;
    }
    Ok(())
}

/// Copy one directory tree, files and permissions included. **The live A/B
/// test uses it** to take a copy of a real profile rather than mutate one.
pub(crate) fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|why| format!("{} cannot be made: {why}", to.display()))?;
    for (entry, kind) in entries(from)? {
        copy_entry(&entry, kind, &to.join(entry.file_name()))?;
    }
    Ok(())
}

/// Fill in the entries `to` lacks from `from`, leaving everything it has
/// alone. A directory on both sides is walked rather than replaced.
fn merge_tree(from: &Path, to: &Path) -> Result<(), String> {
    for (entry, kind) in entries(from)? {
        let target = to.join(entry.file_name());
        if target.exists() {
            if kind.is_dir() && target.is_dir() {
                merge_tree(&entry.path(), &target)?;
            }
            continue;
        }
        copy_entry(&entry, kind, &target)?;
    }
    Ok(())
}

/// One directory's entries with their kinds, read as a pair so a caller
/// cannot forget to check the type before touching the path.
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

/// Record the seat a profile belongs to, beside the profile's own files.
pub fn record_owner(dir: &Path, seat: &SessionSlot) -> Result<(), String> {
    std::fs::create_dir_all(dir)
        .map_err(|why| format!("the profile's directory cannot be made: {why}"))?;
    let body = serde_json::to_vec(seat)
        .map_err(|why| format!("the profile's ownership record cannot be written: {why}"))?;
    std::fs::write(dir.join(OWNER_FILE), body)
        .map_err(|why| format!("the profile's ownership record cannot be written: {why}"))
}

/// The seat recorded as this profile's owner. `Ok(None)` is a directory with
/// no record - a profile the server has never opened - and `Err` is a record
/// that cannot be read, which refuses the name rather than guessing an owner.
pub fn owner_of(dir: &Path) -> Result<Option<SessionSlot>, String> {
    let body = match std::fs::read(dir.join(OWNER_FILE)) {
        Ok(body) => body,
        Err(why) if why.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(why) => return Err(format!("the profile's ownership record cannot be read: {why}")),
    };
    serde_json::from_slice::<SessionSlot>(&body)
        .map(Some)
        .map_err(|why| format!("the profile's ownership record cannot be read: {why}"))
}

#[cfg(test)]
mod tests {
    use super::{SHARED_PROFILE, auto_name, name_refusal};
    use forge_primitives::SessionSlot;

    /// **The shared browser's reserved name is not a profile name.** It is
    /// resolved before the registry is consulted, and the registry itself
    /// still refuses it like any other illegal name - so no profile can ever
    /// be created under it and shadow the shared browser.
    #[test]
    fn the_shared_browsers_name_can_never_be_a_profile() {
        assert!(name_refusal(SHARED_PROFILE).is_some(), "`@` is outside the charset");
        assert_ne!(SHARED_PROFILE, "shared", "the word is reserved, not borrowed");
    }

    /// A name that cannot become a directory is refused with the rule, rather
    /// than sanitised into some other name the session did not ask for.
    #[test]
    fn a_name_is_held_to_what_can_be_a_directory() {
        assert!(name_refusal("").is_some());
        assert!(name_refusal(".").is_some());
        assert!(name_refusal("..").is_some());
        assert!(name_refusal("a/b").is_some());
        assert!(name_refusal("../user-data").is_some());
        assert!(name_refusal(&"a".repeat(65)).is_some());
        assert_eq!(name_refusal("hunt-2_x.y"), None);
    }

    /// **Every seat gets a name the registry accepts**, and the project
    /// qualifies the label so two projects' leads do not collide.
    #[test]
    fn a_seats_own_name_folds_its_project_and_label() {
        let seat = SessionSlot::new("Busytools", "forge", "lead");
        assert_eq!(auto_name(&seat), "forge-lead");
        let spaced = SessionSlot::new("Busytools", "my app", "lead");
        assert_eq!(auto_name(&spaced), "my-app-lead");
        assert_eq!(name_refusal(&auto_name(&spaced)), None);
        let long = SessionSlot::new("Busytools", "p".repeat(80), "lead");
        assert_eq!(auto_name(&long).len(), 64);
        assert_eq!(name_refusal(&auto_name(&long)), None);
    }
}
