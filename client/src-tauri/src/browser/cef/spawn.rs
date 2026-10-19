//! The client's half of a host process: spawn it, wait for its DevTools
//! endpoint, record it beside its profile, and end it.
//!
//! **The record is the client's, never the host's.** A host that refused to
//! start (a second one on a held profile) must not overwrite the record of
//! the host that is live; the client knows the child's pid and writes the
//! record only after the endpoint answers.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::osr::Ring;

/// The longest a host is given from spawn to answering `/json/version`.
pub const LAUNCH_TIMEOUT: Duration = Duration::from_secs(15);

/// How often the wait asks the endpoint.
const PROBE_EVERY: Duration = Duration::from_millis(50);

/// A running host, as the client keeps it.
pub struct ActiveHost {
    pub pid: u32,
    pub port: u16,
    pub ring: Arc<Ring>,
    pub profile: PathBuf,
}

impl ActiveHost {
    /// The identity a held driver is built against: a relaunch is a new
    /// process, and the driver rebuilt against the new port follows from it.
    pub fn identity(&self) -> String {
        format!("cef-{}", self.pid)
    }
}

impl std::fmt::Debug for ActiveHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActiveHost")
            .field("pid", &self.pid)
            .field("port", &self.port)
            .field("profile", &self.profile)
            .finish_non_exhaustive()
    }
}

/// What the client records beside a host's cache root so a client that never
/// ran its exit can be reaped by the next boot.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Record {
    pub pid: u32,
    pub port: u16,
    pub ring: String,
}

/// The ring name a host on `port` is given: one name per port, so two hosts
/// on one machine never share pixels.
pub fn ring_name(port: u16) -> String {
    format!("/forge-cef-{port}")
}

/// The record's path, inside the profile's cache root.
pub fn record_path(profile: &Path) -> PathBuf {
    profile.join("host.json")
}

pub fn read_record(profile: &Path) -> Option<Record> {
    let body = std::fs::read(record_path(profile)).ok()?;
    serde_json::from_slice(&body).ok()
}

pub fn write_record(profile: &Path, record: &Record) -> Result<(), String> {
    let body = serde_json::to_vec(record)
        .map_err(|why| format!("the host record cannot be written: {why}"))?;
    std::fs::write(record_path(profile), body)
        .map_err(|why| format!("the host record cannot be written: {why}"))
}

pub fn remove_record(profile: &Path) {
    let _ = std::fs::remove_file(record_path(profile));
}

/// The first port nothing holds, from a small private range.
pub fn free_port() -> Option<u16> {
    (9411..9431_u16).find(|port| std::net::TcpListener::bind(("127.0.0.1", *port)).is_ok())
}

/// Spawn `--cef-host` for one profile, detached from this client's session
/// and with its own log beside the profile. Answers the child's pid.
pub fn spawn(
    profile: &Path,
    port: u16,
    ring: &str,
    width: u32,
    height: u32,
) -> Result<u32, String> {
    let exe = std::env::current_exe()
        .map_err(|why| format!("the client's own executable cannot be found: {why}"))?;
    spawn_with(&exe, profile, port, ring, width, height)
}

/// The spawn itself, with the binary given: the client passes its own
/// executable; a test passes a stub.
pub fn spawn_with(
    exe: &Path,
    profile: &Path,
    port: u16,
    ring: &str,
    width: u32,
    height: u32,
) -> Result<u32, String> {
    std::fs::create_dir_all(profile)
        .map_err(|why| format!("the profile's directory cannot be made: {why}"))?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(profile.join("cef-host.log"))
        .map_err(|why| format!("the host's log cannot be opened: {why}"))?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("--cef-host")
        .arg("--profile")
        .arg(profile)
        .arg("--port")
        .arg(port.to_string())
        .arg("--ring")
        .arg(ring)
        .arg("--size")
        .arg(format!("{width}x{height}"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(log));
    // Detach from the client's session: a terminal hangup must reach neither,
    // and the client reaps its host on its own exit.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // SAFETY: setsid in the child between fork and exec.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    let child = command.spawn().map_err(|why| format!("the host did not start: {why}"))?;
    Ok(child.id())
}

/// Wait, bounded, for a host's endpoint to answer. A process that exits
/// without ever answering is reported by the caller's own look at the child.
pub async fn wait_ready(port: u16) -> Result<(), String> {
    wait_ready_within(port, LAUNCH_TIMEOUT).await
}

pub async fn wait_ready_within(port: u16, bound: Duration) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + bound;
    loop {
        if probe(port).await {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "the browser host on port {port} did not answer within {} s",
                bound.as_secs()
            ));
        }
        tokio::time::sleep(PROBE_EVERY).await;
    }
}

/// The page target's own websocket URL on a host's endpoint, from
/// `/json/list`. A host with no page is `None` - there is nothing to drive.
pub async fn page_websocket(port: u16) -> Option<String> {
    let url = format!("{}/json/list", super::endpoint(port));
    let answer = tokio::task::spawn_blocking(move || http_get(&url)).await.ok().flatten()?;
    let body = answer.split_once("\r\n\r\n")?.1;
    let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
    parsed
        .as_array()?
        .iter()
        .find(|entry| entry.get("type").and_then(serde_json::Value::as_str) == Some("page"))
        .and_then(|entry| entry.get("webSocketDebuggerUrl"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

/// The page target's current URL on a host's endpoint. `None` when nothing
/// answers or no page is open.
pub async fn page_url(port: u16) -> Option<String> {
    let url = format!("{}/json/list", super::endpoint(port));
    let answer = tokio::task::spawn_blocking(move || http_get(&url)).await.ok().flatten()?;
    let body = answer.split_once("\r\n\r\n")?.1;
    let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
    parsed
        .as_array()?
        .iter()
        .find(|entry| entry.get("type").and_then(serde_json::Value::as_str) == Some("page"))
        .and_then(|entry| entry.get("url"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

/// The prefix every websocket URL on `port` starts with.
pub fn ws_prefix(port: u16) -> String {
    format!("ws://127.0.0.1:{port}")
}

/// Whether the endpoint on `port` is a browser host's DevTools endpoint.
pub async fn probe(port: u16) -> bool {
    let url = format!("{}/json/version", super::endpoint(port));
    tokio::task::spawn_blocking(move || http_get(&url))
        .await
        .ok()
        .flatten()
        .is_some_and(|answer| answer.contains("\"Browser\""))
}

/// End a host: ask it to go, then insist.
pub async fn end(pid: u32) {
    signal(pid, libc::SIGTERM);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while running(pid) {
        if tokio::time::Instant::now() >= deadline {
            signal(pid, libc::SIGKILL);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // **Reap what this client spawned.** A killed child stays a zombie, and a
    // zombie answers `kill(pid, 0)` exactly like a live process; the wait is
    // what turns "ended" into a pid nobody can mistake for running. A pid
    // that is not this client's child answers ECHILD and is left alone.
    let until = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        // SAFETY: WNOHANG waitpid on a pid this client may own.
        let reaped = unsafe { libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG) };
        if reaped == pid as i32 || !running(pid) || tokio::time::Instant::now() >= until {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Whether a pid names a live process.
pub fn running(pid: u32) -> bool {
    // SAFETY: signal 0 checks existence without delivering anything.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn signal(pid: u32, what: i32) {
    // SAFETY: a plain signal to a pid this client spawned or recorded.
    unsafe {
        libc::kill(pid as i32, what);
    }
}

/// One HTTP/1.1 GET, read to its own `Content-Length`: the devtools server
/// keeps the connection open even when asked to close, so reading to
/// end-of-stream would time out on a body that had already arrived.
fn http_get(url: &str) -> Option<String> {
    use std::io::{Read as _, Write as _};

    let rest = url.strip_prefix("http://")?;
    let (authority, path) = rest.split_once('/').map_or((rest, ""), |(a, p)| (a, p));
    let mut stream = std::net::TcpStream::connect(authority).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    stream
        .write_all(
            format!("GET /{path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .ok()?;

    let mut answer = Vec::new();
    let mut chunk = [0_u8; 4096];
    let mut wanted = None;
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => answer.extend_from_slice(&chunk[..read]),
            Err(why) if why.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
        if wanted.is_none() {
            wanted = whole_body_len(&answer);
        }
        if wanted.is_some_and(|len| answer.len() >= len) {
            break;
        }
    }
    String::from_utf8(answer).ok()
}

fn whole_body_len(answer: &[u8]) -> Option<usize> {
    let at = answer.windows(4).position(|window| window == b"\r\n\r\n")?;
    let headers = String::from_utf8_lossy(&answer[..at]);
    let length = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().ok())?
    })?;
    Some(at + 4 + length)
}
