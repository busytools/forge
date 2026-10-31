//! The server's half of a host process: spawn it, wait for its DevTools
//! endpoint, record it beside its profile, and end it.
//!
//! **The record is the server's, never the host's.** A host that refused to
//! start (a second one on a held profile) must not overwrite the record of
//! the host that is live; the server knows the child's pid and writes the
//! record only after the endpoint answers.

#![allow(unsafe_code, reason = "setsid, signals and the kernel's process tables are libc calls")]

use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::engine::endpoint;
use crate::osr::Ring;

/// The longest a host is given from spawn to answering `/json/version`.
pub const LAUNCH_TIMEOUT: Duration = Duration::from_secs(15);

/// How long a show cue waits for the host to apply it. The host's loop runs
/// about every millisecond, so this is a stall bound, not a budget: past it
/// the call goes ahead rather than stall on a wedged pump.
pub const SHOW_ACK_BOUND: Duration = Duration::from_millis(100);

/// How often the wait asks the endpoint.
const PROBE_EVERY: Duration = Duration::from_millis(50);

/// One child this server spawned. Its parent-watch write end is kept in
/// [`parent_watches`] for the host's whole life: the host reads EOF only when
/// every copy closes, so dropping it at spawn would read as the server gone.
struct HostChild {
    child: std::process::Child,
}

/// The children this server spawned, so a launch failure is read from the
/// child's own exit rather than a blind timeout, and a pid another process
/// has since reused is never signalled.
fn children() -> &'static std::sync::Mutex<std::collections::HashMap<u32, HostChild>> {
    static CHILDREN: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<u32, HostChild>>,
    > = std::sync::OnceLock::new();
    CHILDREN.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn locked() -> std::sync::MutexGuard<'static, std::collections::HashMap<u32, HostChild>> {
    children().lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Each spawned host's parent-watch write end, keyed by the host's pid. One
/// entry per child in `children()`, released wherever that entry goes.
fn parent_watches()
-> &'static std::sync::Mutex<std::collections::HashMap<u32, std::os::fd::OwnedFd>> {
    static WATCHES: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<u32, std::os::fd::OwnedFd>>,
    > = std::sync::OnceLock::new();
    WATCHES.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn watches() -> std::sync::MutexGuard<'static, std::collections::HashMap<u32, std::os::fd::OwnedFd>>
{
    parent_watches().lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The parent-watch pipe: the read end for the host, the write end this
/// server keeps for the host's whole life. Both start close-on-exec; the
/// spawn clears that only on the copy it dups to the inherited number.
fn open_parent_watch() -> Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd), String> {
    let mut fds = [0_i32; 2];
    // SAFETY: pipe writes two fresh fds into the array.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(format!(
            "the parent watch's pipe cannot be opened: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: both fds are fresh, and this function owns them from here on.
    let read: std::os::fd::OwnedFd = unsafe { std::os::fd::FromRawFd::from_raw_fd(fds[0]) };
    let write: std::os::fd::OwnedFd = unsafe { std::os::fd::FromRawFd::from_raw_fd(fds[1]) };
    for fd in [&read, &write] {
        // SAFETY: fcntl on a live fd this function owns.
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
        if flags < 0
            || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0
        {
            return Err(format!(
                "the parent watch's pipe cannot be closed on exec: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok((read, write))
}

/// Whether `pid` is a child this server spawned and that has not exited.
fn is_ours(pid: u32) -> bool {
    locked().contains_key(&pid)
}

/// Release the write end of `pid`'s parent watch without ending the host, so
/// a test can make the spawning process "go away" for the one resource the
/// host watches. Production never releases it early - the entry holds it for
/// the host's whole life - so this exists only to drive the host's own pipe
/// handling from a test.
#[cfg(any(test, feature = "testing"))]
pub fn drop_parent_watch_for_test(pid: u32) -> bool {
    watches().remove(&pid).is_some()
}

/// A running host, as the server keeps it.
pub struct ActiveHost {
    pub pid: u32,
    pub port: u16,
    pub ring: Arc<Ring>,
    pub profile: PathBuf,
    /// The host's pointer input, one line per gesture.
    pub input: InputSender,
    /// The frame socket's reader, when this host was started with one. It
    /// ends with the host.
    pub reader: Option<crate::channel::Reader>,
    /// The task watching this host's pid, so a death under a viewer is said.
    pub monitor: Option<tokio::task::JoinHandle<()>>,
}

impl ActiveHost {
    /// The identity a held driver is built against: a relaunch is a new
    /// process, and the driver rebuilt against the new port follows from it.
    pub fn identity(&self) -> String {
        format!("cef-{}", self.pid)
    }

    /// Cue the host to re-assert the page visible before a call drives it,
    /// and wait (bounded) for it to be applied. **The wait is load-bearing**:
    /// the host pumps on its own loop, and a cue it has not reached yet would
    /// leave this call running against the hidden page.
    pub async fn show(&self) {
        let seq = self.ring.show();
        let deadline = tokio::time::Instant::now() + SHOW_ACK_BOUND;
        while !self.ring.shown(seq) {
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
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

/// What the server records beside a host's cache root so a boot that never
/// ran its exit can be reaped by the next one.
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

/// One host's input pipe: a line per pointer gesture, in the order the
/// viewer sent them.
pub type InputSender = std::sync::mpsc::Sender<String>;

/// The host's control events, as the server reads them from its stdout.
pub type EventReceiver = tokio::sync::mpsc::UnboundedReceiver<crate::control::HostEvent>;

/// Spawn `--cef-host` for one profile, detached from the server's session
/// and with its own log beside the profile. Answers the child's pid, the
/// pipe its pointer input goes down, and the events it asks the server for.
pub fn spawn(
    profile: &Path,
    port: u16,
    ring: &str,
    width: u32,
    height: u32,
    profile_id: u16,
    encoding: (u16, u8),
    frames: &Path,
) -> Result<(u32, InputSender, EventReceiver), String> {
    #[cfg(test)]
    {
        // **A test drives the launch's failure arms**, which the bundle's
        // real host cannot be made to take on demand.
        let hook = TEST_SPAWN.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
        if let Some(hook) = hook {
            return hook(TestSpawn {
                profile: profile.to_path_buf(),
                port,
                ring: ring.to_owned(),
                width,
                height,
                profile_id,
                encoding,
                frames: frames.to_path_buf(),
            });
        }
    }
    let exe = crate::host_bundle::checked_executable()?;
    spawn_with(&exe, profile, port, ring, width, height, profile_id, encoding, frames)
}

/// The arguments a test's spawn stand-in receives.
#[cfg(test)]
pub(crate) struct TestSpawn {
    pub(crate) profile: std::path::PathBuf,
    pub(crate) port: u16,
    pub(crate) ring: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) profile_id: u16,
    pub(crate) encoding: (u16, u8),
    pub(crate) frames: std::path::PathBuf,
}

/// One test's replacement for `spawn`.
#[cfg(test)]
type TestSpawnHook =
    Box<dyn FnOnce(TestSpawn) -> Result<(u32, InputSender, EventReceiver), String> + Send>;

/// The next spawn a test has replaced, taken once.
#[cfg(test)]
static TEST_SPAWN: std::sync::Mutex<Option<TestSpawnHook>> = std::sync::Mutex::new(None);

/// Replace the next `spawn` a test drives, so `start_host`'s failure arms
/// can be reached with a stand-in host.
#[cfg(test)]
pub(crate) fn set_test_spawn(hook: TestSpawnHook) {
    *TEST_SPAWN.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(hook);
}

/// The fd the host reads the parent watch on: the read end is `dup2`'d here
/// between fork and exec, so the number is fixed rather than passed through.
const PARENT_WATCH_FD: std::os::fd::RawFd = 3;

/// The host's command line, without the pipes or the detachment: the spawn
/// adds those, and a test reads the arguments a real launch carries.
fn host_command(
    exe: &Path,
    profile: &Path,
    port: u16,
    ring: &str,
    width: u32,
    height: u32,
    profile_id: u16,
    encoding: (u16, u8),
    frames: &Path,
) -> std::process::Command {
    let mut command = std::process::Command::new(exe);
    command
        .arg("--cef-host")
        .arg("--profile")
        .arg(profile)
        .arg("--port")
        .arg(port.to_string())
        .arg("--ring")
        .arg(ring)
        .arg("--frames")
        .arg(frames)
        .arg("--size")
        .arg(format!("{width}x{height}"))
        .arg("--profile-id")
        .arg(profile_id.to_string())
        .arg("--fps")
        .arg(encoding.0.to_string())
        .arg("--quality")
        .arg(encoding.1.to_string())
        // **The host outlives an orderly server and not a killed one.** The
        // read end is the identity: EOF means the server's process - and
        // every copy of the write end it holds - is gone, where a pid could
        // be recycled or belong to an unreaped zombie (#2499).
        .arg("--parent-pipe")
        .arg(PARENT_WATCH_FD.to_string());
    command
}

/// Hand `read` to the exec'd child as [`PARENT_WATCH_FD`], with close-on-exec
/// off on that copy.
///
/// **A self-dup is not enough.** When `read` already is the target fd,
/// `dup2` returns it unchanged and POSIX leaves the close-on-exec bit set, so
/// the watch would close at exec and the host would read `EBADF` and call the
/// server gone at its first interval. Clear the bit explicitly.
fn inherit_parent_watch(read: std::os::fd::RawFd) -> std::io::Result<()> {
    // SAFETY: dup2 and fcntl in the child between fork and exec, on fds this
    // process owns.
    unsafe {
        if libc::dup2(read, PARENT_WATCH_FD) < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let flags = libc::fcntl(PARENT_WATCH_FD, libc::F_GETFD);
        if flags < 0 || libc::fcntl(PARENT_WATCH_FD, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// The spawn itself, with the binary given: the server passes the bundle's
/// executable; a test passes a stand-in. `encoding` is the `[browser]` pair:
/// frames per second and lossy quality, and `frames` the socket the server
/// has bound (a short path; the host would otherwise derive one too long).
pub fn spawn_with(
    exe: &Path,
    profile: &Path,
    port: u16,
    ring: &str,
    width: u32,
    height: u32,
    profile_id: u16,
    encoding: (u16, u8),
    frames: &Path,
) -> Result<(u32, InputSender, EventReceiver), String> {
    std::fs::create_dir_all(profile)
        .map_err(|why| format!("the profile's directory cannot be made: {why}"))?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(host_log_path(profile))
        .map_err(|why| format!("the host's log cannot be opened: {why}"))?;
    // **The parent watch's pipe.** The write end stays in this process for
    // the host's life; the read end is handed across exec as fd 3, so the
    // host reads EOF exactly when this server's process ends, where a pid
    // could be recycled or belong to an unreaped zombie (#2499).
    let (parent_read, parent_write) = open_parent_watch()?;
    let parent_read_fd = parent_read.as_raw_fd();
    let mut command =
        host_command(exe, profile, port, ring, width, height, profile_id, encoding, frames);
    command
        // **Pointer input rides this pipe.** CEF's own mouse calls are what
        // route into a windowless popup, so the host's gestures come down
        // its stdin rather than over the page's DevTools target.
        .stdin(std::process::Stdio::piped())
        // **And its control events ride this one back**: a page's file
        // chooser has to reach a person, and the host has no socket of its
        // own for it.
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::from(log));
    // Detach from the server's session: a terminal hangup must reach neither,
    // and the server reaps its host on its own exit.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // SAFETY: setsid and dup2 in the child between fork and exec.
        unsafe {
            command.pre_exec(move || {
                libc::setsid();
                inherit_parent_watch(parent_read_fd)
            });
        }
    }
    let mut child = command.spawn().map_err(|why| format!("the host did not start: {why}"))?;
    // The host has its own copy of the read end now; dropping this one leaves
    // the host's read end and the write end held below as the only copies
    // keeping the pipe alive.
    drop(parent_read);
    let pid = child.id();
    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("the host's input pipe was not opened".to_owned());
    };
    let (input, lines) = std::sync::mpsc::channel::<String>();
    let writer = std::thread::Builder::new().name(format!("cef-input-{pid}")).spawn(move || {
        use std::io::Write as _;
        for line in lines {
            // **A host that is gone ends the writer**, so a send after its
            // death fails rather than queueing against a closed pipe.
            if writeln!(stdin, "{line}").is_err() {
                break;
            }
        }
    });
    if let Err(why) = writer {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("the host's input writer did not start: {why}"));
    }
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("the host's event pipe was not opened".to_owned());
    };
    let (events, event_rx) = tokio::sync::mpsc::unbounded_channel();
    let reader = std::thread::Builder::new().name(format!("cef-events-{pid}")).spawn(move || {
        use std::io::BufRead as _;
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            let Some(event) = crate::control::parse(&line) else { continue };
            if events.send(event).is_err() {
                break;
            }
        }
    });
    if let Err(why) = reader {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("the host's event reader did not start: {why}"));
    }
    locked().insert(pid, HostChild { child });
    watches().insert(pid, parent_write);
    Ok((pid, input, event_rx))
}

/// Wait for a host this server spawned, failing as soon as the child exits
/// rather than after the blind timeout. The caller records the host log's
/// tail in `browser_host_ended`, which is what a refused launch is diagnosed
/// by.
///
/// **The endpoint answering is not proof the endpoint is ours.** The port is
/// probed free and then bound by the child, and another host can take it in
/// between; adopting its endpoint would drive another profile's page. The
/// ring closes that: only this pid writes its pid and `STATE_LIVE` into it,
/// so the probe is trusted only once the ring says so. A child that cannot
/// own the port never reads live, and the launch fails instead of crossing.
pub async fn wait_ready_for(pid: u32, port: u16, ring: &str) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + LAUNCH_TIMEOUT;
    loop {
        let live = owns(ring, pid);
        if live && probe(port).await {
            return Ok(());
        }
        if !alive(pid) {
            return Err("the browser host exited before its endpoint answered".to_owned());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "the browser host on port {port} did not answer within {} s",
                LAUNCH_TIMEOUT.as_secs()
            ));
        }
        tokio::time::sleep(PROBE_EVERY).await;
    }
}

/// Whether the ring at `name` is `pid`'s own live host's.
fn owns(name: &str, pid: u32) -> bool {
    crate::osr::Ring::open(name)
        .is_ok_and(|mapped| mapped.pid() == pid && mapped.state() == crate::osr::STATE_LIVE)
}

/// How much of the host log a tail keeps: the last few KiB, at most four
/// lines, and a bounded prefix of each. **The log is append-only and never
/// rotated**, so a host that has run for months must not make a record
/// unbounded.
const TAIL_BYTES: u64 = 4 * 1024;
const TAIL_LINES: usize = 4;
const TAIL_CHARS: usize = 512;

/// The log a profile's host writes, where the spawn opens it and the readers
/// find it.
pub fn host_log_path(profile: &Path) -> PathBuf {
    profile.join("cef-host.log")
}

/// The host log's last few lines, read from the file's own end. A seek can
/// land mid-line, so the first kept line may be a fragment.
///
/// **Public because the live test support reads a failed test's host log the
/// same way**, and a second reader would drift from this one's bounds.
pub fn log_tail(log: &Path) -> String {
    use std::io::{Read as _, Seek as _};
    let Ok(mut file) = std::fs::File::open(log) else {
        return String::new();
    };
    let len = file.metadata().map_or(0, |meta| meta.len());
    if len > TAIL_BYTES {
        let _ = file.seek(std::io::SeekFrom::Start(len - TAIL_BYTES));
    }
    let mut buf = Vec::with_capacity(usize::try_from(TAIL_BYTES).unwrap_or(usize::MAX));
    // A seek can land inside a multibyte character, so the tail is read
    // lossily rather than refused.
    if (&mut file).take(TAIL_BYTES).read_to_end(&mut buf).is_err() {
        return String::new();
    }
    let body = String::from_utf8_lossy(&buf);
    let lines: Vec<&str> = body.lines().filter(|line| !line.trim().is_empty()).collect();
    let tail: Vec<&str> = lines[lines.len().saturating_sub(TAIL_LINES)..]
        .iter()
        .map(|line| {
            let cut = line.char_indices().nth(TAIL_CHARS).map_or(line.len(), |(at, _)| at);
            &line[..cut]
        })
        .collect();
    tail.join(" | ")
}

/// Whether a pid names a process this server started that is still running.
/// A child that exited is reaped here, so a zombie never reads as alive.
pub fn alive(pid: u32) -> bool {
    exit(pid).is_none()
}

/// Whether `pid` has exited, **without taking its status**: a later
/// [`exit`] still reads the cause. This is the liveness check for a path
/// that leaves the death to the monitor.
pub fn gone(pid: u32) -> bool {
    let mut held = locked();
    match held.get_mut(&pid) {
        Some(child) => match child.child.try_wait() {
            Ok(status) => status.is_some(),
            // `exit` treats a status it cannot read as gone; the two
            // liveness answers must agree.
            Err(_) => !running(pid),
        },
        None => !running(pid),
    }
}

/// A child's exit, when it went: `None` while it still runs. The status is
/// the cause a death line carries; a pid this process did not start has no
/// status of its own to read, and reports `Some(None)` once it is gone.
pub fn exit(pid: u32) -> Option<Option<std::process::ExitStatus>> {
    let mut held = locked();
    let waited = match held.get_mut(&pid) {
        Some(child) => child.child.try_wait(),
        None => return (!running(pid)).then_some(None),
    };
    match waited {
        Ok(Some(status)) => {
            held.remove(&pid);
            drop(watches().remove(&pid));
            Some(Some(status))
        }
        Ok(None) => None,
        Err(_) => {
            held.remove(&pid);
            drop(watches().remove(&pid));
            (!running(pid)).then_some(None)
        }
    }
}

/// The page target's own URL on a host's endpoint, from `/json/list`. A host
/// with no page is `None` - there is nothing to carry.
pub async fn page_url(port: u16) -> Option<String> {
    let url = format!("{}/json/list", endpoint(port));
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

/// Every page target on a host's endpoint, as (target id, url) pairs, from
/// `/json/list`. The browser tools diff this around a creation to learn the
/// tab they just opened.
pub async fn page_targets(port: u16) -> Vec<(String, String)> {
    let url = format!("{}/json/list", endpoint(port));
    let Some(answer) = tokio::task::spawn_blocking(move || http_get(&url)).await.ok().flatten()
    else {
        return Vec::new();
    };
    let Some(body) = answer.split_once("\r\n\r\n").map(|(_, body)| body) else {
        return Vec::new();
    };
    let Some(entries) = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|parsed| parsed.as_array().cloned())
    else {
        return Vec::new();
    };
    entries
        .iter()
        .filter(|entry| entry.get("type").and_then(serde_json::Value::as_str) == Some("page"))
        .filter_map(|entry| {
            Some((entry.get("id")?.as_str()?.to_owned(), entry.get("url")?.as_str()?.to_owned()))
        })
        .collect()
}

/// Close one target through the endpoint's own close door.
pub async fn close_target(port: u16, target: &str) -> bool {
    let url = format!("{}/json/close/{target}", endpoint(port));
    tokio::task::spawn_blocking(move || http_get(&url)).await.ok().flatten().is_some()
}

/// The page target's own websocket URL on a host's endpoint, from
/// `/json/list`. A host with no page is `None` - there is nothing to drive.
pub async fn page_websocket(port: u16) -> Option<String> {
    let url = format!("{}/json/list", endpoint(port));
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

/// One named page target's own websocket URL on a host's endpoint, from
/// `/json/list`.
pub async fn page_websocket_of(port: u16, target: &str) -> Option<String> {
    let url = format!("{}/json/list", endpoint(port));
    let answer = tokio::task::spawn_blocking(move || http_get(&url)).await.ok().flatten()?;
    let body = answer.split_once("\r\n\r\n")?.1;
    let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
    parsed
        .as_array()?
        .iter()
        .find(|entry| entry.get("id").and_then(serde_json::Value::as_str) == Some(target))
        .and_then(|entry| entry.get("webSocketDebuggerUrl"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

/// The browser's own DevTools endpoint, when it answers.
///
/// **The browser target, not a page target**: its sessions are what reach
/// every page the profile creates.
pub async fn browser_websocket(port: u16) -> Option<String> {
    let url = format!("{}/json/version", endpoint(port));
    let answer = tokio::task::spawn_blocking(move || http_get(&url)).await.ok().flatten()?;
    let body = answer.split_once("\r\n\r\n")?.1;
    let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
    parsed.get("webSocketDebuggerUrl").and_then(serde_json::Value::as_str).map(str::to_owned)
}

/// The prefix every websocket URL on `port` starts with.
pub fn ws_prefix(port: u16) -> String {
    format!("ws://127.0.0.1:{port}")
}

/// Whether the endpoint on `port` is a browser host's DevTools endpoint.
pub async fn probe(port: u16) -> bool {
    let url = format!("{}/json/version", endpoint(port));
    tokio::task::spawn_blocking(move || http_get(&url))
        .await
        .ok()
        .flatten()
        .is_some_and(|answer| answer.contains("\"Browser\""))
}

/// End a host this server spawned, or one still answering on `port`.
///
/// **A recorded pid is only a number**: another process may have been given
/// it since. The signal goes only when the endpoint still answers as a
/// browser host, or when the pid is one of this server's own children.
pub async fn end_verified(pid: u32, port: u16, profile: &Path) -> bool {
    if is_ours(pid) {
        end(pid).await;
        return true;
    }
    // **A recorded pid is a number until its owner is identified.** The
    // endpoint answering proves a host is on the port; it does not prove this
    // pid is it. Only a process that is this bundle's `--cef-host` for THIS
    // profile and port is signalled, checked again after the probe's await -
    // the window the first check cannot cover - immediately before `end`,
    // which signals with no await in between.
    if !is_our_host(pid, profile, Some(port)) {
        // Nothing answers: the record is stale and its ring is ours to clean.
        // Something answers: it is not this pid's host, so neither the pid nor
        // the ring is this sweep's to touch.
        return !probe(port).await;
    }
    if !probe(port).await {
        return true;
    }
    if !is_our_host(pid, profile, Some(port)) {
        return !probe(port).await;
    }
    end(pid).await;
    true
}

/// End a host: ask it to go, then insist.
pub async fn end(pid: u32) {
    if !is_ours(pid) && !running(pid) {
        return;
    }
    signal(pid, libc::SIGTERM);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while alive(pid) {
        if tokio::time::Instant::now() >= deadline {
            signal(pid, libc::SIGKILL);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // **Reap what this server spawned.** A killed child stays a zombie, and a
    // zombie answers `kill(pid, 0)` exactly like a live process; the wait is
    // what turns "ended" into a pid nobody can mistake for running. A pid
    // that is not this server's child has no handle here and is left to its
    // own parent.
    if let Some(mut child) = locked().remove(&pid) {
        let _ = child.child.wait();
    }
    drop(watches().remove(&pid));
}

/// The pid a profile's CEF singleton lock names, when it carries one.
///
/// **The record is not the only trail to a host.** Chromium writes
/// `SingletonLock` as a `<hostname>-<pid>` symlink; a host whose record was
/// lost or removed still leaves this, and a host that was SIGKILLed leaves
/// it stale. The sweep reads it after the record's own path has run.
pub fn singleton_pid(profile: &Path) -> Option<u32> {
    let target = std::fs::read_link(profile.join("SingletonLock")).ok()?;
    let text = target.to_string_lossy().into_owned();
    let (_, pid) = text.rsplit_once('-')?;
    pid.parse().ok()
}

/// The value one `--flag value` pair carries in an argument list.
fn arg_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let at = args.iter().position(|arg| arg == flag)?;
    args.get(at + 1).map(String::as_str)
}

/// Kill a host an unclean exit left behind, and clear its trail.
///
/// **The record names it; Chromium's own singleton lock is the fallback** for
/// a host whose record was lost. Nothing is signalled unless the process is
/// this bundle's `--cef-host` for this profile - a recycled pid is not a
/// host - and the record, the lock and the ring go either way, so the next
/// boot does not trip over them.
pub async fn reap_stale_profile(profile: &Path) {
    let record = read_record(profile);
    let singleton = singleton_pid(profile);
    // **Only a host THIS process started for THIS profile is not an
    // orphan.** The registry alone would skip a recycled pid that is another
    // profile's host this process started; `is_our_host` alone would skip a
    // host a crashed earlier forge left behind, because its path and argv
    // still match - and that orphan is exactly what the record branch below
    // exists to end. Both must hold.
    if record.as_ref().is_some_and(|record| {
        is_ours(record.pid) && is_our_host(record.pid, profile, Some(record.port))
    }) || singleton.is_some_and(is_ours)
    {
        return;
    }
    if let Some(record) = record.as_ref()
        && is_our_host(record.pid, profile, Some(record.port))
    {
        end_verified(record.pid, record.port, profile).await;
        tracing::info!(
            event_name = "browser_stale_host_reaped",
            pid = record.pid,
            port = record.port,
            profile = %profile.display(),
            "a browser host an unclean exit left behind was ended"
        );
    }
    if let Some(pid) = singleton
        && is_our_host(pid, profile, None)
    {
        end(pid).await;
        tracing::info!(
            event_name = "browser_stale_host_reaped",
            pid,
            profile = %profile.display(),
            "a browser host named only by the singleton lock was ended"
        );
    }
    if record.is_none() && singleton.is_none() {
        return;
    }
    if let Some(record) = record {
        Ring::unlink(&record.ring);
    }
    remove_record(profile);
    clear_singleton(profile);
}

/// Whether `pid` is the host of THIS profile: the bundle's executable, in
/// `--cef-host` mode, naming `profile` - and, when a port is given, that
/// port.
///
/// **The mode is not the identity either.** Another profile's host all runs
/// the very same executable; only the `--profile` it names says which
/// browser it is. A reaper that skipped this would end whichever host
/// happened to hold the pid.
pub fn is_our_host(pid: u32, profile: &Path, port: Option<u16>) -> bool {
    if !is_our_binary(pid) {
        return false;
    }
    let Some(args) = process_argv(pid) else {
        return false;
    };
    host_matches(&args, profile, port)
}

/// Whether one host's argument list names THIS profile (and port).
fn host_matches(args: &[String], profile: &Path, port: Option<u16>) -> bool {
    if !args.iter().any(|arg| arg == "--cef-host") {
        return false;
    }
    let Some(named) = arg_value(args, "--profile") else {
        return false;
    };
    let named = Path::new(named);
    let same_profile = named == profile
        || std::fs::canonicalize(named)
            .ok()
            .zip(std::fs::canonicalize(profile).ok())
            .is_some_and(|(named, mine)| named == mine);
    if !same_profile {
        return false;
    }
    match port {
        Some(port) => arg_value(args, "--port").and_then(|value| value.parse().ok()) == Some(port),
        None => true,
    }
}

/// One process's arguments, from the kernel's own `KERN_PROCARGS2`.
fn process_argv(pid: u32) -> Option<Vec<String>> {
    if pid == 0 {
        return None;
    }
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, i32::try_from(pid).unwrap_or(i32::MAX)];
    let mut size = 0_usize;
    // SAFETY: a size probe against the kernel's own mib.
    let probed = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            std::ptr::addr_of_mut!(size),
            std::ptr::null_mut(),
            0,
        )
    };
    if probed != 0 || size <= 4 {
        return None;
    }
    let mut buffer = vec![0_u8; size];
    let mut read = buffer.len();
    // SAFETY: `buffer` is writable for `read` bytes.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buffer.as_mut_ptr().cast(),
            std::ptr::addr_of_mut!(read),
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || read < 4 {
        return None;
    }
    buffer.truncate(read);
    let count = i32::from_ne_bytes(buffer[0..4].try_into().ok()?);
    let argc = usize::try_from(count).unwrap_or(0);
    let mut at = 4;
    // The executable's path follows argc; padding NULs separate it from argv.
    while at < buffer.len() && buffer[at] != 0 {
        at += 1;
    }
    while at < buffer.len() && buffer[at] == 0 {
        at += 1;
    }
    let mut values = Vec::new();
    for _ in 0..argc.min(1024) {
        if at >= buffer.len() {
            break;
        }
        let start = at;
        while at < buffer.len() && buffer[at] != 0 {
            at += 1;
        }
        values.push(String::from_utf8_lossy(&buffer[start..at]).into_owned());
        at += 1;
    }
    Some(values)
}

/// The directory Chromium's code-sign clone manager copies our signed bundle
/// into on some launches (its `kMacAppCodeSignClone` feature), keyed on the
/// bundle identifier the staging script signs.
#[cfg(target_os = "macos")]
const CODE_SIGN_CLONE: &str = "dev.vedhavyas.forge.browser-host.code_sign_clone";

/// Whether `path` is a code-sign clone of this host's own bundle. **The clone
/// is our host**: Chromium re-execs the browser process from the copy, and a
/// reaper that refused it would leave the profile's lock held.
#[cfg(target_os = "macos")]
fn is_code_sign_clone(path: &std::path::Path, mine: &std::path::Path) -> bool {
    path.file_name() == mine.file_name()
        && path.components().any(|part| part.as_os_str() == CODE_SIGN_CLONE)
}

/// Whether `pid` runs the host executable this server ships, so a reaper
/// never signals an unrelated process that reused a recorded number.
pub fn is_our_binary(pid: u32) -> bool {
    let Ok(mine) = crate::host_bundle::executable() else {
        return false;
    };
    #[cfg(target_os = "macos")]
    {
        let mut buffer = [0_i8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: `proc_pidpath` writes at most the buffer's length.
        let wrote = unsafe {
            libc::proc_pidpath(
                i32::try_from(pid).unwrap_or(i32::MAX),
                buffer.as_mut_ptr().cast(),
                u32::try_from(buffer.len()).unwrap_or(u32::MAX),
            )
        };
        if wrote <= 0 {
            return false;
        }
        // SAFETY: a successful call NUL-terminates the path in the buffer.
        let path = unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) };
        let text = path.to_string_lossy();
        let path = std::path::Path::new(text.as_ref());
        path == mine || is_code_sign_clone(path, &mine)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = pid;
        false
    }
}

/// Remove the singleton files a dead host left behind.
pub fn clear_singleton(profile: &Path) {
    for name in ["SingletonLock", "SingletonSocket", "SingletonCookie"] {
        let _ = std::fs::remove_file(profile.join(name));
    }
}

/// Whether a pid names a live process.
///
/// **Only for a host this process did not spawn.** A host's watch on its
/// server is the inherited pipe it reads (#2499). A host adopted from a
/// crashed process's record has no pipe this process holds, so this is the
/// one branch with only the process table to ask. Every pid this process
/// spawned is answered from its `Child` handle instead.
pub fn running(pid: u32) -> bool {
    // SAFETY: signal 0 checks existence without delivering anything.
    unsafe { libc::kill(i32::try_from(pid).unwrap_or(i32::MAX), 0) == 0 }
}

fn signal(pid: u32, what: i32) {
    // SAFETY: a plain signal to a pid this server spawned or recorded.
    unsafe {
        libc::kill(i32::try_from(pid).unwrap_or(i32::MAX), what);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::sync::atomic::Ordering;

    /// **A self-dup keeps close-on-exec set unless it is cleared.** When the
    /// pipe's read end already sits on the host's number, `dup2(3, 3)` is a
    /// no-op and POSIX leaves `FD_CLOEXEC` on it: the exec then closes the
    /// watch and the host reads `EBADF` as "server gone" at its first
    /// interval. The child here reports `fcntl(3, F_GETFD)` after the
    /// inheritance step, so the assertion is about the bit and not about the
    /// whole spawn succeeding.
    #[test]
    fn a_self_dup_still_clears_close_on_exec() {
        let mut fds = [0_i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "the test's pipe opens");
        let (read, _write): (std::os::fd::OwnedFd, std::os::fd::OwnedFd) = unsafe {
            (
                std::os::fd::FromRawFd::from_raw_fd(fds[0]),
                std::os::fd::FromRawFd::from_raw_fd(fds[1]),
            )
        };
        let read_fd = read.as_raw_fd();
        let probe = format!(
            "import fcntl\n\
             try:\n\
             \x20   print(fcntl.fcntl({PARENT_WATCH_FD}, fcntl.F_GETFD))\n\
             except OSError as error:\n\
             \x20   print('closed', error.errno)\n"
        );
        let mut command = std::process::Command::new("python3");
        command.arg("-c").arg(probe).stdout(std::process::Stdio::piped());
        // SAFETY: the child arranges the production state (the read end on
        // the watch's number with close-on-exec set) and then runs the same
        // inheritance step the spawn runs.
        unsafe {
            use std::os::unix::process::CommandExt as _;
            command.pre_exec(move || {
                if libc::dup2(read_fd, PARENT_WATCH_FD) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::fcntl(PARENT_WATCH_FD, libc::F_SETFD, libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                inherit_parent_watch(PARENT_WATCH_FD)
            });
        }
        let output = command.output().expect("the probe child runs");
        assert!(
            output.status.success(),
            "the child exits cleanly; stderr {:?}",
            String::from_utf8_lossy(&output.stderr),
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "0",
            "fd {PARENT_WATCH_FD} survives the exec with no close-on-exec bit",
        );
    }

    /// **A ring is its writer's, and only while it reads live.** An endpoint
    /// that answers on the port proves nothing about who owns it; the pid and
    /// state the host wrote are what close the gap between the free-port
    /// probe and the child's bind.
    #[test]
    fn a_ring_belongs_to_the_live_pid_that_wrote_it() {
        let name = format!("/forge-cef-test-own-{}", std::process::id());
        let ring = Ring::create(&name, 4, 4).expect("a test ring");
        ring.set_pid(4242);
        assert!(!owns(&name, 4242), "a starting host is not ready");
        ring.set_state(crate::osr::STATE_LIVE);
        assert!(owns(&name, 4242), "the pid that wrote it owns the live ring");
        assert!(!owns(&name, 4243), "another pid's ring is not ours");
        ring.set_state(crate::osr::STATE_FAILED);
        assert!(!owns(&name, 4242), "a failed host is not ready either");
        assert!(!owns("/forge-cef-test-absent", 4242), "a ring nobody wrote is not ours");
        Ring::unlink(&name);
    }

    /// **A stale record is cleared and a foreign pid is never signalled.**
    /// The record here names THIS test process, which is not a browser host;
    /// the reaper must take the trail away and leave the process alone.
    #[tokio::test]
    async fn a_stale_record_is_cleared_without_signalling_a_foreign_pid() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let profile = dir.path().join("profile");
        std::fs::create_dir_all(&profile).expect("the profile dir");
        let me = std::process::id();
        write_record(
            &profile,
            &Record { pid: me, port: 9419, ring: "/forge-test-ring".to_owned() },
        )
        .expect("the record");
        std::os::unix::fs::symlink(format!("host-{me}"), profile.join("SingletonLock"))
            .expect("the lock");

        reap_stale_profile(&profile).await;

        assert!(read_record(&profile).is_none(), "the record is cleared");
        assert!(singleton_pid(&profile).is_none(), "and the lock with it");
        assert!(running(me), "a pid that is not this profile's host is left alone");
    }

    /// **A cue is not one-way.** The server waits for the host to apply it
    /// before driving the call; the applier here is the host loop's own
    /// store, stood up as its own task so a missing wait shows as an
    /// unacknowledged cue.
    #[tokio::test]
    async fn a_show_cue_waits_for_the_host_to_apply_it() {
        let name = "/forge-cef-test-show-wait";
        let ring = Arc::new(Ring::create(name, 4, 4).expect("a test ring"));
        let (input, _keep) = std::sync::mpsc::channel();
        let host = ActiveHost {
            pid: 999_999,
            port: 1,
            ring: Arc::clone(&ring),
            profile: PathBuf::from("/nonexistent"),
            input,
            reader: None,
            monitor: None,
        };
        let applier = Arc::clone(&ring);
        tokio::spawn(async move {
            loop {
                // SAFETY: the ring outlives this task and holds a header.
                let seq = unsafe { (*applier.header()).show_seq.load(Ordering::Acquire) };
                if seq > 0 {
                    // SAFETY: as above.
                    unsafe { (*applier.header()).show_applied.store(seq, Ordering::Release) };
                    break;
                }
                tokio::task::yield_now().await;
            }
        });
        host.show().await;
        assert!(ring.shown(1), "the cue was applied before show returned");
        Ring::unlink(name);
    }

    /// **The wait is bounded.** A host that never applies the cue must not
    /// hold a call forever: the server goes ahead after the bound.
    #[tokio::test]
    async fn a_show_cue_gives_up_after_its_bound() {
        let name = "/forge-cef-test-show-bound";
        let ring = Arc::new(Ring::create(name, 4, 4).expect("a test ring"));
        let (input, _keep) = std::sync::mpsc::channel();
        let host = ActiveHost {
            pid: 999_999,
            port: 1,
            ring: Arc::clone(&ring),
            profile: PathBuf::from("/nonexistent"),
            input,
            reader: None,
            monitor: None,
        };
        let started = std::time::Instant::now();
        host.show().await;
        let waited = started.elapsed();
        assert!(
            waited >= SHOW_ACK_BOUND.saturating_sub(Duration::from_millis(10)),
            "the wait ran to its bound: {waited:?}"
        );
        // A generous ceiling: the property pinned above is the bound itself,
        // and a tight wall-clock ceiling flakes when the machine is loaded.
        assert!(waited < Duration::from_secs(5), "and returned rather than running on: {waited:?}");
        assert!(!ring.shown(1), "nothing acknowledged an unapplied cue");
        Ring::unlink(name);
    }

    /// **The singleton lock names the host the record lost.** A profile
    /// whose record is gone is still reaped from Chromium's own symlink.
    /// **Another profile's host is not ours**, even on the same binary and
    /// in `--cef-host` mode: the `--profile` it names is the identity, and
    /// the port narrows it further.
    #[test]
    fn a_host_is_matched_by_its_profile_and_port() {
        let args: Vec<String> = [
            "pattern",
            "--cef-host",
            "--profile",
            "/profiles/hunt/cef",
            "--port",
            "9412",
            "--ring",
            "/forge-cef-9412",
        ]
        .map(str::to_owned)
        .to_vec();
        assert!(host_matches(&args, Path::new("/profiles/hunt/cef"), Some(9412)));
        assert!(
            !host_matches(&args, Path::new("/profiles/other/cef"), Some(9412)),
            "another profile's host is not ours"
        );
        assert!(
            !host_matches(&args, Path::new("/profiles/hunt/cef"), Some(9411)),
            "another port is not ours"
        );
        assert!(
            !host_matches(
                &["pattern", "--profile", "/p"].map(str::to_owned),
                Path::new("/p"),
                None
            ),
            "no host mode, no host"
        );
    }

    /// **A singleton lock names its pid; the test process is not a host.**
    /// Only the bundle's own executable is a host binary, so a record that
    /// points at any other process is never signalled by that path.
    /// **A code-sign clone of our bundle is still our host.** Chromium
    /// re-execs the browser process from the copy, so a reaper that only
    /// matched the installed path would leave that profile's lock held.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_code_sign_clone_of_the_host_is_ours() {
        let mine = std::path::Path::new(
            "/Users/x/Library/Application Support/forge/browser-host/Forge Browser.app/Contents/MacOS/Forge Browser",
        );
        let clone = std::path::Path::new(
            "/var/folders/x/dev.vedhavyas.forge.browser-host.code_sign_clone/code_sign_clone.ABC/Forge Browser.app.bundle/Contents/MacOS/Forge Browser",
        );
        assert!(is_code_sign_clone(clone, mine), "the clone is our own host");
        assert!(
            !is_code_sign_clone(
                std::path::Path::new("/tmp/Other.app/Contents/MacOS/Forge Browser"),
                mine
            ),
            "a same-named binary outside the clone directory is not ours"
        );
        assert!(
            !is_code_sign_clone(
                std::path::Path::new(
                    "/tmp/dev.vedhavyas.forge.browser-host.code_sign_clone/code_sign_clone.ABC/Other"
                ),
                mine
            ),
            "a clone directory holding another binary is not ours"
        );
    }

    #[test]
    fn a_singleton_lock_names_its_pid_and_only_the_host_binary_is_ours() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let profile = dir.path();
        assert_eq!(singleton_pid(profile), None, "no lock, no pid");
        std::os::unix::fs::symlink("mac-studio-4242", profile.join("SingletonLock"))
            .expect("the lock");
        assert_eq!(singleton_pid(profile), Some(4242));
        assert!(!is_our_binary(std::process::id()), "the test binary is not the host");
        assert!(
            !is_our_host(std::process::id(), Path::new("/tmp/forge-no-such-profile"), None),
            "the same binary is not a host without --cef-host in its argv"
        );
        assert!(!is_our_binary(1), "launchd is not");
        clear_singleton(profile);
        assert_eq!(singleton_pid(profile), None, "and the lock is gone");
    }

    /// **The launch line hands the host a watch pipe, not a pid.** A SIGKILL
    /// runs no server exit path, and EOF on the read end is tied to the
    /// server's process where a pid could be recycled or be an unreaped
    /// zombie (#2499).
    #[test]
    fn the_launch_line_hands_the_host_a_parent_watch_pipe() {
        let command = host_command(
            Path::new("/tmp/Forge Browser"),
            Path::new("/tmp/forge-profile/cef"),
            9411,
            "/forge-ring",
            1440,
            900,
            1,
            (30, 75),
            Path::new("/tmp/forge-frames.sock"),
        );
        let args: Vec<&std::ffi::OsStr> = command.get_args().collect();
        let at = args
            .iter()
            .position(|arg| *arg == "--parent-pipe")
            .expect("the watch flag is on the line");
        assert_eq!(
            args.get(at + 1).map(|fd| fd.to_string_lossy().into_owned()),
            Some(PARENT_WATCH_FD.to_string()),
            "the read end is dup'd to the fixed fd the host reads"
        );
    }

    /// **A recycled pid is never signalled.** This test's own pid stands in
    /// for a record whose process has since been reused: no endpoint answers
    /// on the port, so `end_verified` must leave it alone. The regression is
    /// fatal to the test run, which is the point - the caller would have
    /// killed an unrelated process.
    #[tokio::test]
    async fn a_pid_is_not_signalled_when_its_endpoint_is_silent() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("a free port");
        let port = listener.local_addr().expect("the bound address").port();
        drop(listener);
        end_verified(std::process::id(), port, Path::new("/tmp/forge-no-such-profile")).await;
        assert!(alive(std::process::id()), "the unrelated process is still running");
    }

    /// **A window that opens inside a character still reads the tail.** The
    /// host log carries whatever a page printed, and refusing the window would
    /// blank the record for exactly the logs that hold one.
    #[test]
    fn a_tail_read_from_inside_a_character_is_lossy_not_empty() {
        let dir = tempfile::tempdir().expect("a temp dir");
        // 6100 bytes: the 4 KiB window opens two bytes into a three-byte char.
        // **The first-character assertion is what keeps that true**: move
        // `TAIL_BYTES` and the window lands on a boundary, which fails here
        // rather than passing vacuously.
        let body = format!("{}{}", "a".repeat(100), "\u{20ac}".repeat(2_000));
        let log = dir.path().join("cef-host.log");
        std::fs::write(&log, body).expect("the host log");

        let tail = log_tail(&log);
        assert!(!tail.is_empty(), "the window was refused instead of read lossily");
        assert!(tail.starts_with('\u{FFFD}'), "the window did not open inside a character: {tail}");
        assert!(tail.contains('\u{20ac}'), "the tail lost the log's own text: {tail}");
    }

    /// **The tail is bounded by bytes, lines and characters.** The host log is
    /// append-only and never rotated, so a full read would grow with the
    /// host's uptime and a long line would grow the record with it.
    #[test]
    fn the_log_tail_keeps_the_last_four_lines_and_caps_each() {
        use std::fmt::Write as _;

        let dir = tempfile::tempdir().expect("a temp dir");
        let mut big = String::new();
        for n in 0..1000 {
            let _ = writeln!(big, "line {n}");
        }
        std::fs::write(dir.path().join("cef-host.log"), big).expect("the host log");
        let tail = log_tail(&dir.path().join("cef-host.log"));
        assert!(tail.starts_with("line 996 | "), "the last four lines are kept: {tail}");
        assert!(tail.ends_with("line 999"), "and the newest is the last: {tail}");

        let long = "x".repeat(600);
        std::fs::write(
            dir.path().join("cef-host.log"),
            format!("one\ntwo\nlong {long}\nthree\nfour\n"),
        )
        .expect("the host log");
        let tail = log_tail(&dir.path().join("cef-host.log"));
        assert_eq!(tail.matches(" | ").count(), 3, "the four newest lines: {tail}");
        assert!(tail.starts_with("two | "), "the oldest line is dropped: {tail}");
        assert!(!tail.contains(&long), "the long line is cut at the cap: {tail}");
        assert!(tail.chars().count() <= 4 * 512 + 12, "the tail cannot grow with a line: {tail}");

        // Four lines wider than the byte window: the marker at the start of
        // the oldest kept line is outside the window, so only its own tail
        // is kept.
        let mut wide = String::new();
        for n in 0..4 {
            let prefix = if n == 0 { "MARKER".to_owned() } else { format!("line{n}") };
            let _ = writeln!(wide, "{prefix}-{}", "M".repeat(1_100));
        }
        std::fs::write(dir.path().join("cef-host.log"), wide).expect("the host log");
        let tail = log_tail(&dir.path().join("cef-host.log"));
        assert!(!tail.contains("MARKER"), "a whole read keeps the marker: {tail}");
    }
}
