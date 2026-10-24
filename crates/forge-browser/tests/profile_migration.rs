//! The one-time copy of the client's browser profiles onto the server.
//!
//! The client owned these directories until this cut; a machine that ran it
//! has its logins and its carried pages there. The copy has to happen once,
//! fill in only what the server is missing, and never replace a profile the
//! server has been using.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use forge_browser::profiles::{Store, owner_of};
use forge_primitives::SessionSlot;

/// A file the migration can be seen through.
fn write(path: &std::path::Path, body: &str) {
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("the directory");
    std::fs::write(path, body).expect("the file");
}

/// The client's tree as it stands on a machine that ran it: one named
/// profile with its cache, owner record and carried page, and the shared
/// browser's cache.
fn client_tree(root: &std::path::Path, seat: &SessionSlot) -> std::path::PathBuf {
    let browser = root.join("dev.vedhavyas.forge").join("browser");
    write(&browser.join("profiles/hunt/cef/Cookies"), "hunt's cookies");
    write(&browser.join("profiles/hunt/cef/page"), "https://example.com/job");
    forge_browser::profiles::record_owner(&browser.join("profiles/hunt"), seat)
        .expect("the client's owner record");
    write(&browser.join("cef-profile/page"), "https://example.com/");
    browser
}

fn seat() -> SessionSlot {
    SessionSlot::lead("TestOrg", "proj")
}

/// **The copy runs, and the logins, the carried page and the owner travel
/// with it.** The shared browser's cache lands where the server keeps it,
/// which is not the same path the client used.
#[test]
fn the_client_profiles_are_copied_once_with_their_owner_and_page() {
    let root = tempfile::tempdir().expect("a temp dir");
    let source = client_tree(&root.path().join("client"), &seat());
    let server = Store::at(&root.path().join("server"));

    let migrated = server.migrate(Some(&source)).expect("the copy runs");

    assert!(migrated.ran, "the first boot takes ownership");
    assert_eq!(migrated.copied, 2, "the named profile and the shared cache");
    assert_eq!(
        std::fs::read_to_string(server.named("hunt").join("cef/Cookies")).expect("the copy"),
        "hunt's cookies",
        "the logins came across",
    );
    assert_eq!(
        std::fs::read_to_string(server.named("hunt").join("cef/page")).expect("the copy"),
        "https://example.com/job",
        "and so did the carried page",
    );
    assert_eq!(
        owner_of(&server.named("hunt")).expect("the owner reads"),
        Some(seat()),
        "the seat that opened the name stays its owner",
    );
    assert_eq!(
        std::fs::read_to_string(server.shared().join("cef/page")).expect("the copy"),
        "https://example.com/",
        "the shared browser's cache lands under the server's shared directory",
    );
}

/// **A second boot does nothing**, even when the client tree has grown since:
/// the marker is the whole condition, so nobody's later client profiles are
/// imported on top of a server that has moved on.
#[test]
fn a_second_boot_copies_nothing() {
    let root = tempfile::tempdir().expect("a temp dir");
    let source = client_tree(&root.path().join("client"), &seat());
    let server = Store::at(&root.path().join("server"));
    server.migrate(Some(&source)).expect("the copy runs");
    write(&source.join("profiles/later/cef/Cookies"), "arrived after the cut");

    let again = server.migrate(Some(&source)).expect("the second boot");

    assert!(!again.ran, "the marker is present, so nothing is read");
    assert!(!server.named("later").exists(), "and a later client profile is not imported");
}

/// **A profile the server already has keeps every entry it has** and gains
/// the ones it lacks: a stale client copy must not replace logins the server
/// has been using, and a copy a previous boot left half done is completed.
#[test]
fn an_existing_server_profile_keeps_its_own_and_gains_what_it_lacks() {
    let root = tempfile::tempdir().expect("a temp dir");
    let source = client_tree(&root.path().join("client"), &seat());
    let server = Store::at(&root.path().join("server"));
    write(&server.named("hunt").join("cef/Cookies"), "the server's cookies");

    let migrated = server.migrate(Some(&source)).expect("the copy runs");

    assert_eq!(migrated.kept, 1, "the name was already the server's");
    assert_eq!(
        std::fs::read_to_string(server.named("hunt").join("cef/Cookies")).expect("the file"),
        "the server's cookies",
        "the server's logins stay",
    );
    assert_eq!(
        std::fs::read_to_string(server.named("hunt").join("cef/page")).expect("the missing entry"),
        "https://example.com/job",
        "and an entry the server does not have is filled in",
    );
}

/// **A copy that could not finish leaves no marker**, names the profile it
/// could not read, and completes on the next boot. The marker is the
/// ownership claim: taking it over a partial copy would strand the rest.
#[test]
fn a_failed_copy_leaves_no_marker_and_the_next_boot_completes_it() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().expect("a temp dir");
    let source = client_tree(&root.path().join("client"), &seat());
    write(&source.join("profiles/locked/cef/Cookies"), "locked away");
    let locked = source.join("profiles/locked");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
        .expect("the profile cannot be read");

    let server = Store::at(&root.path().join("server"));
    let why = server.migrate(Some(&source)).expect_err("an unreadable profile fails the copy");
    assert!(why.contains("locked"), "the profile is named: {why}");
    assert!(
        !root.path().join("server/profiles-migrated").exists(),
        "no marker over a copy that did not finish",
    );

    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))
        .expect("the profile can be read again");
    let migrated = server.migrate(Some(&source)).expect("the next boot completes the copy");
    assert!(migrated.ran, "the marker was absent, so the copy ran again");
    assert_eq!(
        std::fs::read_to_string(server.named("hunt").join("cef/Cookies")).expect("the copy"),
        "hunt's cookies",
        "the profiles that copied the first time are still there",
    );
    assert_eq!(
        std::fs::read_to_string(server.named("locked").join("cef/Cookies")).expect("the copy"),
        "locked away",
        "and the one that could not be read is taken now",
    );
    assert!(
        root.path().join("server/profiles-migrated").exists(),
        "a complete copy takes the marker",
    );
}

/// **A machine that only had the old shared browser has no `profiles/` at
/// all**, and that is nothing to copy rather than a failed migration: the
/// shared cache comes across and the marker is written, so the boot stops
/// claiming a failure that never happened.
#[test]
fn a_source_without_named_profiles_still_migrates_the_shared_browser() {
    let root = tempfile::tempdir().expect("a temp dir");
    let source = root.path().join("client/dev.vedhavyas.forge/browser");
    write(&source.join("cef-profile/page"), "https://example.com/");
    let server = Store::at(&root.path().join("server"));

    let migrated = server.migrate(Some(&source)).expect("the shared cache copies");

    assert!(migrated.ran, "the boot took ownership");
    assert_eq!(migrated.copied, 1, "the shared cache, and no named profiles");
    assert_eq!(
        std::fs::read_to_string(server.shared().join("cef/page")).expect("the copy"),
        "https://example.com/",
        "the shared browser's page came across",
    );
    assert!(
        root.path().join("server/profiles-migrated").exists(),
        "a complete copy takes the marker",
    );
}

/// **A machine that never ran the client still takes ownership**: there is
/// nothing to copy, and the marker is written so a client tree that appears
/// later is never imported.
#[test]
fn no_client_tree_still_writes_the_marker() {
    let root = tempfile::tempdir().expect("a temp dir");
    let server = Store::at(&root.path().join("server"));

    let migrated = server.migrate(None).expect("ownership begins");

    assert!(migrated.ran, "the boot took ownership");
    assert_eq!(migrated.copied, 0, "with nothing to copy");
    let later = client_tree(&root.path().join("client"), &seat());
    let again = server.migrate(Some(&later)).expect("a later boot");
    assert!(!again.ran, "and a client tree that appears later is not imported");
}
