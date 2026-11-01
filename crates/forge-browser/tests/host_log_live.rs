//! What a live test's failure carries: the host's own last words.
//!
//! **The log names the wait that expired**, and it lives in the failing
//! test's scratch dir, which nothing reaches after the panic. So the shared
//! [`support::Host`] prints its log's tail as the test unwinds; this file
//! pins that the print lands in the failure output and that a passing test
//! prints nothing. The tail's own bounds are pinned by the reader's test in
//! `spawn.rs`, which every caller shares.

#![cfg(target_os = "macos")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
#![allow(unsafe_code, reason = "the capture points this test's own stderr at a file")]

mod support;

use std::io::Write as _;
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use forge_browser::osr::Ring;

/// How long CEF is given to boot.
const BOUND: Duration = Duration::from_secs(60);

/// The ring, unlinked when the test ends however it ends.
struct RingOnly(String);

impl Drop for RingOnly {
    fn drop(&mut self) {
        Ring::unlink(&self.0);
    }
}

/// **A passing test prints nothing.** The tail rides the unwind alone; if
/// that gate went, every green live run would carry four lines of host log.
#[test]
fn a_passing_host_prints_nothing() {
    let scratch = tempfile::tempdir().expect("a scratch dir");
    let exe = stand_in(scratch.path(), "#!/bin/sh\necho \"cef host: stub\" >&2\nexec sleep 30\n");
    let log = scratch.path().join("host.log");
    let ring = RingOnly(format!("/forge-quiet-{}", std::process::id()));
    let host = support::Host::start(
        &exe,
        &scratch.path().join("profile"),
        1,
        &ring.0,
        "800x600",
        &[],
        &log,
    );
    wait_for_the_line(&log);

    let (output, panicked) = capturing_stderr(|| drop(host));
    assert!(!panicked, "the drop of a passing test's host panicked");
    assert!(output.is_empty(), "a passing test prints no host tail: {output}");
}

/// **A host that wrote nothing is said in words**, rather than as a blank
/// tail that reads like a log with no content.
#[test]
fn an_empty_log_is_said_in_words() {
    let scratch = tempfile::tempdir().expect("a scratch dir");
    let exe = stand_in(scratch.path(), "#!/bin/sh\nexec sleep 30\n");
    let log = scratch.path().join("host.log");
    let ring = RingOnly(format!("/forge-empty-{}", std::process::id()));
    let host = support::Host::start(
        &exe,
        &scratch.path().join("profile"),
        1,
        &ring.0,
        "800x600",
        &[],
        &log,
    );

    let (output, panicked) = capturing_stderr(|| {
        let _host = host;
        fail_a_spent_bound();
    });
    assert!(panicked, "the spent bound did not panic");
    assert!(output.contains("the host log is empty"), "the empty log is not said: {output}");
}

/// **A bound that expires carries the host's own last words**, whichever
/// wait it was. The short bound here is meant to fail; the test reads the
/// failure output the way CI does.
#[test]
fn a_failed_bound_carries_the_hosts_last_words() {
    let scratch = tempfile::tempdir().expect("a scratch dir");
    let exe = support::host_executable(scratch.path());
    let profile = scratch.path().join("profile");
    std::fs::create_dir_all(&profile).expect("the profile dir");
    let port = support::free_port();
    let ring = RingOnly(format!("/forge-tail-{}", std::process::id()));
    let log = scratch.path().join("host.log");
    let mut host = support::Host::start(&exe, &profile, port, &ring.0, "800x600", &[], &log);
    support::wait_for_page(port, &log, BOUND);

    // **A helper process inherits the host's stderr**, and `terminate` waits
    // for the direct child alone, so the log is not settled when it returns.
    // The seeded pair is what the failure is asserted against: the joined
    // form cannot come from a full-log print, and a line written after it
    // does not move the pair the way it would move a window. So the tail is
    // only seeded once a helper still on its way out has finished writing.
    host.terminate(Duration::from_secs(15));
    wait_for_the_log_to_settle(&log);
    let mut file = std::fs::OpenOptions::new().append(true).open(&log).expect("the host log");
    writeln!(file, "seed line one").expect("the seeded line");
    writeln!(file, "seed line two").expect("the seeded line");
    drop(file);

    let (output, panicked) = capturing_stderr(|| {
        let _host = host;
        fail_a_spent_bound();
    });
    assert!(panicked, "the spent bound did not panic");
    assert!(
        output.contains("seed line one | seed line two"),
        "the failure does not carry the tail's joined form: {output}"
    );
}

/// A stand-in host: [`support::Host::start`] takes any executable, so these
/// tests never stage CEF.
fn stand_in(scratch: &Path, body: &str) -> PathBuf {
    let exe = scratch.join("stand-in-host.sh");
    std::fs::write(&exe, body).expect("the stand-in is written");
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755))
        .expect("the stand-in is executable");
    exe
}

/// Wait for the log to stop growing before appending to it. **An inherited
/// stderr can write after the host is gone**, and a line landing after the
/// seeds pushes them out of the tail's window.
fn wait_for_the_log_to_settle(log: &Path) {
    const QUIET: Duration = Duration::from_millis(500);
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut seen = std::fs::metadata(log).map_or(0, |meta| meta.len());
    let mut quiet_since = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(50));
        let len = std::fs::metadata(log).map_or(0, |meta| meta.len());
        if len != seen {
            seen = len;
            quiet_since = Instant::now();
        } else if quiet_since.elapsed() >= QUIET {
            return;
        }
        assert!(Instant::now() < deadline, "the host log never stopped growing");
    }
}

/// Wait for the stand-in to write its own line, so the quiet a passing test
/// must keep is quiet over real log content.
fn wait_for_the_line(log: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::fs::metadata(log).map_or(0, |meta| meta.len()) == 0 {
        assert!(Instant::now() < deadline, "the stand-in never wrote its line");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Fail a bound that has already expired, the shape every live wait takes.
fn fail_a_spent_bound() {
    let deadline = Instant::now() + Duration::from_millis(1);
    std::thread::sleep(Duration::from_millis(5));
    assert!(Instant::now() < deadline, "the host's page target never answered");
}

/// Run `body` with this process's stderr pointed at a file; return what was
/// written and whether the body panicked.
///
/// **One test per process is assumed**: the whole process's fd 2 is pointed
/// at the capture, which nextest guarantees and libtest does not.
fn capturing_stderr(body: impl FnOnce()) -> (String, bool) {
    let capture = tempfile::NamedTempFile::new().expect("a capture file");
    // SAFETY: dup/dup2 on this test's own process, restored before returning.
    let saved = unsafe { libc::dup(libc::STDERR_FILENO) };
    assert!(saved >= 0, "stderr was not duplicated");
    let pointed = unsafe { libc::dup2(capture.as_file().as_raw_fd(), libc::STDERR_FILENO) };
    assert!(pointed >= 0, "stderr was not pointed at the capture");
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).is_err();
    let _ = std::io::stderr().flush();
    // SAFETY: restores the descriptor the test started with, and closes the copy.
    unsafe {
        libc::dup2(saved, libc::STDERR_FILENO);
        libc::close(saved);
    }
    (std::fs::read_to_string(capture.path()).expect("the captured output"), panicked)
}
