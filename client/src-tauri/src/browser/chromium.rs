//! The Chromium the browser host drives: where it lives, how it is launched,
//! and how a launch is found again after the client restarts.
//!
//! **The browser outlives the client.** It is launched detached, against a
//! profile under the client's app-support directory, so a login survives both
//! a client restart and a forge restart - and a relaunch is not needed to get
//! back to the page that was open. The `DevToolsActivePort` file Chromium
//! writes into the profile is what makes that work: the port a launch really
//! bound, and the browser target's own path, are in it.
//!
//! **The port is the browser's own choice, read back from that file, and
//! this is measured rather than assumed.** Measured on Chrome for Testing
//! 155, and true of the Brave and Chrome builds this drives: the browser
//! writes `DevToolsActivePort` when it is asked for
//! `--remote-debugging-port=0` and NOT when it is given a port number -
//! asked for 9333 by hand it listened on 9333 and wrote no file at all. A
//! fixed number would therefore have been a host that could never find its
//! browser again, and it would also collide with whichever
//! browser-automation tooling already holds the conventional ports (the
//! user's own Brave is on 9222).
//!
//! **One launch shape: a real window, born out of sight.** The browser runs
//! as the person's own installed build (never headless), started with
//! `--no-startup-window` so nothing appears at launch, and the host then
//! creates the window the profile keeps - a session drives it with the page
//! fully alive, and a hand-off's Open only moves it into view. That is what
//! makes the page a session prepared the same page the person acts on. The
//! moves themselves are [`window`]'s; this module is the lifecycle around
//! them. Measured 2026-10-12, the facts the shape rests on:
//!
//! - a plain headed launch shows its window and takes the front within ~1.5 s
//!   (`--no-startup-window` is what avoids that);
//! - `--window-position` is clamped by the window server onto the screen, so
//!   a launch cannot place its own window;
//! - a minimized window's page goes hidden and its animation frames stop,
//!   which wedges the driver's clicks (a 5 s timeout, `force` included) -
//!   so minimize is never the hide;
//! - a NORMAL window moved clear of every screen keeps its page visible and
//!   fully drivable, and the window server keeps ~40x40 points of it on
//!   screen whatever its size (the engine's own minimum window is 500x375,
//!   larger than that margin, so shrinking cannot remove it) - the hide's
//!   residual, named in the design;
//! - a window created `background` in a `--no-startup-window` browser never
//!   surfaces on its own, and the driver works in it unchanged.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::json;

use super::window;

/// How long a launch is given to write its port file and answer.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(15);

/// How long the window moves are given before they are given up on: a
/// browser that answers on its port but not on its own devtools endpoint is
/// a browser the host will not wedge on.
const WINDOW_TIMEOUT: Duration = Duration::from_secs(10);

/// Where a raise puts the window: on screen, near the top-left the window
/// server itself chooses for a fresh window, with the size it already has.
const RAISE_AT: (i64, i64) = (40, 60);

/// Where the hide asks the window to go: off every screen. The window server
/// clamps this to the nearest corner it allows, which is the point - the ask
/// names no screen size, so the same value works on any display setup.
const OUT_OF_SIGHT: (i64, i64) = (100_000, 100_000);

/// The argument list one launch runs with.
///
/// `profile` is the browser's own directory, which is what carries logins,
/// cookies and the HTTP cache across everything. There is one launch shape
/// and it is the person's own browser (Ved, 2026-10-07: it must look like a
/// real browser, not an automation tool) - never `--headless`, whose
/// `HeadlessChrome` brand and blanked client hints were why a UA mask
/// existed at all. That mask retired with the headless launch (2026-10-12):
/// a real build presents its real UA and its real hints, which is a better
/// posture than any mask.
///
/// **`--no-startup-window` is what keeps the launch invisible**: with no
/// window flag the browser opens its 1200x988 default and takes the front
/// within ~1.5 s (measured). Started without one, it holds no page and shows
/// nothing until the host creates the window itself, which it does
/// background-born and moved off every screen before any driver can reach
/// the browser.
pub fn launch_args(profile: &Path) -> Vec<String> {
    vec![
        // Let the browser choose, and read the choice from its port file:
        // handed a number it writes no file, which is a browser nothing can
        // find again.
        "--remote-debugging-port=0".to_owned(),
        format!("--user-data-dir={}", profile.display()),
        // The automation tell, switched off at the source: the engine's own
        // webdriver getter then reports false, which is what a real browser
        // reports - no page patch, nothing for a page to notice.
        "--disable-blink-features=AutomationControlled".to_owned(),
        // Where the engine honours it, the person's own X on the raised
        // window leaves the browser serving (measured on an older Brave: the
        // X takes the process down instead; the next call recovers either
        // way, on the page the last raise carried).
        "--keep-alive-for-test".to_owned(),
        // The window is the host's own, born hidden: see above.
        "--no-startup-window".to_owned(),
        // **Never the OS keychain.** This profile is forge's own and holds
        // nothing worth a keychain entry, while reaching for one raises a
        // system dialog naming the browser's own Safe Storage - once per ask
        // - that nobody is sitting in front of an unseen browser to answer,
        // and that spams whoever is at the machine. The mock store keeps the
        // same shape with a key that goes nowhere.
        "--use-mock-keychain".to_owned(),
        "--password-store=basic".to_owned(),
        // A first-run flow is a dialog nothing can see and nothing answers.
        "--no-first-run".to_owned(),
        "--no-default-browser-check".to_owned(),
    ]
}

/// Make the profile start fresh for the launch about to happen: the clean
/// marker written, the last session's tab data cleared.
///
/// **A launch must open exactly the tabs it is given.** Chromium restores the
/// previous session's tabs here - measured 2026-10-07: every relaunch piled
/// the window up (Ved's four-tab window: two carried pages and two blanks),
/// and the pile then breaks the DRIVER: the tabs it did not navigate sit
/// behind the page it did, the browser marks a background tab hidden, rAF
/// stops, and playwright's click waits forever on "visible, enabled and
/// stable" until its five-second timeout. The clean marker alone was not
/// enough (measured: Brave restores regardless of `exit_type`), so the
/// session-restore data is removed as well - the tabs a launch wants are the
/// ones on its command line, and a named profile's logins live in its own
/// data directory, which this never touches.
/// The marker is written the way a clean exit writes it, with everything else
/// in the file preserved; a prefs file that cannot be parsed is left alone
/// rather than clobbered.
fn fresh_session(profile: &Path) {
    let default = profile.join("Default");
    for stale in ["Sessions", "Current Session", "Current Tabs", "Last Session", "Last Tabs"] {
        let path = default.join(stale);
        if path.is_dir() {
            let _ = std::fs::remove_dir_all(&path);
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
    let prefs = default.join("Preferences");
    let Ok(text) = std::fs::read_to_string(&prefs) else { return };
    let Ok(mut parsed) = serde_json::from_str::<serde_json::Value>(&text) else { return };
    let Some(profile_prefs) = parsed.get_mut("profile").and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    profile_prefs.insert("exit_type".to_owned(), serde_json::Value::String("Normal".to_owned()));
    profile_prefs.insert("exited_cleanly".to_owned(), serde_json::Value::Bool(true));
    if let Ok(serialized) = serde_json::to_string(&parsed) {
        let _ = std::fs::write(&prefs, serialized);
    }
}

/// **One launch per profile ACROSS PROCESSES.** The host's own mutex
/// serializes its calls; a second host on the same directory - the app
/// opened twice, a second client - would otherwise delete the live launch's
/// port file (`forget_launch` does exactly that) and spawn onto a locked
/// profile, wedging both. An flock beside the port file makes the launch
/// critical section machine-wide, and the loser re-checks for the live
/// launch under the guard instead of launching over it.
///
/// A crash releases the flock with the process, so there is no stale-lock
/// cleanup; a holder that wedges past every bound is given up on by name
/// rather than waited for.
pub(super) struct LaunchLock {
    _file: std::fs::File,
}

enum Locked {
    Held(LaunchLock),
    Contended,
    Unavailable(String),
}

fn try_lock(profile: &Path) -> Locked {
    let path = profile.join("launch.lock");
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
    {
        Ok(file) => file,
        Err(why) => {
            return Locked::Unavailable(format!(
                "the launch lock at {} cannot be opened: {why}",
                path.display()
            ));
        }
    };
    match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Locked::Held(LaunchLock { _file: file }),
        Err(why) if why == rustix::io::Errno::WOULDBLOCK || why == rustix::io::Errno::AGAIN => {
            Locked::Contended
        }
        Err(why) => Locked::Unavailable(format!(
            "the launch lock at {} cannot be taken: {why}",
            path.display()
        )),
    }
}

impl LaunchLock {
    /// The lock for `profile`, waiting out whoever holds it - a launch takes
    /// seconds and the holder releases the moment its port answers - up to
    /// the launch deadline plus slack. A delete takes the same lock, so a
    /// launch and a removal can never interleave.
    pub(super) async fn acquire(profile: &Path) -> Result<Self, String> {
        let deadline = tokio::time::Instant::now() + LAUNCH_TIMEOUT + Duration::from_secs(2);
        loop {
            match try_lock(profile) {
                Locked::Held(lock) => return Ok(lock),
                Locked::Unavailable(why) => return Err(why),
                Locked::Contended => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(format!(
                            "the browser on {} is being launched by another process and did not \
                             answer in time",
                            profile.display(),
                        ));
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }
}

/// One launch, as `DevToolsActivePort` records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivePort {
    pub port: u16,
    /// The browser target's path, `/devtools/browser/<uuid>`.
    pub path: String,
    /// The process's id, when THIS call started it.
    ///
    /// A browser this process launched can be reaped by pid, which is what a
    /// run that ends without finishing - a failed test, a cancelled one -
    /// needs: `None` for one found rather than launched, which is nobody's
    /// child here and is meant to outlive the client.
    pub pid: Option<u32>,
}

/// Read the port file a launch writes into the profile.
///
/// `None` for a profile no launch has touched, and for a file that is not
/// two lines of what it should be - a half-written file is not a launch.
pub fn read_active_port(profile: &Path) -> Option<ActivePort> {
    let text = std::fs::read_to_string(profile.join("DevToolsActivePort")).ok()?;
    let mut lines = text.lines();
    let port = lines.next()?.trim().parse::<u16>().ok()?;
    let path = lines.next()?.trim();
    if !path.starts_with("/devtools/browser/") {
        return None;
    }
    Some(ActivePort { port, path: path.to_owned(), pid: None })
}

/// Whether the port a launch asked for is answered.
///
/// A real probe rather than a socket that accepts: `/json/version` is the
/// endpoint Chrome serves for exactly this question, and something else
/// holding the port answers it with nothing that parses.
pub async fn probe(port: u16) -> bool {
    probe_identity(port).await.is_some()
}

/// The first page target's URL on `port`, from the browser's own HTTP
/// endpoint: the page the sessions were driving, which Open carries across
/// its relaunch. `None` when nothing answers, or when no tab is open - a
/// window whose last tab was closed is a browser with nothing to carry.
pub async fn page_url(port: u16) -> Option<String> {
    let url = format!("http://127.0.0.1:{port}/json/list");
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

/// The browser target path an answering port reports - `/devtools/browser/
/// <uuid>`, taken from its own `/json/version` - or `None` when nothing
/// answers there as a browser. Compared against the port file's own line,
/// this is what tells a launch from whatever else took the port over.
pub async fn probe_identity(port: u16) -> Option<String> {
    let url = format!("http://127.0.0.1:{port}/json/version");
    let answer = tokio::task::spawn_blocking(move || http_get(&url)).await.ok().flatten()?;
    let body = answer.split_once("\r\n\r\n")?.1;
    let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
    let ws = parsed.get("webSocketDebuggerUrl")?.as_str()?;
    ws.find("/devtools/").map(|at| ws[at..].to_owned())
}

/// One HTTP/1.1 GET, without an HTTP client: the endpoint is loopback and
/// answers a small body, and a dependency for this would be larger than the
/// request.
///
/// **The answer is read to its own `Content-Length`, not to end-of-stream.**
/// Chrome's devtools server keeps the connection open even when the request
/// asks it to close (measured: the exact request below gets a 200 and 560
/// bytes, and the socket stays open), so a read that waited for the close
/// would time out on a body that had already arrived - which is a probe that
/// answers "no browser" for a browser that is right there.
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
            // A timeout is the end of what this far end was going to say;
            // what arrived is still read below.
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

/// The length of a whole HTTP answer, once its headers have arrived: the
/// header block plus the body its `Content-Length` names.
///
/// `None` until the headers are complete, and for an answer that states no
/// length - chunked or connection-delimited, which this server does not use
/// for this endpoint.
fn whole_body_len(answer: &[u8]) -> Option<usize> {
    let at = answer.windows(4).position(|window| window == b"\r\n\r\n")?;
    let headers = String::from_utf8_lossy(&answer[..at]);
    let length = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().ok())?
    })?;
    Some(at + 4 + length)
}

/// How much of a failed launch's stderr is kept, in bytes: enough for the
/// first complaint a Chrome prints about a locked profile or a quarantined
/// binary, bounded so a chatty failure cannot grow without end.
const STDERR_TAIL: usize = 4096;

/// Launch the browser against `profile`, and answer once its hidden window is
/// ready.
///
/// Detached on purpose: the child is left running when this drops - its
/// stdout goes nowhere a client reads, and its lifetime is the machine's, not
/// the client run's.
///
/// **Nothing appears and nothing takes the front.** The browser comes up with
/// `--no-startup-window`; the host then creates the profile's own window
/// through the browser's devtools endpoint - born `background`, so it never
/// surfaces by itself - and moves it clear of every screen before returning.
/// The move is a hard ordering, not a nicety: measured, the driver's work
/// surfaces the window at its CURRENT bounds, so a window still at its birth
/// point would appear mid-screen at the person the moment a session drove it.
///
/// `page` is what the window opens on: **the page the last raise carried**,
/// so a person pressing Open after a window was taken down (the X, where the
/// keep-alive does not hold) lands on the page they were last looking at
/// rather than a blank one. `None` opens `about:blank`.
pub async fn launch(
    binary: &Path,
    profile: &Path,
    page: Option<&str>,
) -> Result<ActivePort, String> {
    if !binary.is_file() {
        return Err(format!("the browser is not there at {} any more", binary.display()));
    }
    std::fs::create_dir_all(profile)
        .map_err(|why| format!("the browser profile directory cannot be made: {why}"))?;
    // **The machine-wide guard, held until the port answers.** Another
    // process may be mid-launch on this very directory: waiting for its lock
    // and then re-checking means this call attaches to the browser it
    // started, instead of deleting its port file and spawning onto a locked
    // profile.
    let _guard = LaunchLock::acquire(profile).await?;
    if let Some(active) = verified(profile).await {
        return Ok(active);
    }
    // A launch about to happen owns the wires: an older port file would be
    // read as this one's, and an older pid names a process this launch is not.
    forget_launch(profile);
    fresh_session(profile);

    let mut command = tokio::process::Command::new(binary);
    for arg in launch_args(profile) {
        command.arg(arg);
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        // **A failure's own words, not a silent null.** A locked profile, a
        // quarantined binary and an exec denial all answer the same way
        // otherwise - "did not answer within 15 s" - which names nothing a
        // reader can act on.
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn().map_err(|why| format!("the browser would not start: {why}"))?;
    // The id before the handle goes: the browser is meant to outlive this
    // call, and this process reaps nothing it did not spawn as its own work -
    // but a caller that must reap it needs the id, and the handle is what
    // carries it.
    let pid = child.id();
    let tail = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    if let Some(mut stderr) = child.stderr.take() {
        let tail = std::sync::Arc::clone(&tail);
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt as _;
            let mut chunk = [0_u8; 1024];
            while let Ok(read) = stderr.read(&mut chunk).await {
                if read == 0 {
                    break;
                }
                let mut held = tail.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                held.extend_from_slice(&chunk[..read]);
                let len = held.len();
                if len > STDERR_TAIL {
                    held.drain(..len - STDERR_TAIL);
                }
            }
        });
    }
    drop(child);

    let deadline = tokio::time::Instant::now() + LAUNCH_TIMEOUT;
    let active = loop {
        if let Some(active) = verified(profile).await {
            // The id is written down only once the launch ANSWERS, so a file
            // never names a browser that never came up.
            if let Some(pid) = pid {
                let _ = std::fs::write(pid_file(profile), pid.to_string());
            }
            break active;
        }
        if tokio::time::Instant::now() >= deadline {
            // The child is ours and it never answered: kill it here, so a
            // timed-out launch leaves no tree nobody can reap.
            if let Some(pid) = pid {
                kill_pid(pid, false).await;
                tokio::time::sleep(Duration::from_millis(300)).await;
                kill_pid(pid, true).await;
            }
            forget_launch(profile);
            let said = String::from_utf8_lossy(
                &tail.lock().unwrap_or_else(std::sync::PoisonError::into_inner),
            )
            .trim()
            .to_owned();
            let because =
                if said.is_empty() { String::new() } else { format!("; it said: {said}") };
            return Err(format!(
                "the browser did not answer on its port within {} s{because}",
                LAUNCH_TIMEOUT.as_secs(),
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    // The window, born hidden and moved clear of every screen BEFORE this
    // returns. A browser that answered on its port but could not get its
    // window is killed rather than left behind: a process nothing can raise
    // is not a launch.
    if let Err(why) = birth_hidden_window(&active, page).await {
        if let Some(pid) = pid {
            kill_pid(pid, true).await;
        }
        forget_launch(profile);
        return Err(why);
    }
    // **The profile's browser has a window**, which the raise path reads
    // before it moves anything: a live launch without this marker is the
    // headless shape a build before this one left behind, and [`show`] closes
    // it rather than raising at a window that is not there.
    if let Err(why) = std::fs::write(born_marker(profile), b"") {
        tauri_plugin_log::log::warn!(
            "the window's birth marker could not be written (event_name browser_window_born, \
             profile {}): a raise will take this launch for the older headless shape and relaunch \
             it: {why}",
            profile.display(),
        );
    }
    Ok(ActivePort { pid, ..active })
}

/// The browser to talk to: the one already running on `profile`, launched if
/// there is none. `page` is only consulted by the launch.
pub async fn ensure(
    binary: &Path,
    profile: &Path,
    page: Option<&str>,
) -> Result<ActivePort, String> {
    if let Some(active) = verified(profile).await {
        return Ok(active);
    }
    launch(binary, profile, page).await
}

/// Create the window a launch keeps, born `background` so it never surfaces
/// by itself, and move it clear of every screen before anything can drive it.
async fn birth_hidden_window(active: &ActivePort, page: Option<&str>) -> Result<(), String> {
    let answer = tokio::time::timeout(WINDOW_TIMEOUT, async {
        let mut cdp = window::Cdp::connect(active.port, &active.path).await?;
        let created = cdp
            .call(
                "Target.createTarget",
                json!({
                    "url": page.unwrap_or("about:blank"),
                    "newWindow": true,
                    "background": true,
                }),
                None,
            )
            .await?;
        let target = created
            .get("targetId")
            .and_then(serde_json::Value::as_str)
            .ok_or("the browser created a window but named no target for it")?
            .to_owned();
        let attached = cdp
            .call("Target.attachToTarget", json!({ "targetId": target, "flatten": true }), None)
            .await?;
        let session = attached
            .get("sessionId")
            .and_then(serde_json::Value::as_str)
            .ok_or("the browser refused the session for its own window")?
            .to_owned();
        let page = window::Page { target, session };
        window::move_to(&page, &mut cdp, OUT_OF_SIGHT.0, OUT_OF_SIGHT.1).await
    })
    .await
    .map_err(|_| {
        format!("the browser's window did not come up within {} s", WINDOW_TIMEOUT.as_secs())
    })?;
    answer.map_err(|why| format!("the browser's window could not be made: {why}"))
}

/// Move a live launch's window clear of every screen - the hide an answer and
/// the strip's lower both use, best-effort: a window that cannot be moved is
/// named, and the marker still comes down.
async fn move_out_of_sight(active: &ActivePort) -> Result<(), String> {
    tokio::time::timeout(WINDOW_TIMEOUT, async {
        let mut cdp = window::Cdp::connect(active.port, &active.path).await?;
        let page = cdp.page().await?;
        window::move_to(&page, &mut cdp, OUT_OF_SIGHT.0, OUT_OF_SIGHT.1).await
    })
    .await
    .map_err(|_| format!("the window did not move within {} s", WINDOW_TIMEOUT.as_secs()))?
}

/// The launch this profile names, when the port really answers AS that
/// launch: the probe's own `/devtools/browser/<uuid>` compared against the
/// port file's. A port another program took over answers without that path,
/// which is what keeps a port file from being taken for a live browser.
async fn verified(profile: &Path) -> Option<ActivePort> {
    let active = read_active_port(profile)?;
    answers_as(profile, active.port).await.then_some(active)
}

/// Whether `profile`'s launch is still the browser answering on `port`: the
/// port file names that port AND the probe's own target path matches the
/// file's. The identity check a profile runs after a failed call.
pub async fn answers_as(profile: &Path, port: u16) -> bool {
    let Some(active) = read_active_port(profile) else {
        return false;
    };
    active.port == port && probe_identity(port).await.as_deref() == Some(active.path.as_str())
}

/// The id a launch left, so a later client can close what it did not start.
fn pid_file(profile: &Path) -> PathBuf {
    profile.join("browser.pid")
}

/// The browsers a machine can offer, in the order they are picked.
const INSTALLED: [&str; 2] = [
    "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
];

/// The binary every launch drives: **the person's own installed browser,
/// headless and headed alike** (Ved, 2026-10-07) - a real browser rather
/// than an automation-branded bundle, and nothing vendored: the app ships
/// the driver, the machine ships the browser.
pub fn browser_binary() -> Result<PathBuf, String> {
    browser_binary_from(&INSTALLED)
}

/// The choice itself, apart from the standard paths: the first candidate
/// that is really there, else the refusal.
fn browser_binary_from(candidates: &[&str]) -> Result<PathBuf, String> {
    for candidate in candidates {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return Ok(path);
        }
    }
    Err("no browser to drive: this client drives the browser already on the machine - \
         install Brave or Google Chrome"
        .to_owned())
}

/// The marker a launch leaves once its window exists: the port file says
/// nothing about a window, and this is what tells "the launch this new code
/// made" from "a headless launch a build before it left running".
fn born_marker(profile: &Path) -> PathBuf {
    profile.join("has-window")
}

/// The marker a RAISE leaves, carrying the page it showed: the port file says
/// nothing about what the person can see, and this is what makes "is the
/// window up" answerable - the strip's show/hide button toggles on it, and
/// [`hide`] takes it down while the browser stays.
fn windowed_marker(profile: &Path) -> PathBuf {
    profile.join("windowed")
}

/// Whether the running launch is the raised one.
pub fn launched_windowed(profile: &Path) -> bool {
    windowed_marker(profile).is_file()
}

/// Whether this profile has EVER been launched: a launched Chromium writes
/// its own state into the data directory, while a profile carrying only the
/// seat's record has never been opened. The strip's row says which - "its
/// driver is gone" over a profile that never had one reads as a fault.
pub fn ever_opened(profile: &Path) -> bool {
    profile.join("Local State").is_file()
}

/// Whether the window is really UP on `profile`: the raise's marker AND a
/// live launch answering as this profile's own AND a page left to carry.
///
/// **The marker alone lied once** (Ved, 2026-10-07, live): he closed the
/// window with the X, the process survived with the marker still set, and
/// the next Open concluded a window was up and raised nothing. Everything
/// that SPEAKS for a window - the strip's window-open mark and its hide
/// label - must keep this predicate.
pub async fn windowed(profile: &Path) -> bool {
    if !launched_windowed(profile) {
        return false;
    }
    let Some(active) = verified(profile).await else {
        return false;
    };
    page_url(active.port).await.is_some()
}

/// The page the last raise carried, when it carried one: what a launch after
/// the window went (the engine took it down with the X) opens on, so the
/// person lands where they were rather than on a blank tab.
fn windowed_page(profile: &Path) -> Option<String> {
    let carried = std::fs::read_to_string(windowed_marker(profile)).ok()?;
    let carried = carried.trim();
    (!carried.is_empty()).then(|| carried.to_owned())
}

/// Bring the browser's window into view, for a hand-off's Open or the strip's
/// show: the profile's own window over the same browser the sessions drive,
/// **the very page they prepared**.
///
/// **Nothing is relaunched and nothing is closed.** The window exists from
/// the profile's first call, born hidden; the raise is two moves on it - on
/// screen, then brought to the front - so the page's DOM state, the fills a
/// session typed into it and every tab beside it are the same ones the
/// session left. A browser with no launch at all comes up first (hidden, on
/// the page the last raise carried) and is then raised: that is the one path
/// where a raise launches anything, and it is the path a person takes after
/// the engine took the window down (the X, where the keep-alive does not
/// hold).
///
/// The marker, written only here, carries the page the raise showed. It means
/// "the window is up" - what the strip's button toggles on - and [`hide`]
/// takes it down.
pub async fn show(binary: &Path, profile: &Path) -> Result<ActivePort, String> {
    // **A launch from before the window model has no window to raise** - a
    // headless browser an upgraded client adopted. It is closed, once, and
    // the new shape comes up on the page the last raise carried; every raise
    // after that finds a window.
    if !born_marker(profile).is_file()
        && let Some(active) = verified(profile).await
    {
        let _guard = LaunchLock::acquire(profile).await?;
        close_under_lock(profile, active.port).await;
    }
    let active = ensure(binary, profile, windowed_page(profile).as_deref()).await?;
    let raised = tokio::time::timeout(WINDOW_TIMEOUT, async {
        let mut cdp = window::Cdp::connect(active.port, &active.path).await?;
        let page = cdp.page().await?;
        window::bring_forward(&page, &mut cdp, RAISE_AT.0, RAISE_AT.1).await
    })
    .await
    .map_err(|_| {
        format!("the window did not come forward within {} s", WINDOW_TIMEOUT.as_secs())
    })?;
    raised.map_err(|why| format!("the window could not be raised: {why}"))?;
    let carried = page_url(active.port).await.unwrap_or_default();
    if let Err(why) = std::fs::write(windowed_marker(profile), carried) {
        // Not fatal - the window IS up - but the strip reads the marker as
        // "the window is up", so a launch whose marker cannot be written
        // shows a button that offers show where hide is the honest word.
        tauri_plugin_log::log::warn!(
            "the raised window's marker could not be written (event_name \
             browser_windowed_marker, profile {}): {why}",
            profile.display(),
        );
    }
    Ok(active)
}

/// Take the window back out of the person's sight: **the browser, its tabs
/// and its page all stay**, so the next call drives the same browser and the
/// next raise shows the same page. The hand-off is NOT answered by this -
/// Done or Not now is - so a person who lowers the window and walks away
/// leaves the session parked exactly as it was.
pub async fn hide(profile: &Path) {
    if let Some(active) = verified(profile).await
        && let Err(why) = move_out_of_sight(&active).await
    {
        tauri_plugin_log::log::warn!(
            "the window could not be moved out of sight (event_name browser_window_hide_failed, \
             profile {}): {why}",
            profile.display(),
        );
    }
    // The marker comes down whatever happened: it means "the window is up",
    // and after a lower it is not. A stale one would have the strip offer
    // hide over a window nobody can see.
    let _ = std::fs::remove_file(windowed_marker(profile));
}

/// The real close, under the launch lock a caller already holds: a delete
/// takes the lock across its whole act, and a close that re-took it would
/// spin the launch deadline against its own holder.
#[cfg(desktop)]
pub(super) async fn close_held_launch(profile: &Path) {
    if let Some(active) = read_active_port(profile) {
        close_under_lock(profile, active.port).await;
    }
}

/// Close the browser on `profile` for real - the profile's own close verb.
/// **An answer never comes here**: Done and Not now use [`hide`], which moves
/// the window out of sight and leaves everything running. This is the door
/// that ends the run - close, delete, the sweep - and the next call relaunches
/// over the same directory.
pub async fn close(profile: &Path) {
    // **The launch lock, so a close cannot eat a launch in flight.** An
    // unlocked closer reads "not answering" for a browser coming up, forgets
    // its port file, and that launch then kills the browser it just started
    // at its own deadline. The take is bounded like every other; a lock that
    // cannot be taken at all leaves the close running - no launch can be in
    // flight behind a lock nobody could take.
    let _guard = LaunchLock::acquire(profile).await;
    if let Some(active) = read_active_port(profile) {
        close_under_lock(profile, active.port).await;
    }
}

/// Close the browser on `profile`, so a relaunch is not a second browser onto
/// one profile.
///
/// **The LISTENER is the truth and the pid file is a hint.** The pid a launch
/// wrote describes a process that may since have exited - a dead pid, or a
/// recycled one belonging to something else entirely - so killing it blindly
/// is the exact outcome the pid file exists to avoid. So the pid is killed
/// only when it holds THIS port; otherwise, what holds the port is - by the
/// PORT and never by a pattern or a name, because this machine runs other
/// browsers and one of them belongs to the person sitting at it. **And only
/// after the port answers AS this profile's launch**: a port the launch let go
/// and something else took over is a stranger, and the wires go without a
/// kill.
///
/// Callers already hold the launch lock: a delete takes it across its whole
/// act, and a close that re-took it would spin the launch deadline against
/// its own holder.
async fn close_under_lock(profile: &Path, port: u16) {
    if !answers_as(profile, port).await {
        // **A young port file is a launch coming up**: the file exists 14-125
        // ms before `/json/version` answers (measured), and it gets the
        // launch's own bound to answer before it is declared dead.
        let deadline = tokio::time::Instant::now() + LAUNCH_TIMEOUT;
        while tokio::time::Instant::now() < deadline
            && launch_still_coming(profile)
            && !answers_as(profile, port).await
        {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if !answers_as(profile, port).await {
            forget_launch(profile);
            return;
        }
    }
    let listeners = port_pids(port).await;
    if listeners.is_empty() {
        return;
    }
    let named = std::fs::read_to_string(pid_file(profile))
        .ok()
        .and_then(|written| written.trim().parse::<u32>().ok());
    match named.filter(|pid| listeners.contains(pid)) {
        Some(pid) => {
            kill_pid(pid, false).await;
            if !port_frees(port).await {
                kill_pid(pid, true).await;
            }
        }
        None => {
            for pid in &listeners {
                kill_pid(*pid, false).await;
            }
            if !port_frees(port).await {
                for pid in &listeners {
                    kill_pid(*pid, true).await;
                }
            }
        }
    }
    if port_frees(port).await {
        forget_launch(profile);
    }
}

/// Whatever holds `port`.
async fn port_pids(port: u16) -> Vec<u32> {
    let Ok(listed) = tokio::process::Command::new("lsof")
        .args(["-t", &format!("-iTCP:{port}"), "-sTCP:LISTEN"])
        .output()
        .await
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&listed.stdout)
        .split_whitespace()
        .filter_map(|pid| pid.parse().ok())
        .collect()
}

/// Wait, bounded, for the port a closing browser holds to stop answering.
/// `false` when it never let go, which is what escalates the kill.
async fn port_frees(port: u16) -> bool {
    for _ in 0..20 {
        if !probe(port).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// Drop the wires a launch leaves. Called by a launch about to take them
/// over.
fn forget_launch(profile: &Path) {
    let _ = std::fs::remove_file(profile.join("DevToolsActivePort"));
    let _ = std::fs::remove_file(pid_file(profile));
    let _ = std::fs::remove_file(windowed_marker(profile));
    let _ = std::fs::remove_file(born_marker(profile));
}

/// Whether the port file is young enough to belong to a launch still coming
/// up: the file exists 14-125 ms before `/json/version` answers (measured),
/// and the launch bound is the window that may be spent waiting for a young
/// one.
fn launch_still_coming(profile: &Path) -> bool {
    std::fs::metadata(profile.join("DevToolsActivePort"))
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|written| written.elapsed().ok())
        .is_some_and(|age| age < LAUNCH_TIMEOUT)
}

/// Ask one process to stop; `hard` sends SIGKILL rather than SIGTERM.
async fn kill_pid(pid: u32, hard: bool) {
    let mut kill = tokio::process::Command::new("kill");
    if hard {
        kill.arg("-9");
    }
    let _ = kill.arg(pid.to_string()).status().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile_with(text: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::write(dir.path().join("DevToolsActivePort"), text).expect("the port file");
        dir
    }

    /// The port file is two lines: the port that was really bound, and the
    /// browser target's own path. Both are read rather than assumed, which is
    /// what lets a launch that could not have the fixed port still be found.
    #[test]
    fn the_active_port_file_is_the_launchs_own_port_and_path() {
        let dir = profile_with("9333\n/devtools/browser/9d1f-77aa\n");
        assert_eq!(
            read_active_port(dir.path()),
            Some(ActivePort {
                port: 9333,
                path: "/devtools/browser/9d1f-77aa".to_owned(),
                pid: None,
            }),
        );
    }

    /// A half-written file, a port that is not one, and a line that is not a
    /// browser path are all "no launch here" rather than a launch to attach
    /// to - attaching to whatever a partial file names is how a client talks
    /// to a stranger's debug port.
    #[test]
    fn a_port_file_that_is_not_two_whole_lines_is_no_launch() {
        for text in ["", "9333\n", "not-a-port\n/devtools/browser/x\n", "9333\n/system/page\n"] {
            let dir = profile_with(text);
            assert_eq!(read_active_port(dir.path()), None, "{text:?} is not a launch");
        }
        let empty = tempfile::tempdir().expect("a temp dir");
        assert_eq!(read_active_port(empty.path()), None, "a profile no launch touched");
    }

    /// The launch asks the browser to choose its own port - handed a number,
    /// the browser writes no port file and nothing can find the launch again;
    /// it carries the profile that keeps logins, starts with **no window at
    /// all** (nothing appears on the person's screen until the host creates
    /// one, and a hand-off's Open is what brings it forward), and **never
    /// reaches for the OS keychain**: that dialog is raised at whoever is at
    /// the machine, once per ask, and a browser whose window nobody is
    /// looking at has nobody to answer it.
    ///
    /// **And nothing appears at launch**: no `--headless` (the browser is the
    /// person's own, with its real UA and real client hints - the mask that
    /// used to ride a headless launch retired with it), and
    /// `--no-startup-window`, so the browser holds no page and shows nothing
    /// until the host creates the profile's window itself.
    #[test]
    fn a_launch_asks_for_port_zero_and_shows_nothing_until_the_host_says_so() {
        let args = launch_args(Path::new("/tmp/forge-profile"));
        assert!(
            args.contains(&"--remote-debugging-port=0".to_owned()),
            "the port is the browser's choice, read back from its own file: {args:?}",
        );
        assert!(args.contains(&"--user-data-dir=/tmp/forge-profile".to_owned()), "{args:?}");
        assert!(
            !args.contains(&"--headless".to_owned()),
            "every launch is the person's own real browser: {args:?}",
        );
        assert!(
            args.contains(&"--no-startup-window".to_owned()),
            "the browser starts with no window; the host creates one, born hidden: {args:?}",
        );
        assert!(
            !args.iter().any(|arg| arg.starts_with("--user-agent=")),
            "no UA mask: a real launch presents the browser's real user agent: {args:?}",
        );
        assert!(
            !args.iter().any(|arg| arg.starts_with("--disable-features=")),
            "and no client-hint switch - nothing overrides the UA, so nothing blanks them: {args:?}",
        );
        assert!(
            !args.iter().any(|arg| arg.starts_with("http") || arg.ends_with(":blank")),
            "no page rides the command line: the window opens on what the host asks for: {args:?}",
        );
        assert!(
            args.contains(&"--use-mock-keychain".to_owned()),
            "a launch that can raise Chromium Safe Storage's dialog is a launch that spams the \
             machine's user: {args:?}",
        );
        assert!(
            args.contains(&"--password-store=basic".to_owned()),
            "and the store behind the mock is the plain one: {args:?}",
        );
        assert!(
            args.contains(&"--keep-alive-for-test".to_owned()),
            "where the engine honours it, a person's X leaves the browser serving: {args:?}",
        );
        assert!(
            args.contains(&"--disable-blink-features=AutomationControlled".to_owned()),
            "and the webdriver tell is switched off at the engine, as a real browser reports it: \
             {args:?}",
        );
    }

    /// The page a raise carried rides its marker: a launch after the window
    /// went (the engine took it down with the X) opens on it, and an empty or
    /// missing marker is a launch that opens blank.
    #[test]
    fn the_marker_carries_the_page_a_lost_window_reopens_on() {
        let dir = tempfile::tempdir().expect("a temp dir");
        assert_eq!(windowed_page(dir.path()), None, "no marker, no page");

        std::fs::write(windowed_marker(dir.path()), b"https://example.com/x").expect("a marker");
        assert_eq!(
            windowed_page(dir.path()),
            Some("https://example.com/x".to_owned()),
            "the page the raise showed rides the marker",
        );

        std::fs::write(windowed_marker(dir.path()), b"").expect("a blank marker");
        assert_eq!(windowed_page(dir.path()), None, "a blank raise reopens blank");
    }

    /// A fake devtools endpoint for the windowed predicate: `/json/version`
    /// answers with an identity, everything else with the page list given.
    async fn fake_devtools(identity: &str, pages: &str) -> u16 {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("a port");
        let port = listener.local_addr().expect("the bound port").port();
        let identity = identity.to_owned();
        let pages = pages.to_owned();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { continue };
                let mut buffer = [0_u8; 2048];
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]);
                let body = if request.starts_with("GET /json/version") {
                    format!("{{\"webSocketDebuggerUrl\":\"ws://127.0.0.1:{port}{identity}\"}}")
                } else {
                    pages.clone()
                };
                let answer = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len(),
                );
                let _ = socket.write_all(answer.as_bytes()).await;
            }
        });
        port
    }

    /// **A window is the marker AND a live launch AND a page** - the same
    /// predicate `show` keeps. The marker alone lied once (a person's X left
    /// the keep-alive process with the marker set and no page), so anything
    /// that SPEAKS for a window - the strip's mark and its hide label - must
    /// not trust it alone.
    #[tokio::test]
    async fn a_window_is_marker_and_a_live_launch_and_a_page() {
        let dir = tempfile::tempdir().expect("a temp dir");
        assert!(!windowed(dir.path()).await, "no marker, no window");

        std::fs::write(windowed_marker(dir.path()), b"").expect("the marker");
        assert!(!windowed(dir.path()).await, "a marker with nothing answering is no window");

        // The X-close state: the marker survives, the launch answers, and no
        // page is left.
        let port = fake_devtools("/devtools/browser/live", "[]").await;
        std::fs::write(
            dir.path().join("DevToolsActivePort"),
            format!("{port}\n/devtools/browser/live\n"),
        )
        .expect("the port file");
        assert!(
            !windowed(dir.path()).await,
            "a windowless launch leaves the marker and no page - not a window",
        );

        // Marker + a launch answering AS this profile + a page target: up.
        let port = fake_devtools(
            "/devtools/browser/live2",
            "[{\"type\":\"page\",\"url\":\"about:blank\"}]",
        )
        .await;
        std::fs::write(
            dir.path().join("DevToolsActivePort"),
            format!("{port}\n/devtools/browser/live2\n"),
        )
        .expect("the port file");
        assert!(
            windowed(dir.path()).await,
            "the marker, a launch answering as it, and a page make a window",
        );
    }

    /// **A launch must not resurrect the last session's tabs.** The clean
    /// marker is normalized and the session-restore data is cleared here
    /// before every spawn - a launch opens exactly the tabs it is given
    /// (Ved's four-tab window, 2026-10-07).
    #[test]
    fn a_launch_clears_the_last_session_so_tabs_do_not_accumulate() {
        let dir = tempfile::tempdir().expect("a temp dir");
        // A fresh profile has no prefs at all, which is not an error: there
        // is no marker to normalize and no session to clear.
        fresh_session(dir.path());
        assert!(!dir.path().join("Default/Preferences").exists());

        let default = dir.path().join("Default");
        std::fs::create_dir_all(default.join("Sessions")).expect("a Default profile");
        // Every entry the clearing covers, or a new one added without a
        // write here is one nothing checks.
        for stale in
            ["Sessions/Session_1", "Current Session", "Current Tabs", "Last Session", "Last Tabs"]
        {
            std::fs::write(default.join(stale), b"stale")
                .unwrap_or_else(|why| panic!("{stale}: {why}"));
        }
        let prefs = default.join("Preferences");
        std::fs::write(
            &prefs,
            br#"{"profile": {"exit_type": "Crashed", "name": "Person 1"}, "other": 7}"#,
        )
        .expect("prefs");

        fresh_session(dir.path());

        for stale in ["Sessions", "Current Session", "Current Tabs", "Last Session", "Last Tabs"] {
            assert!(!default.join(stale).exists(), "{stale} is cleared");
        }
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&prefs).expect("prefs read"))
                .expect("prefs are JSON");
        assert_eq!(parsed["profile"]["exit_type"], serde_json::json!("Normal"), "{parsed}");
        assert_eq!(parsed["profile"]["exited_cleanly"], serde_json::json!(true), "{parsed}");
        assert_eq!(
            parsed["profile"]["name"],
            serde_json::json!("Person 1"),
            "everything else stays"
        );
        assert_eq!(parsed["other"], serde_json::json!(7), "{parsed}");

        // A file that is not JSON must not be clobbered by the normalizer.
        std::fs::write(&prefs, b"not json at all").expect("prefs");
        fresh_session(dir.path());
        assert_eq!(
            std::fs::read_to_string(&prefs).expect("prefs read"),
            "not json at all",
            "a prefs file this cannot parse is left exactly as it was",
        );
    }

    /// **The launch lock is exclusive across open file descriptions**, which
    /// is what makes it work across processes: a second take on the same
    /// directory is contended while the first is held, and free once it
    /// drops. (The wait-until-free path is pinned under a paused clock just
    /// below, and the guard's whole span across a real launch by the
    /// two-hosts live test in `tests/browser_live.rs`.)
    #[tokio::test]
    async fn the_launch_lock_is_held_until_it_drops() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let first = LaunchLock::acquire(dir.path()).await.expect("the first take");
        assert!(
            matches!(try_lock(dir.path()), Locked::Contended),
            "a second take is contended while the first is held",
        );
        drop(first);
        assert!(matches!(try_lock(dir.path()), Locked::Held(_)), "and free once the first drops",);
    }

    /// **The wait path, with the clock under the test's hand**: a contended
    /// acquire waits the holder out and takes the lock the moment it frees -
    /// no second live process needed, the contention is the same flock
    /// between two file descriptions - and a holder that never frees is
    /// given up on BY NAME at the bound rather than wedged on forever.
    #[tokio::test(start_paused = true)]
    async fn a_contended_acquire_waits_out_the_holder_and_gives_up_by_name() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let held = LaunchLock::acquire(dir.path()).await.expect("the first take");

        let waiter = tokio::spawn({
            let path = dir.path().to_path_buf();
            async move { LaunchLock::acquire(&path).await }
        });
        // Let the waiter make its first attempt and drop into its wait.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert!(
            !waiter.is_finished(),
            "the waiter cannot hold what the first take holds - it must be waiting",
        );

        // The holder frees: the waiter takes it, well before the deadline.
        drop(held);
        let taken = waiter
            .await
            .expect("the waiter task")
            .expect("the waiter takes the lock the moment it frees");
        drop(taken);

        // A holder that NEVER frees is given up on by name at the bound.
        let _wedged = LaunchLock::acquire(dir.path()).await.expect("the wedged holder");
        let refused = LaunchLock::acquire(dir.path()).await;
        let Err(why) = refused else {
            panic!("an acquire behind a holder past the bound must be refused, not wedged");
        };
        assert!(
            why.contains("did not answer in time"),
            "the give-up is BY NAME, naming what did not answer: {why}",
        );
        assert!(
            why.contains(dir.path().to_str().expect("a path")),
            "and the refusal names the profile it gave up on: {why}",
        );
    }

    /// **A young port file is a launch coming up, not a dead one**: the file
    /// exists 14-125 ms before `/json/version` answers (measured), so a close
    /// gives it the launch's own bound to answer before declaring it dead -
    /// and only then forgets it. Deleting a live launch's file makes that
    /// launch kill the browser it just started at its own deadline.
    #[tokio::test(start_paused = true)]
    async fn a_close_tolerates_a_young_port_file() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::write(dir.path().join("DevToolsActivePort"), b"9544\n/devtools/browser/young\n")
            .expect("a young port file");
        let started = tokio::time::Instant::now();
        close_under_lock(dir.path(), 9544).await;
        let waited = started.elapsed();
        assert!(
            waited >= LAUNCH_TIMEOUT,
            "a young port file gets the launch bound to answer before it is declared dead: {waited:?}",
        );
        assert!(
            !dir.path().join("DevToolsActivePort").exists(),
            "and once the bound passes without an answer, the file is forgotten",
        );
    }

    /// An OLD port file whose endpoint does not answer is not a launch coming
    /// up: nothing waits on it, and it is forgotten at once.
    #[tokio::test]
    async fn a_close_forgets_an_old_dead_port_file_at_once() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let file = dir.path().join("DevToolsActivePort");
        std::fs::write(&file, b"9544\n/devtools/browser/dead\n").expect("an old port file");
        let old = std::time::SystemTime::now() - LAUNCH_TIMEOUT - Duration::from_secs(60);
        let handle = std::fs::OpenOptions::new().write(true).open(&file).expect("the file");
        handle
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .expect("the file's time moves back");
        drop(handle);
        let started = tokio::time::Instant::now();
        close_under_lock(dir.path(), 9544).await;
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "an old dead file is not waited on: {:?}",
            started.elapsed(),
        );
        assert!(!file.exists(), "an old dead port file is forgotten");
    }

    /// A lock file that cannot be used answers why rather than looping: a
    /// directory where the lock file goes is the cheapest stand-in.
    #[tokio::test]
    async fn an_unusable_launch_lock_answers_why() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::create_dir(dir.path().join("launch.lock"))
            .expect("a directory in the lock's place");
        let refused = LaunchLock::acquire(dir.path()).await;
        assert!(
            matches!(refused, Err(ref why) if why.contains("launch.lock")),
            "the refusal names the file it could not take",
        );
    }

    /// The answer's own `Content-Length` is what says it is complete, since
    /// the connection is not what ends it. A headers-only answer, a
    /// mismatched case and an answer naming no length all read as they are.
    #[test]
    fn a_whole_answer_is_as_long_as_its_content_length_says() {
        let answer = b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"a\":\"bcd\"}";
        assert_eq!(whole_body_len(answer), Some(answer.len()), "headers + the named body");

        let odd_case = b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhello";
        assert_eq!(whole_body_len(odd_case), Some(odd_case.len()));

        assert_eq!(
            whole_body_len(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n"),
            None,
            "no body yet"
        );
        assert_eq!(whole_body_len(b"HTTP/1.1 200 OK\r\n\r\nhello"), None, "no length named");
    }

    /// The browser choice: the first candidate that is really there, in the
    /// order given, and a refusal that names what to install when none is.
    #[test]
    fn the_browser_is_the_first_installed_candidate_or_a_refusal() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let there = dir.path().join("there");
        std::fs::write(&there, b"").expect("a candidate that exists");
        let missing = dir.path().join("missing");

        let found = browser_binary_from(&[
            missing.to_str().expect("a path"),
            there.to_str().expect("a path"),
        ])
        .expect("the second candidate is there");
        assert_eq!(found, there, "the first candidate that exists wins");

        let refused = browser_binary_from(&[missing.to_str().expect("a path")])
            .expect_err("nothing there to drive");
        assert!(refused.contains("Brave"), "the refusal names what to install: {refused}");
    }
}
