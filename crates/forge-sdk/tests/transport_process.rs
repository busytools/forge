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

fn lingering_mock_path() -> String {
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mock_claude_lingering.sh").into()
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
    let pid = wait_for_descendant_pid(&marker, &sub).await;
    // Whatever this test does from here, it owns this pid and gives it
    // back by pid alone.
    let _orphan = Orphan(pid);

    // The control: without a live descendant below, the assertion after
    // close would pass for a fixture that never spawned one.
    assert!(is_running(pid), "the fixture's descendant must be running before teardown: pid {pid}");

    // close() reports the SIGKILL fallback as an error by design; the
    // property under test is what became of the descendant.
    let started = tokio::time::Instant::now();
    let _outcome = tokio::time::timeout(WAIT_DEADLINE, sub.close())
        .await
        .expect("teardown must return within the deadline");
    // The child ignores stdin EOF, so the graceful wait runs its whole
    // budget before the group is signalled. A group kill ahead of that
    // wait would return here in milliseconds and red this.
    let elapsed = started.elapsed();
    assert!(
        elapsed >= forge_sdk::CLOSE_WAIT_TIMEOUT,
        "the group is signalled only after the close wait: close returned in {elapsed:?}"
    );

    assert!(
        wait_until_gone(pid).await,
        "a descendant spawned by the session child outlived the teardown: pid {pid}"
    );
}

/// The child exits on its own and a descendant stays behind: teardown
/// still takes the descendant. This is the graceful arm, where the group
/// signal follows a clean wait rather than the kill fallback.
#[tokio::test]
async fn close_kills_a_descendant_after_a_clean_child_exit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let marker = dir.path().join("lingering.pid");
    let opts = Options {
        binary: lingering_mock_path(),
        env: [("FORGE_TEST_ORPHAN_MARKER".to_owned(), marker.to_string_lossy().into_owned())]
            .into(),
        ..Options::default()
    };

    let mut sub = Subprocess::spawn(&opts).await.expect("spawn");
    drop(sub.read_line().await.expect("read").expect("init line"));
    // The fixture writes its descendant's pid once the handshake is
    // answered, so the marker doubles as proof the session got that far.
    sub.write_line(
        "{\"type\":\"control_request\",\"request_id\":\"init-1\",\
         \"request\":{\"subtype\":\"initialize\",\"protocolVersion\":\"1.0\"}}\n",
    )
    .await
    .expect("initialize");

    let pid = wait_for_descendant_pid(&marker, &sub).await;
    let _orphan = Orphan(pid);
    assert!(is_running(pid), "the fixture's descendant must be running before teardown: pid {pid}");

    let outcome = tokio::time::timeout(WAIT_DEADLINE, sub.close())
        .await
        .expect("teardown must return within the deadline");
    assert!(outcome.is_ok(), "a child that exits on stdin EOF closes clean: {outcome:?}");

    assert!(
        wait_until_gone(pid).await,
        "a descendant outlived a clean teardown of its session: pid {pid}"
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

async fn wait_for_descendant_pid(marker: &Path, sub: &Subprocess) -> u32 {
    let deadline = tokio::time::Instant::now() + WAIT_DEADLINE;
    while tokio::time::Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(marker)
            && let Ok(pid) = text.trim().parse()
        {
            return pid;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // A sleeper the fixture spawned without writing its pid has no number
    // to give back, so the group this test's own child leads is swept
    // instead - a group, never a name.
    if let Some(pgid) = sub.child_pid() {
        let _ =
            std::process::Command::new("kill").args(["-s", "KILL", &format!("-{pgid}")]).status();
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
