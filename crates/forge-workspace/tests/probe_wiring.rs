//! Test-time guard: the boot probes' production wiring.
//!
//! `Workspace::new` is the only place the real probers enter a real run.
//! Handing the constructor `None` instead, and deleting the helper that
//! leaves orphaned, compiles cleanly - the compiler names the helper and
//! nothing else - and every test stays green with the feature dead in
//! production. No check in the suite holds it, so this one does.
//!
//! The statuspage probe is why the guard covers both. It was wired
//! unconditionally rather than through a parameter, so a test workspace ran a
//! live fetch, and the answer it announced was a slot-less update landing in
//! the middle of whatever a test was reading. Wiring it like the version probe
//! is what removed that, and the same edit that could silently undo it is the
//! one this reads for.

// An integration test is a crate of its own, so clippy's test exemption does
// not reach the scanning helpers below.
#![allow(clippy::expect_used)]

use std::path::PathBuf;

const SOURCE: &str = "src/workspace.rs";

fn the_module() -> String {
    std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(SOURCE))
        .expect("the workspace module reads")
}

/// The constructor's own source, so the scan reads the wiring rather than a
/// `Some(real_...)` that a `#[cfg(test)]` helper happens to hold.
fn the_production_constructor(source: &str) -> &str {
    let at = source
        .find("pub fn new(config_dir: PathBuf)")
        .expect("the production constructor is in the module this scans");
    let body = &source[at..];
    let end = body.find("\n    }\n").expect("the constructor closes on its own line");
    &body[..end]
}

#[test]
fn the_production_constructor_wires_the_real_probes() {
    let source = the_module();

    // The control: a path typo or an empty read reports the same clean
    // verdict as a scan of the module itself.
    assert!(
        source.contains("fn real_cli_version_prober() -> CliVersionProber"),
        "the scan did not read the module that defines the real probe, so its verdict means nothing",
    );

    let wired = the_production_constructor(&source);
    for prober in ["Some(real_cli_version_prober())", "Some(real_service_status_prober())"] {
        assert!(
            wired.contains(prober),
            "the production constructor does not hand `{prober}` on, so that probe is dead in \
             every real run. Constructor as scanned: {wired}",
        );
    }

    assert!(
        source.contains("Box::pin(forge_agent::env::cli_version::fetch_info())"),
        "and the real prober no longer reads the CLI and npm, so `real` is a name rather than a fact",
    );
    assert!(
        source.contains("Box::pin(forge_agent::cloud::service_status::fetch_service_status())"),
        "and the statuspage prober no longer reads the statuspage, so `real` is a name rather than a fact",
    );
}
