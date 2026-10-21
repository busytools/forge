//! Integration test for `transport::process::Subprocess` against the mock binary.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::Path;
use std::time::Duration;

use forge_sdk::Options;
use forge_sdk::transport::process::Subprocess;

fn mock_binary_path() -> String {
    // Use the raw mock that doesn't expect an initialize handshake -
    // transport_process tests exercise Subprocess directly, below
    // Client's initialize round-trip.
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mock_claude_raw.sh").into()
}

fn orphan_mock_path() -> String {
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mock_claude_orphan.sh").into()
}

/// Ceiling on every wait below. A condition that never holds fails the
/// test; it never turns into a longer sleep.
const WAIT_DEADLINE: Duration = Duration::from_secs(15);

#[tokio::test]
async fn spawn_reads_init_line() {
    let opts = Options { binary: mock_binary_path(), ..Options::default() };

    let mut sub = Subprocess::spawn(&opts).await.expect("spawn");
    let line = sub.read_line().await.expect("read").expect("init line present");
    assert!(line.contains("\"type\":\"system\""), "expected init system line, got: {line}");
    sub.close().await.expect("close");
}

#[tokio::test]
async fn send_and_read_roundtrip() {
    let opts = Options { binary: mock_binary_path(), ..Options::default() };

    let mut sub = Subprocess::spawn(&opts).await.expect("spawn");
    // Drop the init line.
    let _init = sub.read_line().await.expect("read").expect("init");

    sub.write_line("any input\n").await.expect("write");

    let assistant = sub.read_line().await.expect("read").expect("assistant");
    assert!(assistant.contains("\"type\":\"assistant\""));

    let result = sub.read_line().await.expect("read").expect("result");
    assert!(result.contains("\"type\":\"result\""));

    sub.close().await.expect("close");
}

/// The session child leaves a descendant behind; teardown must take it
/// down too. Asserted by the pid the fixture wrote, never by a name.
#[tokio::test]
async fn close_kills_a_descendant_that_outlives_the_child() {
    let dir = tempfile::tempdir().expect("tempdir");
    let marker = dir.path().join("orphan.pid");
    let opts = Options {
        binary: orphan_mock_path(),
        env: [("FORGE_TEST_ORPHAN_MARKER".to_owned(), marker.to_string_lossy().into_owned())]
            .into(),
        ..Options::default()
    };

    let mut sub = Subprocess::spawn(&opts).await.expect("spawn");
    let pid = wait_for_descendant_pid(&marker).await;
    // Whatever this test does from here, it owns this pid and gives it
    // back by pid alone.
    let _orphan = Orphan(pid);

    // The control: without a live descendant below, the assertion after
    // close would pass for a fixture that never spawned one.
    assert!(is_running(pid), "the fixture's descendant must be running before teardown: pid {pid}");

    // close() reports the SIGKILL fallback as an error by design; the
    // property under test is what became of the descendant.
    let _outcome = tokio::time::timeout(WAIT_DEADLINE, sub.close())
        .await
        .expect("teardown must return within the deadline");

    assert!(
        wait_until_gone(pid).await,
        "a descendant spawned by the session child outlived the teardown: pid {pid}"
    );
}

/// A pid the test owns, killed by number when the test ends however it
/// ends. A failing run would otherwise leave its sleeper running.
struct Orphan(u32);

impl Drop for Orphan {
    fn drop(&mut self) {
        let _ = std::process::Command::new("kill").args(["-KILL", &self.0.to_string()]).status();
    }
}

async fn wait_for_descendant_pid(marker: &Path) -> u32 {
    let deadline = tokio::time::Instant::now() + WAIT_DEADLINE;
    while tokio::time::Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(marker)
            && let Ok(pid) = text.trim().parse()
        {
            return pid;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the fixture never wrote its descendant's pid to {}", marker.display());
}

async fn wait_until_gone(pid: u32) -> bool {
    let deadline = tokio::time::Instant::now() + WAIT_DEADLINE;
    while tokio::time::Instant::now() < deadline {
        if !is_running(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    !is_running(pid)
}

/// Read the pid's own state rather than the process table at large, and
/// count a zombie as gone: a killed descendant sits unreaped for a
/// moment, and that moment is not survival.
fn is_running(pid: u32) -> bool {
    let out = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "stat="])
        .output()
        .expect("ps runs");
    let stat = String::from_utf8_lossy(&out.stdout);
    let stat = stat.trim();
    !stat.is_empty() && !stat.starts_with('Z')
}

#[tokio::test]
async fn spawn_rejects_missing_binary() {
    let opts = Options { binary: "/definitely/does/not/exist/claude".into(), ..Options::default() };
    let err = Subprocess::spawn(&opts).await.expect_err("should fail");
    let rendered = format!("{err}");
    assert!(
        rendered.contains("not found") || rendered.contains("No such file"),
        "expected missing-binary error, got: {rendered}"
    );
}
