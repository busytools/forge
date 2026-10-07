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

use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a launch is given to write its port file and answer.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(15);

/// The argument list one launch runs with.
///
/// `profile` is the browser's own directory, which is what carries logins,
/// cookies and the HTTP cache across everything. `headed` is the hand-off's
/// own window: the default is **headless** - the browser is seen only when
/// the person says Open - and the headed launch is the same launch without
/// that flag, against the person's installed browser when one is there (Ved,
/// 2026-10-07: it must look like a real browser, not an automation tool).
///
/// `page` is the tab the launch opens on: **the page the sessions were
/// driving**, carried across the relaunch Open makes, because a person who
/// pressed Open to act on a page must not land on a blank one. `None` for a
/// cold launch, which opens `about:blank` (a browser with no tab makes the
/// first navigation depend on the driver inventing one).
pub fn launch_args(profile: &Path, headed: bool, page: Option<&str>) -> Vec<String> {
    let mut args = vec![
        // Let the browser choose, and read the choice from its port file:
        // handed a number it writes no file, which is a browser nothing can
        // find again.
        "--remote-debugging-port=0".to_owned(),
        format!("--user-data-dir={}", profile.display()),
    ];
    if !headed {
        args.push("--headless".to_owned());
    } else {
        // **The X closes the window, not the browser** (measured on Brave,
        // 2026-10-07): Chrome's own automation keep-alive, so the process
        // and the agents' CDP connection survive a person closing the
        // window. The tab dies with its window; the next call re-navigates.
        args.push("--keep-alive-for-test".to_owned());
    }
    args.extend([
        // **Never the OS keychain.** This profile is forge's own and holds
        // nothing worth a keychain entry, while reaching for one raises a
        // system dialog naming the browser's own Safe Storage - once per ask
        // - that nobody is sitting in front of a headless browser to answer,
        // and that spams whoever is at the machine. The mock store keeps the
        // same shape with a key that goes nowhere.
        "--use-mock-keychain".to_owned(),
        "--password-store=basic".to_owned(),
        // A first-run flow is a dialog nothing can see and nothing answers.
        "--no-first-run".to_owned(),
        "--no-default-browser-check".to_owned(),
    ]);
    args.push(page.unwrap_or("about:blank").to_owned());
    args
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
/// ones on its command line, and named profiles reopen their own saved tabs.
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
    let Some(profile_prefs) =
        parsed.get_mut("profile").and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    profile_prefs.insert("exit_type".to_owned(), serde_json::Value::String("Normal".to_owned()));
    profile_prefs.insert("exited_cleanly".to_owned(), serde_json::Value::Bool(true));
    if let Ok(serialized) = serde_json::to_string(&parsed) {
        let _ = std::fs::write(&prefs, serialized);
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

/// Launch the browser against `profile`, and answer once it is answering on
/// its port.
///
/// Detached on purpose: the child is left running when this drops - its
/// stdout goes nowhere a client reads, and its lifetime is the machine's, not
/// the client run's.
///
/// **Headless by default.** The agents drive it invisibly, and the only
/// visible surface is the hand-off's window: `show` raises the same browser
/// over the same profile, so the person is dropped into the very browser the
/// sessions drive rather than a second one.
pub async fn launch(binary: &Path, profile: &Path) -> Result<ActivePort, String> {
    launch_with(binary, profile, false, None).await
}

/// The same launch with `headed` asked for - and, when the relaunch is Open's,
/// the page the sessions were driving, so the window opens on it.
async fn launch_with(
    binary: &Path,
    profile: &Path,
    headed: bool,
    page: Option<&str>,
) -> Result<ActivePort, String> {
    if !binary.is_file() {
        return Err(format!("the browser is not there at {} any more", binary.display()));
    }
    std::fs::create_dir_all(profile)
        .map_err(|why| format!("the browser profile directory cannot be made: {why}"))?;
    // A launch about to happen owns the wires: an older port file would be
    // read as this one's, and an older pid names a process this launch is not.
    forget_launch(profile);
    fresh_session(profile);

    let mut command = tokio::process::Command::new(binary);
    for arg in launch_args(profile, headed, page) {
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
    let mut child =
        command.spawn().map_err(|why| format!("the browser would not start: {why}"))?;
    // The id before the handle goes: the browser is meant to outlive this
    // call, and this process reaps nothing it did not spawn as its own work -
    // but a caller that must reap it needs the id, and the handle is what
    // carries it. It is written down only once the launch ANSWERS, so a file
    // never names a browser that never came up.
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
    loop {
        if let Some(active) = verified(profile).await {
            if let Some(pid) = pid {
                let _ = std::fs::write(pid_file(profile), pid.to_string());
            }
            // Written only now, with the browser answering: `show` reads it as
            // "a window is up", and a marker for a launch that failed is a
            // claim nobody can check. It carries the page the launch opened
            // on, so an Open after the person closed the window reopens that
            // page rather than a blank one.
            if headed {
                let _ = std::fs::write(windowed_marker(profile), page.unwrap_or(""));
            }
            return Ok(ActivePort { pid, ..active });
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
    }
}

/// The browser to talk to: the one already running on `profile`, launched if
/// there is none.
pub async fn ensure(binary: &Path, profile: &Path) -> Result<ActivePort, String> {
    if let Some(active) = verified(profile).await {
        return Ok(active);
    }
    launch(binary, profile).await
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
/// file's. The identity check a context runs after a failed call.
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

/// The marker a headed launch leaves in the profile: the port file says
/// nothing about a window, and this is what makes "is a window up"
/// answerable.
fn windowed_marker(profile: &Path) -> PathBuf {
    profile.join("windowed")
}

/// Whether the running launch is the headed one.
pub fn launched_windowed(profile: &Path) -> bool {
    windowed_marker(profile).is_file()
}

/// The page the last headed launch opened on, when it carried one: the
/// fallback an Open uses once the person has closed the window (the tab goes
/// with it, so there is no live page left to read).
fn windowed_page(profile: &Path) -> Option<String> {
    let carried = std::fs::read_to_string(windowed_marker(profile)).ok()?;
    let carried = carried.trim();
    (!carried.is_empty()).then(|| carried.to_owned())
}

/// Bring the running browser's window to the front. `open` on the app bundle
/// activates an app that is already running; a failure here is not the
/// raise's failure - the window is up either way.
fn activate(binary: &Path) {
    let Some(bundle) = app_bundle(binary) else { return };
    let _ = std::process::Command::new("open").arg(bundle).spawn();
}

/// The `.app` bundle a browser binary lives in, from its
/// `.../Contents/MacOS/<name>` path; `None` for a binary outside one.
fn app_bundle(binary: &Path) -> Option<PathBuf> {
    binary
        .ancestors()
        .find(|at| at.extension().is_some_and(|ext| ext == "app"))
        .map(Path::to_path_buf)
}

/// Bring the browser up VISIBLY, for a hand-off's Open or the strip's show:
/// the person's own installed browser, over the same profile the agents
/// drive, **opened on the page the sessions were driving**.
///
/// **A window is a launch flag, not something a running browser can be
/// told**, so the running launch is closed and the same profile is
/// relaunched headed. The relaunch is a fresh browser, so the page is
/// carried across explicitly - Chromium's own session restore is guesswork,
/// and a person who pressed Open to act on the session's page (a CAPTCHA,
/// say) must not land on a blank tab. The cost of the relaunch: open driver
/// transports die with the old browser and rebuild on the next call, and the
/// profile keeps logins and cookies.
///
/// **A window with a page in it is the only thing answered as it is.** The
/// headed marker alone lied once (Ved, 2026-10-07, live): he closed the
/// window with the X, the process survived on Chrome's keep-alive with the
/// marker still set, and the next Open concluded a window was up and raised
/// nothing. A window the person closed holds no tab - measured: the targets
/// empty with the window - so "a window is up" means marker AND a page.
pub async fn show(binary: &Path, profile: &Path) -> Result<ActivePort, String> {
    if launched_windowed(profile)
        && let Some(active) = verified(profile).await
        && page_url(active.port).await.is_some()
    {
        // **A window already up is BROUGHT FORWARD, not answered as it is.**
        // The dock's word is "raise", and the early return alone made a
        // second Open a silent no-op - measured live, 2026-10-07.
        activate(binary);
        return Ok(active);
    }
    let page = match read_active_port(profile) {
        Some(active) => {
            // The live tab first; the last carried page otherwise - a window
            // the person closed took its tab with it, and reopening on the
            // page they were looking at is the recovery the X-close needs.
            let page = page_url(active.port).await.or_else(|| windowed_page(profile));
            close(profile, active.port).await;
            page
        }
        None => windowed_page(profile),
    };
    launch_with(binary, profile, true, page.as_deref()).await
}

/// Take the window back down: the browser is closed, and the next agent call
/// relaunches it headless over the same profile. The hand-off is NOT answered
/// by this - Done or Not now is - so a person who closes the window and walks
/// away leaves the session parked exactly as it was.
pub async fn hide(profile: &Path) {
    if let Some(active) = read_active_port(profile) {
        close(profile, active.port).await;
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
async fn close(profile: &Path, port: u16) {
    if !answers_as(profile, port).await {
        forget_launch(profile);
        return;
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
    /// it carries the profile that keeps logins, opens a page (because a
    /// browser with no tab makes the first navigation depend on the driver
    /// inventing one), runs **headless** (nothing appears on the person's
    /// screen until a hand-off's Open raises the window), and **never reaches
    /// for the OS keychain**: that dialog is raised at whoever is at the
    /// machine, once per ask, and a headless browser has nobody to answer it.
    ///
    /// **And an Open's relaunch opens on the session's page**, not a blank
    /// tab: the page rides as the launch's own argument.
    #[test]
    fn a_launch_lets_the_browser_choose_its_port_and_carries_its_profile_and_a_page() {
        let args = launch_args(Path::new("/tmp/forge-profile"), false, None);
        assert!(
            args.contains(&"--remote-debugging-port=0".to_owned()),
            "the port is the browser's choice, read back from its own file: {args:?}",
        );
        assert!(args.contains(&"--user-data-dir=/tmp/forge-profile".to_owned()), "{args:?}");
        assert!(args.contains(&"--headless".to_owned()), "{args:?}");
        assert_eq!(args.last().map(String::as_str), Some("about:blank"), "{args:?}");
        assert!(
            args.contains(&"--use-mock-keychain".to_owned()),
            "a launch that can raise Chromium Safe Storage's dialog is a launch that spams the \
             machine's user: {args:?}",
        );
        assert!(
            args.contains(&"--password-store=basic".to_owned()),
            "and the store behind the mock is the plain one: {args:?}",
        );

        let carried =
            launch_args(Path::new("/tmp/forge-profile"), true, Some("https://example.com/x"));
        assert_eq!(
            carried.last().map(String::as_str),
            Some("https://example.com/x"),
            "Open's relaunch opens on the page the sessions were driving: {carried:?}",
        );
    }

    /// **The headed launch survives its own window closing** (measured on
    /// Brave): `--keep-alive-for-test` is Chrome's own automation switch for
    /// it, and the X must not take the agents' browser down with the window.
    #[test]
    fn a_headed_launch_drops_headless_and_keeps_the_browser_alive() {
        let headless = launch_args(Path::new("/tmp/forge-profile"), false, None);
        let headed = launch_args(Path::new("/tmp/forge-profile"), true, None);
        assert!(headless.contains(&"--headless".to_owned()), "{headless:?}");
        assert!(!headed.contains(&"--headless".to_owned()), "{headed:?}");
        assert!(
            headed.contains(&"--keep-alive-for-test".to_owned()),
            "a window close must leave the browser serving: {headed:?}",
        );
        assert!(
            !headless.contains(&"--keep-alive-for-test".to_owned()),
            "the headless launch has no window to keep it alive past: {headless:?}",
        );
    }

    /// The page a closed window's Open reopens on: the last headed launch
    /// carries it in the marker, and an empty or missing marker is a launch
    /// that opened blank.
    #[test]
    fn the_marker_carries_the_page_a_closed_window_reopens_on() {
        let dir = tempfile::tempdir().expect("a temp dir");
        assert_eq!(windowed_page(dir.path()), None, "no marker, no page");

        std::fs::write(windowed_marker(dir.path()), b"https://example.com/x").expect("a marker");
        assert_eq!(
            windowed_page(dir.path()),
            Some("https://example.com/x".to_owned()),
            "the page the launch carried rides the marker",
        );

        std::fs::write(windowed_marker(dir.path()), b"").expect("a blank marker");
        assert_eq!(windowed_page(dir.path()), None, "a blank launch reopens blank");
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
        std::fs::write(default.join("Sessions/Session_1"), b"last session").expect("a session");
        std::fs::write(default.join("Current Session"), b"current").expect("a current session");
        std::fs::write(default.join("Last Tabs"), b"tabs").expect("last tabs");
        let prefs = default.join("Preferences");
        std::fs::write(
            &prefs,
            br#"{"profile": {"exit_type": "Crashed", "name": "Person 1"}, "other": 7}"#,
        )
        .expect("prefs");

        fresh_session(dir.path());

        assert!(!default.join("Sessions").exists(), "the session data is gone");
        assert!(!default.join("Current Session").exists(), "so is the current one");
        assert!(!default.join("Last Tabs").exists(), "and the older tab files");
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&prefs).expect("prefs read"))
                .expect("prefs are JSON");
        assert_eq!(parsed["profile"]["exit_type"], serde_json::json!("Normal"), "{parsed}");
        assert_eq!(parsed["profile"]["exited_cleanly"], serde_json::json!(true), "{parsed}");
        assert_eq!(parsed["profile"]["name"], serde_json::json!("Person 1"), "everything else stays");
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

    /// The bundle a browser binary lives in, which is what activation opens:
    /// Brave's binary is under its `.app`, and a path outside one leaves
    /// nothing to bring forward.
    #[test]
    fn a_browser_binary_resolves_to_its_app_bundle() {
        assert_eq!(
            app_bundle(Path::new("/Applications/Brave Browser.app/Contents/MacOS/Brave Browser")),
            Some(PathBuf::from("/Applications/Brave Browser.app")),
        );
        assert_eq!(app_bundle(Path::new("/tmp/plain-binary")), None);
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
