//! The headless server binary, run as a process: it boots the whole core
//! with no view, serves the socket, takes the single-instance lock and
//! prints completions.
//!
//! The binary IS the instrument: a test that reached the boot another way
//! would leave the thing a deployment actually runs unexercised.

// An integration test is a crate of its own, so clippy's test exemption does
// not reach it: the denied lints fire on a file that is entirely test code.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;

/// Two ports the OS just handed out.
///
/// Both are bound at once before either is released, so they cannot be the
/// same number, and the window in which something else could take one is as
/// small as it gets.
fn two_free_ports() -> (u16, u16) {
    let first = std::net::TcpListener::bind("127.0.0.1:0").expect("first port");
    let second = std::net::TcpListener::bind("127.0.0.1:0").expect("second port");
    let pair =
        (first.local_addr().expect("addr").port(), second.local_addr().expect("addr").port());
    drop(first);
    drop(second);
    pair
}

/// A scratch home + config dir over one project.
///
/// The account is base-url'd at a dead local port, so its probe fails in
/// milliseconds instead of reaching a real endpoint from a test. The HOME
/// redirect is what keeps the durable store (and the single-instance lock)
/// inside the scratch tree rather than under the real one.
fn write_fixture(
    root: &std::path::Path,
    server_port: u16,
    gateway_port: u16,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let home = root.join("home");
    let config = root.join("config");
    let project = root.join("project");
    std::fs::create_dir_all(config.join("forge")).expect("config dir");
    std::fs::create_dir_all(&home).expect("home dir");
    std::fs::create_dir_all(&project).expect("project dir");
    std::fs::write(
        config.join("forge/forge.toml"),
        format!(
            r#"
[[orgs]]
name = "Default"
accounts = ["Stub"]

[[orgs.projects]]
name = "forge"
path = "{}"

[[accounts]]
display_name = "Stub"
token = "t"
provider = "openrouter"
base_url = "http://127.0.0.1:9/api"
models = ["claude-sonnet-5"]

[server]
enabled = true
bind = "127.0.0.1"
port = {server_port}

[gateway]
port = {gateway_port}
"#,
            project.display(),
        ),
    )
    .expect("write forge.toml");
    (home, config)
}

/// The server binary over one scratch tree.
fn server_command(home: &std::path::Path, config: &std::path::Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_forge"));
    command
        .env("HOME", home)
        // The data dir too: on a Linux runner with XDG_DATA_HOME exported,
        // the store and the single-instance lock would resolve outside the
        // scratch tree and onto a live path.
        .env("XDG_DATA_HOME", home.join(".local").join("share"))
        .env("CLAUDE_CONFIG_DIR", config)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        // A panicking assertion must not leak a detached server against a
        // deleted tempdir.
        .kill_on_drop(true);
    command
}

/// Send a signal the way a system does, without linking a signal crate: the
/// `kill` the PATH resolves is the instrument.
async fn send_signal(pid: u32, signal: &str) {
    let status = tokio::process::Command::new("kill")
        .arg(signal)
        .arg(pid.to_string())
        .status()
        .await
        .expect("kill runs");
    assert!(status.success(), "kill {signal} {pid} lands: {status}");
}

/// SIGTERM, the stop a supervisor sends.
async fn terminate(pid: u32) {
    send_signal(pid, "-TERM").await;
}

/// Wait until the server answers on `port`, or fail with its stderr.
async fn wait_for_port(port: u16, server: &mut tokio::process::Child) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(Some(status)) = server.try_wait() {
            let stderr = drain_stderr(server).await;
            panic!("the server exited before binding ({status}): {stderr}");
        }
        if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return;
        }
        if std::time::Instant::now() > deadline {
            drop(server.kill().await);
            let stderr = drain_stderr(server).await;
            panic!("the server never bound port {port}: {stderr}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Whatever the server wrote to stderr, for a failure's message.
async fn drain_stderr(server: &mut tokio::process::Child) -> String {
    use tokio::io::AsyncReadExt as _;
    let Some(mut stderr) = server.stderr.take() else { return String::new() };
    let mut bytes = Vec::new();
    let _ = stderr.read_to_end(&mut bytes).await;
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Walk the socket with the protocol client, one JSON message per line.
///
/// The instrument a person runs, not a hand-rolled client: the same binary
/// `protocol.rs` drives, so what this test hears is what a client hears.
async fn walk(script: &str, url: &str) -> Vec<Value> {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("walk.txt");
    std::fs::write(&path, script).expect("write the script");

    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_forge-protocol-client"))
        .arg("--url")
        .arg(url)
        .arg("--script")
        .arg(&path)
        .arg("--seconds")
        .arg("3")
        .output()
        .await
        .expect("the client runs");
    assert!(
        output.status.success(),
        "the client exited cleanly: {}",
        String::from_utf8_lossy(&output.stderr),
    );

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("one message per line"))
        .collect()
}

/// A scratch tree whose project starts at boot, over a mock `claude` on
/// PATH: the boot wave spawns a live session, and that session's child
/// leaves a sleeper whose pid lands in `marker`.
///
/// The mock is the SDK's own lingering fixture under the name production
/// spawns, so one script describes the shape for both layers.
fn write_session_fixture(
    root: &std::path::Path,
    server_port: u16,
    gateway_port: u16,
    marker: &std::path::Path,
) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let home = root.join("home");
    let config = root.join("config");
    let project = root.join("project");
    let bin = root.join("bin");
    std::fs::create_dir_all(config.join("forge")).expect("config dir");
    std::fs::create_dir_all(&home).expect("home dir");
    std::fs::create_dir_all(&project).expect("project dir");
    std::fs::create_dir_all(&bin).expect("bin dir");
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../forge-sdk/tests/fixtures/mock_claude_lingering.sh"
        ),
        bin.join("claude"),
    )
    .expect("install the mock claude");
    std::fs::write(
        config.join("forge/forge.toml"),
        format!(
            r#"
[env]
FORGE_TEST_ORPHAN_MARKER = "{marker}"

[[orgs]]
name = "Default"
accounts = ["Stub"]

[[orgs.projects]]
name = "forge"
path = "{project}"
auto_start = true
model = "claude-sonnet-5"

[[accounts]]
display_name = "Stub"
token = "t"
provider = "openrouter"
base_url = "http://127.0.0.1:9/api"
models = ["claude-sonnet-5"]

[server]
enabled = true
bind = "127.0.0.1"
port = {server_port}

[gateway]
port = {gateway_port}
"#,
            marker = marker.display(),
            project = project.display(),
        ),
    )
    .expect("write forge.toml");
    (home, config, bin)
}

/// The pid the session's mock child wrote, or a failure naming the tree.
async fn wait_for_marker_pid(marker: &std::path::Path) -> u32 {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(text) = std::fs::read_to_string(marker)
            && let Ok(pid) = text.trim().parse()
        {
            return pid;
        }
        assert!(
            std::time::Instant::now() <= deadline,
            "the session's child never wrote its descendant's pid to {}",
            marker.display()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Read the pid's own state; a zombie is gone, not surviving.
fn is_running(pid: u32) -> bool {
    let out = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "stat="])
        .output()
        .expect("ps runs");
    let stat = String::from_utf8_lossy(&out.stdout);
    let stat = stat.trim();
    !stat.is_empty() && !stat.starts_with('Z')
}

async fn wait_until_gone(pid: u32) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        if !is_running(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    !is_running(pid)
}

/// A pid this test owns, killed by number when it ends however it ends.
struct Orphan(u32);

impl Drop for Orphan {
    fn drop(&mut self) {
        let _ = std::process::Command::new("kill").args(["-KILL", &self.0.to_string()]).status();
    }
}

/// A stop signal takes the session's descendants with it. SIGTERM is the
/// supervisor's stop; SIGHUP is a terminal closing, whose default
/// disposition kills forge with no unwinding at all.
#[tokio::test]
async fn a_stop_signal_takes_the_sessions_descendants_with_it() {
    for signal in ["-TERM", "-HUP"] {
        assert_stop_leaves_no_descendant(signal).await;
    }
}

async fn assert_stop_leaves_no_descendant(signal: &str) {
    let dir = tempfile::tempdir().expect("tempdir");
    let (port, gateway) = two_free_ports();
    let marker = dir.path().join("session.pid");
    let (home, config, bin) = write_session_fixture(dir.path(), port, gateway, &marker);

    let mut command = server_command(&home, &config);
    command
        .env("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").expect("PATH is set")));
    let mut server = command.spawn().expect("the server spawns");
    wait_for_port(port, &mut server).await;

    // The control: a session is live and its descendant is running before
    // the stop, so a pass below cannot come from a session that never was.
    let pid = wait_for_marker_pid(&marker).await;
    let _orphan = Orphan(pid);
    assert!(is_running(pid), "the session's descendant must be running before {signal}: pid {pid}");

    send_signal(server.id().expect("the server has a pid while it runs"), signal).await;
    let exit = tokio::time::timeout(Duration::from_secs(30), server.wait())
        .await
        .expect("the server finishes its stop within the deadline")
        .expect("the server is reaped");
    assert!(exit.success(), "the {signal} stop exits clean: {exit}");

    assert!(
        wait_until_gone(pid).await,
        "a descendant outlived the server's {signal} stop: pid {pid}"
    );
}

/// The boot stands alone: the binary binds the socket, greets a client and
/// answers the home read - with no view anywhere in the process - and
/// the stop a system sends releases what the boot took.
#[tokio::test]
async fn headless_boot_serves_the_home_and_stops_clean() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (port, gateway) = two_free_ports();
    let (home, config) = write_fixture(dir.path(), port, gateway);

    let mut server = server_command(&home, &config).spawn().expect("the server spawns");
    wait_for_port(port, &mut server).await;

    let url = format!("ws://127.0.0.1:{port}/socket");
    let transcript = walk("subscribe home\n", &url).await;

    let greeting = transcript.first().expect("the greeting lands first");
    assert_eq!(greeting["kind"], "greeting", "the socket greets a client: {transcript:?}");
    assert!(greeting["version"].is_u64(), "the greeting states the protocol: {greeting}");
    assert!(
        greeting["forge_version_short"].as_str().is_some_and(|version| !version.is_empty()),
        "and the build that answered: {greeting}",
    );

    let home_snapshot = transcript
        .iter()
        .find(|message| message["kind"] == "snapshot" && message["subject"] == "home")
        .expect("the home subscription is answered");
    let projects =
        home_snapshot["data"]["projects"].as_array().expect("projects crosses as a list");
    assert!(
        projects.iter().any(|row| row["project"]["name"] == "forge"),
        "with the project the fixture declares, not an empty roster: {home_snapshot}",
    );

    // SIGTERM takes the same path Ctrl-C does: shutdown, then a clean exit.
    // The lock's release is the property a long-lived headless run leans on,
    // so it is asserted rather than assumed - by the next boot taking it.
    let holder = server.id().expect("the server has a pid while it runs");
    terminate(holder).await;
    let status = server.wait().await.expect("the server is reaped");
    assert!(status.success(), "the signal path exits clean: {status}");

    let mut again = server_command(&home, &config).spawn().expect("the server boots again");
    wait_for_port(port, &mut again).await;
    terminate(again.id().expect("the restarted server has a pid")).await;
    let status = again.wait().await.expect("the restarted server is reaped");
    assert!(status.success(), "and stops clean in turn: {status}");
}

/// One forge per config dir, headless included: the second boot is refused
/// in the holder's own words.
#[tokio::test]
async fn a_second_instance_is_refused_with_the_holders_pid() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (port, gateway) = two_free_ports();
    let (home, config) = write_fixture(dir.path(), port, gateway);

    let mut first = server_command(&home, &config).spawn().expect("the first server spawns");
    wait_for_port(port, &mut first).await;
    let holder = first.id().expect("the first server has a pid while it runs");

    let second = server_command(&home, &config).output().await.expect("the second boot runs");
    assert!(!second.status.success(), "the second boot is refused rather than served");
    let refusal = String::from_utf8_lossy(&second.stderr);
    assert!(
        refusal.contains("already running on this config dir"),
        "the refusal says why: {refusal}",
    );
    assert!(
        refusal.contains(&holder.to_string()),
        "and names the holder (PID {holder}): {refusal}",
    );

    // SIGKILL frees the lock too: the flock lives with the process, so the
    // next boot takes it rather than being refused.
    drop(first.kill().await);
    let _ = first.wait().await.expect("the first server is reaped");

    let mut third = server_command(&home, &config).spawn().expect("a third boot runs");
    wait_for_port(port, &mut third).await;
    terminate(third.id().expect("the third server has a pid")).await;
    let status = third.wait().await.expect("the third server is reaped");
    assert!(status.success(), "and it stops clean: {status}");
}

/// The hidden completion flag survives the move: `scripts/install.sh` calls
/// it on the binary this crate now ships.
#[tokio::test]
async fn generate_completion_survives_on_the_server_binary() {
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_forge"))
        .arg("--generate-completion")
        .arg("zsh")
        .output()
        .await
        .expect("the completion run starts");
    assert!(
        output.status.success(),
        "the completion run exits cleanly: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let script = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        script.lines().next(),
        Some("#compdef forge"),
        "the script's first line names the command exactly, `forgex` included: {script}",
    );
}
