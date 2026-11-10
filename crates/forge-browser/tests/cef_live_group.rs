//! The `cef-live` test group's filter has to name every test binary that
//! boots a host: two CEF hosts at once starve the macOS runner, so an
//! ungrouped binary runs beside a grouped one and recreates exactly that.
//! `dialog_live` booted a host and was missing from the list, failing the
//! same test on repeated CI runs before the omission was found, so the list
//! is checked against the tests directory rather than trusted.

// An integration test is a crate of its own, so clippy's test exemption does
// not reach it: the denied lints fire on a file that is entirely test code.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::{Path, PathBuf};

/// Built from parts so this file does not match its own needle.
const BOOTS_A_HOST: &str = concat!("Host", "::", "start");

/// The comment above the `cef-live` group states the rule; this is what
/// makes a binary that skips it fail here instead of on a CI runner.
#[test]
fn every_host_booting_test_binary_is_in_the_cef_live_group() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let config = std::fs::read_to_string(manifest.join("../../.config/nextest.toml"))
        .expect("read the nextest config");
    let filter = config
        .split("[[profile.default.overrides]]")
        .find(|block| block.contains("test-group = 'cef-live'"))
        .expect("the cef-live override is present");

    let mut files: Vec<PathBuf> = std::fs::read_dir(manifest.join("tests"))
        .expect("read the tests directory")
        .map(|entry| entry.expect("directory entry").path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("rs"))
        .collect();
    files.sort();

    let mut booters = Vec::new();
    for path in &files {
        let source = std::fs::read_to_string(path).expect("read a test file");
        if !source.contains(BOOTS_A_HOST) {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .expect("a test file's stem is its binary name");
        assert!(
            filter.contains(&format!("binary({name})")),
            "{name} boots a host and must join the cef-live group in \
             .config/nextest.toml: an ungrouped host runs beside a grouped one, \
             and two hosts at once starve the macOS runner"
        );
        booters.push(name.to_owned());
    }

    // A sentinel, so a scan that found nothing (a moved directory, a renamed
    // support module) fails here instead of passing vacuously.
    assert!(
        booters.iter().any(|name| name == "dialog_live"),
        "the scan must find the host-booting binaries; it found {booters:?}"
    );
}
