//! The `cef-live` group's filter has to name every test binary that boots a
//! host: two CEF hosts at once starve the macOS runner, so an ungrouped
//! binary runs beside a grouped one and recreates exactly that.
//! `dialog_live` booted a host and was missing from the list, failing the
//! same test on repeated CI runs before the omission was found, so the list
//! is checked against both crates' test directories rather than trusted.
//!
//! The check is textual: it looks for the call sites each crate's live tests
//! boot through. A boot hidden behind an alias or a helper in another module
//! evades it, which is why the needles are named here rather than guessed.

use std::path::PathBuf;

/// forge-browser's live tests boot a host directly. Built from parts so this
/// file does not match its own needle when the scan reads the directory it
/// lives in.
const BROWSER_BOOT: &str = concat!("Host", "::", "start");

/// forge-server's live tests boot through the browser service instead.
const SERVER_BOOTS: [&str; 3] = ["BrowserService", "spawn_with", "checked_executable"];

/// The comment above the `cef-live` group states the rule; this is what
/// makes a binary that skips it fail here instead of on a CI runner.
#[test]
fn host_booting_test_binaries_join_the_cef_live_group() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let config = std::fs::read_to_string(manifest.join("../../.config/nextest.toml"))
        .expect("read the nextest config");
    let filter = config
        .split("[[profile.default.overrides]]")
        .find(|block| block.contains("test-group = 'cef-live'"))
        .expect("the cef-live override is present");
    // A binary in the group still boots beside an ungrouped one if the group
    // stopped serialising, so the property the membership serves is checked
    // too. Moving the override elsewhere is beyond what a text check sees.
    assert!(config.contains("max-threads = 1"), "the cef-live group must still serialise hosts");

    for (dir, needles, sentinel) in [
        ("../../crates/forge-browser/tests", &[BROWSER_BOOT][..], "dialog_live"),
        ("../../crates/forge-server/tests", &SERVER_BOOTS[..], "browser_service"),
    ] {
        let directory = manifest.join(dir);
        let mut files: Vec<PathBuf> = std::fs::read_dir(&directory)
            .expect("read a test directory")
            .map(|entry| entry.expect("directory entry").path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("rs"))
            .collect();
        files.sort();

        let mut booters = Vec::new();
        for path in &files {
            let source = std::fs::read_to_string(path).expect("read a test file");
            if !needles.iter().any(|needle| source.contains(needle)) {
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

        // A sentinel per directory, so a scan that found nothing (a moved
        // directory, a renamed needle) fails here instead of passing vacuously.
        assert!(
            booters.iter().any(|name| name == sentinel),
            "the scan of {dir} must find the host-booting binaries; it found {booters:?}"
        );
    }
}
