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
//!
//! Both ends of the lock, because either alone is half a guard: a production
//! constructor that stopped handing the real prober on, and a test constructor
//! that started handing it on, produce the same outage from opposite sides.

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

/// The `new_impl` call the test constructor builds through, which is where a
/// fixture's two probes are decided.
fn the_test_constructors_wiring(source: &str) -> &str {
    let at = source
        .find("fn new_for_test_impl(")
        .expect("the test constructor is in the module this scans");
    let body = &source[at..];
    let call = body.find("Self::new_impl(").expect("the test constructor builds through new_impl");
    let call = &body[call..];
    let end = call.find(")?;").expect("the call to new_impl closes");
    &call[..end]
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

/// The other end of the same lock. A production constructor that stopped
/// wiring the real prober, and a test constructor that started wiring it, do
/// the same damage from opposite sides, and only the first was guarded.
#[test]
fn the_test_constructor_wires_no_real_prober() {
    let source = the_module();
    let fixture = the_test_constructors_wiring(&source);

    // The control: the scan reached the call's own arguments, so a clean
    // verdict is not an empty slice agreeing with everything.
    assert!(
        fixture.contains("cli_version_prober"),
        "the scan did not reach the test constructor's arguments, so its verdict means nothing: \
         {fixture}",
    );

    for prober in ["real_cli_version_prober", "real_service_status_prober"] {
        assert!(
            !fixture.contains(prober),
            "a test constructor hands on `{prober}`, so a fixture reaches the network and every \
             test that reads the core's own stream is exposed to a remote service's health. Call \
             as scanned: {fixture}",
        );
    }
}
