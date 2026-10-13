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
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_forge-server"));
    command
        .env("HOME", home)
        .env("CLAUDE_CONFIG_DIR", config)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    command
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

/// The boot stands alone: the binary binds the socket, greets a client and
/// answers the home read - with no terminal anywhere in the process.
#[tokio::test]
async fn headless_boot_serves_the_home_over_the_socket() {
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
    assert!(
        home_snapshot["data"]["projects"].as_array().is_some(),
        "with the roster the fixture declares: {home_snapshot}",
    );

    drop(server.kill().await);
    let status = server.wait().await.expect("the server is reaped");
    assert!(!status.success(), "a killed server does not report success");
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

    drop(first.kill().await);
    let _ = first.wait().await.expect("the first server is reaped");
}

/// The hidden completion flag survives the move: `scripts/install.sh` calls
/// it on the binary this crate now ships.
#[tokio::test]
async fn generate_completion_survives_on_the_server_binary() {
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_forge-server"))
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
    assert!(script.contains("#compdef forge"), "a zsh completion for the installed name: {script}");
}
