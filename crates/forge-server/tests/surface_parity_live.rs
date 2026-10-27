//! The advertised browser surface is answerable, live: every tool the spec
//! table offers is either one the pinned driver publishes or one this host
//! answers itself.
//!
//! **This is the check that would have caught #2502.** That cut deleted the
//! host module behind `browser_form_state` and `browser_click_and_capture`
//! while their specs stayed, so every session was offered two calls nothing
//! could answer; the driver's own refusal was what a session saw. The
//! advertised names reach this crate through
//! `forge_primitives::browser::ADVERTISED_TOOLS`, because `forge-browser`
//! may not depend on the crate that owns the table (`forge-workspace`); a
//! unit test beside the table keeps the two lists equal.
//!
//! **A live browser, not a mock.** The test spawns the staged host bundle
//! and the vendored Playwright driver, so both must be in place:
//! `scripts/stage-browser-host.sh` and `scripts/vendor_browser_stack.sh`
//! (which installs to `FORGE_BROWSER_STACK_DIR`); the stack is read from
//! **`FORGE_BROWSER_TEST_STACK`**.

#![cfg(target_os = "macos")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::PathBuf;

use forge_browser::custom;
use forge_browser::driver::{Driver, cli_path, node_path};
use forge_primitives::browser::ADVERTISED_TOOLS;

/// The vendored driver stack: an override for a checkout under test, else the
/// installed one beside the server's profiles.
fn stack() -> PathBuf {
    match std::env::var("FORGE_BROWSER_TEST_STACK") {
        Ok(stack) => PathBuf::from(stack),
        Err(_) => forge_browser::host_bundle::app_support()
            .expect("the app-support directory")
            .join("browser/stack"),
    }
}

/// End a host this test started itself, however the test ends.
struct HostGuard {
    pid: u32,
    ring: String,
}

impl Drop for HostGuard {
    fn drop(&mut self) {
        let _ = std::process::Command::new("kill").args(["-TERM", &self.pid.to_string()]).status();
        forge_browser::osr::Ring::unlink(&self.ring);
    }
}

/// **Every advertised name is answered, and nothing is answered in secret.**
///
/// The three assertions read in the order a defect would arrive: a spec with
/// no implementation (the #2502 shape), a host arm nothing advertises, and an
/// added argument riding a tool the driver has stopped publishing.
#[ignore = "needs the built host bundle and the vendored driver stack; run with --run-ignored ignored-only"]
#[tokio::test(flavor = "multi_thread")]
async fn every_advertised_browser_tool_is_answered() {
    let stack = stack();
    assert!(
        stack.join("node/bin/node").is_file(),
        "the vendored driver stack is not installed at {} - run \
         scripts/vendor_browser_stack.sh with FORGE_BROWSER_STACK_DIR and point \
         FORGE_BROWSER_TEST_STACK at the directory it installs",
        stack.display()
    );
    let scratch = tempfile::tempdir().expect("a scratch root");
    let profile = scratch.path().join("parity").join("cef");
    let exe = forge_browser::host_bundle::checked_executable().expect("the host bundle");
    let port = forge_browser::spawn::free_port().expect("a free port");
    let ring = format!("/forge-parity-{}", std::process::id());
    let frames = scratch.path().join("frames.sock");
    let (pid, _input, _events) = forge_browser::spawn::spawn_with(
        &exe,
        &profile,
        port,
        &ring,
        800,
        600,
        1,
        (15, 75),
        &frames,
    )
    .expect("the host spawns");
    let _guard = HostGuard { pid, ring: ring.clone() };
    forge_browser::spawn::wait_ready_for(pid, port, &ring, &profile)
        .await
        .expect("the host is ready");
    let chooser = scratch.path().join("chooser.sock");
    let driver = Driver::start(
        &node_path(&stack),
        &cli_path(&stack),
        &format!("http://127.0.0.1:{port}"),
        &scratch.path().join("output"),
        &chooser,
    )
    .await
    .expect("the driver starts");
    let published = driver.tool_names().await.expect("the driver lists its tools");
    println!("the pinned driver publishes: {published:?}");

    for name in ADVERTISED_TOOLS {
        assert!(
            published.iter().any(|tool| tool == name) || custom::answers(name),
            "{name} is advertised but nothing answers it: the pinned driver does not publish it \
             and no host arm answers it",
        );
    }
    for name in custom::host_answered() {
        assert!(
            ADVERTISED_TOOLS.contains(name),
            "{name} is answered by a host arm but is not advertised, so no session can reach it",
        );
    }
    for (tool, argument) in [("browser_click", "force"), ("browser_wait_for", "expression")] {
        assert!(
            published.iter().any(|name| name == tool),
            "the host-added `{argument}` rides {tool}, which the pinned driver no longer \
             publishes",
        );
    }
}
