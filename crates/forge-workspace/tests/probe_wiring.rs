//! Test-time guard: the version probe's production wiring.
//!
//! `Workspace::new` is the only place the real prober enters a real run.
//! Handing the constructor `None` instead, and deleting the helper that
//! leaves orphaned, compiles cleanly - the compiler names the helper and
//! nothing else - and every test stays green with the feature dead in
//! production. No check in the suite holds it, so this one does.

use std::path::PathBuf;

#[test]
fn the_production_constructor_wires_the_real_probe() {
    let source =
        std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/workspace.rs"))
            .expect("the workspace module reads");

    // The control: a path typo or an empty read reports the same clean
    // verdict as a scan of the module itself.
    assert!(
        source.contains("fn real_cli_version_prober() -> CliVersionProber"),
        "the scan did not read the module that defines the real probe, so its verdict means nothing",
    );

    let wired = source.lines().collect::<Vec<_>>().windows(2).any(|pair| {
        pair[0].contains("pub fn new(config_dir: PathBuf)")
            && pair[1].contains("Some(real_cli_version_prober())")
    });
    assert!(
        wired,
        "the production constructor no longer hands its probe the real prober, so the version is dead in every real run",
    );

    assert!(
        source.contains("Box::pin(forge_agent::env::cli_version::fetch_info())"),
        "and the real prober no longer reads the CLI and npm, so `real` is a name rather than a fact",
    );
}
