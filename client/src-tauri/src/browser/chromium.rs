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
///
/// `user_agent` is the mask's own half - the UA a headless launch presents,
/// from [`masked_user_agent`] - and `None` for a headed launch, which
/// presents the browser's real one.
///
/// **The mask rides the launch because this is the only place it can.** A
/// driven browser reports `navigator.webdriver` true and brands its UA
/// `HeadlessChrome`, and a server reads both; the flag leaves the engine's
/// own webdriver getter reporting false, and the UA pair fixes the string
/// and the header together. See `masked_user_agent` for why the UA cannot
/// come from the driver instead.
pub fn launch_args(
    profile: &Path,
    headed: bool,
    page: Option<&str>,
    user_agent: Option<&str>,
) -> Vec<String> {
    let mut args = vec![
        // Let the browser choose, and read the choice from its port file:
        // handed a number it writes no file, which is a browser nothing can
        // find again.
        "--remote-debugging-port=0".to_owned(),
        format!("--user-data-dir={}", profile.display()),
        // The automation tell, switched off at the source: the engine's own
        // webdriver getter then reports false, which is what a real browser
        // reports - no page patch, nothing for a page to notice.
        "--disable-blink-features=AutomationControlled".to_owned(),
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
    if let Some(user_agent) = user_agent {
        args.push(format!("--user-agent={user_agent}"));
        // **Chromium blanks a page's client hints when the UA is overridden**
        // (measured on Brave 155: without this, `sec-ch-ua` goes empty on
        // every request). Switched off, the override fixes the UA string and
        // leaves the real `sec-ch-ua` and `sec-ch-ua-platform` beside it.
        // This is the one `--disable-features` switch a launch carries.
        args.push("--disable-features=UACHOverrideBlank".to_owned());
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

/// How long the browser is given to answer `--version` before the mask is
/// given up on for this launch.
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);

/// The user agent a headless launch presents: the browser's own, with the
/// headless brand removed.
///
/// **It cannot come from the driver**, which is where the initial script
/// rides: measured, the driver's own `--user-agent` is inert over a
/// `--cdp-endpoint` connection, and the header is what a site's server
/// reads, so the launch's own switch is the one route to it.
///
/// The version is read from the binary rather than spelled out here, because
/// the reduced UA freezes the platform and the version's last three
/// components: a real build reports `Chrome/<major>.0.0.0` whatever its full
/// version is, so the major - all `--version` is parsed for - is the only
/// part that moves across browser updates. A version that cannot be read
/// leaves the UA unmasked and says so; it is not a reason to refuse the
/// launch.
async fn masked_user_agent(binary: &Path) -> Result<String, String> {
    let answer = tokio::time::timeout(
        VERSION_TIMEOUT,
        tokio::process::Command::new(binary).arg("--version").output(),
    )
    .await
    .map_err(|_| format!("--version did not answer within {} s", VERSION_TIMEOUT.as_secs()))?
    .map_err(|why| format!("--version could not be run: {why}"))?;
    let said = String::from_utf8_lossy(&answer.stdout);
    reduced_user_agent(&said).ok_or_else(|| format!("the version line did not name one: {said:?}"))
}

/// The reduced user agent for a browser's `--version` line (`Brave Browser
/// 155.1.97.56`, `Google Chrome 155.0.1234.56`).
///
/// The platform and the version's tail are Chromium's frozen ones - a real
/// macOS build reports `Macintosh; Intel Mac OS X 10_15_7` and
/// `Chrome/<major>.0.0.0` - which is what the launched browser's own UA was
/// measured to carry. macOS is the platform this client ships on; a second
/// one brings its own string here.
///
/// **A version line is read by shape, and a future one whose last token is a
/// different dotted number would build a well-formed but wrong UA without
/// saying so** - the price of not spelling the string out here, paid only if
/// a browser ever prints something other than `<name> <version>`.
fn reduced_user_agent(version: &str) -> Option<String> {
    let named = version.lines().next()?.split_whitespace().last()?;
    let major = named.split('.').next()?;
    if !named.contains('.') || major.is_empty() || !major.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(format!(
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) \
         Chrome/{major}.0.0.0 Safari/537.36"
    ))
}

/// What one page reads about the browser, as the driver's capture snippet
/// answers it.
///
/// **The source values are the page's own**: the low-entropy hints stay real
/// under the launch's UA override (measured), while `blanked` records that
/// the high-entropy five came back empty - the state this rebuild repairs.
#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct HintCapture {
    /// The UA string the page itself reads - the masked one, which the
    /// rebuild presents unchanged.
    pub user_agent: String,
    /// The real brand list, served verbatim.
    pub brands: Vec<HintBrand>,
    pub platform: String,
    pub mobile: bool,
    /// Whether all five high-entropy hints came back empty. `false` means the
    /// browser reports its own (an unmasked launch) and nothing should be
    /// rebuilt over them.
    pub blanked: bool,
}

/// One client-hint brand, as the page lists it.
#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct HintBrand {
    pub brand: String,
    pub version: String,
}

/// The full version a UA string carries, or `None` for a string that names
/// none.
fn user_agent_version(user_agent: &str) -> Option<&str> {
    let at = user_agent.find("Chrome/")? + "Chrome/".len();
    let rest = &user_agent[at..];
    let end = rest.find(' ').unwrap_or(rest.len());
    let version = &rest[..end];
    (!version.is_empty()).then_some(version)
}

/// The client-hint metadata the launch's UA override blanks, rebuilt from the
/// page's own values plus the machine's.
///
/// **The version comes from the string the page reads**, which is the masked
/// reduced UA: a real build reports `Chrome/<major>.0.0.0` there while its
/// client hints freeze the same way (measured: Brave 155 headless reads
/// `155.0.0.0` for both). Each brand's full version is its own major with
/// that frozen tail, grease brands included - exactly the list the same
/// browser reports without the override.
///
/// `None` for a capture that cannot support a truthful rebuild: an unmasked
/// launch reads the five for real, and rebuilding over them would replace
/// the browser's own values with derived ones; a capture with no brands read
/// no document; a UA string naming no version is a half-claim.
pub fn user_agent_metadata(
    capture: &HintCapture,
    platform_version: &str,
    architecture: &str,
    bitness: &str,
) -> Option<serde_json::Value> {
    if !capture.blanked || capture.brands.is_empty() {
        return None;
    }
    let version = user_agent_version(&capture.user_agent)?;
    let tail = version.split_once('.').map_or("0.0.0", |(_, tail)| tail);
    let full_version_list: Vec<serde_json::Value> = capture
        .brands
        .iter()
        .map(|brand| {
            serde_json::json!({
                "brand": brand.brand,
                "version": format!("{}.{}", brand.version, tail),
            })
        })
        .collect();
    Some(serde_json::json!({
        "brands": capture.brands,
        "fullVersionList": full_version_list,
        "fullVersion": version,
        "platform": capture.platform,
        "platformVersion": platform_version,
        "architecture": architecture,
        "model": "",
        "mobile": capture.mobile,
        "bitness": bitness,
        "wow64": false,
    }))
}

/// Chromium's own architecture name for a `std::env::consts::ARCH` value:
/// `arm` on Apple Silicon (measured: what the browser reports), `x86` on
/// Intel. `None` for an architecture nothing here has measured.
pub fn client_hint_architecture(arch: &str) -> Option<&'static str> {
    match arch {
        "aarch64" | "arm" => Some("arm"),
        "x86_64" | "x86" => Some("x86"),
        _ => None,
    }
}

/// The macOS product version (`26.5.2`), which is the client hints'
/// `platformVersion` and **not** the kernel's own Darwin release: measured,
/// `sw_vers -ProductVersion` and the browser's read agree, and the kernel
/// version would say `25.5.0` for the same machine.
pub async fn platform_version() -> Result<String, String> {
    let answer = tokio::time::timeout(
        VERSION_TIMEOUT,
        tokio::process::Command::new("sw_vers").arg("-productVersion").output(),
    )
    .await
    .map_err(|_| format!("sw_vers did not answer within {} s", VERSION_TIMEOUT.as_secs()))?
    .map_err(|why| format!("sw_vers could not be run: {why}"))?;
    let said = String::from_utf8_lossy(&answer.stdout);
    product_version(&said).ok_or_else(|| format!("sw_vers named no version: {said:?}"))
}

/// The version line `sw_vers -productVersion` prints - dotted, all-numeric -
/// or `None` for anything else.
fn product_version(said: &str) -> Option<String> {
    let line = said.lines().next()?.trim();
    let dotted = line.contains('.')
        && line.split('.').all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()));
    dotted.then(|| line.to_owned())
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
struct LaunchLock {
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
    /// the launch deadline plus slack.
    async fn acquire(profile: &Path) -> Result<Self, String> {
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
pub async fn launch_with(
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
    // **The machine-wide guard, held until the port answers.** Another
    // process may be mid-launch on this very directory: waiting for its lock
    // and then re-checking means this call attaches to the browser it
    // started, instead of deleting its port file and spawning onto a locked
    // profile.
    let _guard = LaunchLock::acquire(profile).await?;
    if let Some(active) = verified(profile).await {
        // **Only a launch that already has a window is a headed answer.** A
        // headed relaunch that waited behind another host's headless launch
        // would otherwise answer Ok with no window and no marker written -
        // and Open's whole promise is the window. The live launch is closed
        // first: it holds the profile, and a bare fall-through would spawn
        // onto it. The adopt itself stays for the launches a headed one can
        // honestly attach to (and for every headless ask), which is what
        // keeps a relaunch that waited from spawning onto a locked profile.
        if !headed || launched_windowed(profile) {
            return Ok(active);
        }
        close_under_lock(profile, active.port).await;
    }
    // A launch about to happen owns the wires: an older port file would be
    // read as this one's, and an older pid names a process this launch is not.
    forget_launch(profile);
    fresh_session(profile);

    // Only a headless launch needs the UA: a headed one presents the
    // browser's real user agent already. Read here rather than earlier, so
    // a launch that adopts the running browser pays nothing for it.
    let user_agent = if headed {
        None
    } else {
        match masked_user_agent(binary).await {
            Ok(user_agent) => Some(user_agent),
            Err(why) => {
                // A launch without the mask must not be a silent one.
                tauri_plugin_log::log::warn!(
                    "the headless user agent is unmasked (event_name browser_mask_user_agent): \
                     {why}"
                );
                None
            }
        }
    };

    let mut command = tokio::process::Command::new(binary);
    for arg in launch_args(profile, headed, page, user_agent.as_deref()) {
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

/// Whether a WINDOW is really up on `profile`: the headed marker AND a live
/// launch answering as this profile's own AND a page left to carry.
///
/// **The marker alone lied once** (Ved, 2026-10-07, live): he closed the
/// window with the X, the process survived with the marker still set, and
/// the next Open concluded a window was up and raised nothing. `show` keeps
/// this same predicate, and everything that SPEAKS for a window - the
/// strip's window-open mark and its hide label - must keep it too.
pub async fn windowed(profile: &Path) -> bool {
    if !launched_windowed(profile) {
        return false;
    }
    let Some(active) = verified(profile).await else {
        return false;
    };
    page_url(active.port).await.is_some()
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
    // **The launch lock, so a close cannot eat a launch in flight.** An
    // unlocked closer reads "not answering" for a browser coming up, forgets
    // its port file, and that launch then kills the browser it just started
    // at its own deadline. The take is bounded like every other; a lock that
    // cannot be taken at all leaves the close running - no launch can be in
    // flight behind a lock nobody could take.
    let _guard = LaunchLock::acquire(profile).await;
    close_under_lock(profile, port).await
}

/// The close body, for callers that already hold the launch lock (a launch's
/// own headed relaunch closes the launch it found under its own guard).
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
        let args = launch_args(Path::new("/tmp/forge-profile"), false, None, None);
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
            launch_args(Path::new("/tmp/forge-profile"), true, Some("https://example.com/x"), None);
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
        let headless = launch_args(Path::new("/tmp/forge-profile"), false, None, None);
        let headed = launch_args(Path::new("/tmp/forge-profile"), true, None, None);
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

    /// **The webdriver tell is switched off on both launches.** Measured
    /// through the real driver: with the flag, `navigator.webdriver` reads
    /// false from the engine's own native getter - no page patch, so nothing
    /// for a page to notice - and the headed launch needs it exactly as the
    /// headless one does.
    #[test]
    fn a_launch_switches_the_automation_tell_off() {
        for headed in [false, true] {
            let args = launch_args(Path::new("/tmp/forge-profile"), headed, None, None);
            assert!(
                args.contains(&"--disable-blink-features=AutomationControlled".to_owned()),
                "webdriver must read false from the engine itself (headed: {headed}): {args:?}",
            );
        }
    }

    /// **The headless UA mask: the browser's own string, and the client
    /// hints kept.** Measured: the UA override alone blanks `sec-ch-ua` on
    /// every request, and the pair leaves the real hints beside the clean
    /// string. A headed launch passes none of it - its UA is the real one -
    /// and an unmasked launch (the version unreadable) adds nothing either.
    #[test]
    fn a_headless_launch_presents_the_masked_user_agent() {
        let user_agent = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                          (KHTML, like Gecko) Chrome/155.0.0.0 Safari/537.36";
        let headless = launch_args(Path::new("/tmp/forge-profile"), false, None, Some(user_agent));
        assert!(
            headless.contains(&format!("--user-agent={user_agent}")),
            "the header a server reads is fixed by the launch's own switch: {headless:?}",
        );
        assert!(
            headless.contains(&"--disable-features=UACHOverrideBlank".to_owned()),
            "without it the override blanks the client hints: {headless:?}",
        );
        assert_eq!(headless.last().map(String::as_str), Some("about:blank"), "{headless:?}");

        let carried =
            launch_args(Path::new("/tmp/forge-profile"), true, Some("https://example.com"), None);
        assert!(
            !carried.iter().any(|arg| arg.starts_with("--user-agent=")),
            "a headed launch presents the browser's real user agent: {carried:?}",
        );
        assert!(!carried.iter().any(|arg| arg.starts_with("--disable-features=")), "{carried:?}",);

        let unmasked = launch_args(Path::new("/tmp/forge-profile"), false, None, None);
        assert!(
            !unmasked.iter().any(|arg| arg.starts_with("--user-agent=")),
            "a launch whose version could not be read adds nothing: {unmasked:?}",
        );
    }

    /// The reduced UA is built from the browser's own version line, with
    /// Chromium's frozen platform and version tail - the string the launched
    /// browser's own UA was measured to carry on this machine.
    #[test]
    fn the_masked_user_agent_is_built_from_the_browsers_own_version() {
        let expected = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                        (KHTML, like Gecko) Chrome/155.0.0.0 Safari/537.36";
        assert_eq!(reduced_user_agent("Brave Browser 155.1.97.56\n").as_deref(), Some(expected),);
        assert_eq!(
            reduced_user_agent("Google Chrome 156.0.1234.56\n"),
            Some(
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, \
                 like Gecko) Chrome/156.0.0.0 Safari/537.36"
                    .to_owned(),
            ),
        );

        for gibberish in ["", "\n", "not a version\n", "Brave Browser x.y.z\n", "155\n"] {
            assert_eq!(reduced_user_agent(gibberish), None, "{gibberish:?} names no version");
        }
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
        close(dir.path(), 9544).await;
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
        close(dir.path(), 9544).await;
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "an old dead file is not waited on: {:?}",
            started.elapsed(),
        );
        assert!(!file.exists(), "an old dead port file is forgotten");
    }

    /// **Close takes the launch lock**: a close racing a launch waits it out
    /// rather than reading "not answering" for a browser mid-launch.
    ///
    /// **An OLD port file, so the young-port wait cannot stand in for the
    /// lock**: without the take the close reads "not answering", forgets the
    /// file and finishes in one probe - which is what the timeout below
    /// catches. (With a young file the wait alone keeps the close busy past
    /// the window, and the assertion passes for the wrong reason.)
    #[tokio::test(start_paused = true)]
    async fn a_close_waits_for_a_launch_in_flight() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let file = dir.path().join("DevToolsActivePort");
        std::fs::write(&file, b"9544\n/devtools/browser/x\n").expect("a port file");
        let old = std::time::SystemTime::now() - LAUNCH_TIMEOUT - Duration::from_secs(60);
        let handle = std::fs::OpenOptions::new().write(true).open(&file).expect("the file");
        handle
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .expect("the file's time moves back");
        drop(handle);

        let held = LaunchLock::acquire(dir.path()).await.expect("the lock a launch holds");
        let mut closing = tokio::spawn({
            let path = dir.path().to_path_buf();
            async move { close(&path, 9544).await }
        });
        let finished_while_held =
            tokio::time::timeout(Duration::from_secs(1), &mut closing).await.is_ok();
        assert!(
            !finished_while_held,
            "the close must WAIT on the launch lock - it finished while a launch held it",
        );
        drop(held);
        closing.await.expect("the close task");
    }

    /// **The close HOLDS the launch lock through its body**, not only while
    /// acquiring: the reconcile below it is exactly the window a second
    /// launch would slip into - and a close that acquired and let go
    /// immediately reopens the very race the take exists for.
    ///
    /// The young port file is the window made observable: the body waits on
    /// it, and the lock must stay contended for as long as that wait runs.
    #[tokio::test(start_paused = true)]
    async fn a_close_holds_the_launch_lock_through_its_body() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::write(dir.path().join("DevToolsActivePort"), b"9544\n/devtools/browser/x\n")
            .expect("a young port file");

        let held = LaunchLock::acquire(dir.path()).await.expect("the lock a launch holds");
        let closing = tokio::spawn({
            let path = dir.path().to_path_buf();
            async move { close(&path, 9544).await }
        });
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert!(!closing.is_finished(), "the close waits for the lock a launch holds");

        drop(held);
        let mut contended = false;
        for _ in 0..50 {
            if matches!(try_lock(dir.path()), Locked::Contended) {
                contended = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            contended,
            "the close holds the launch lock through its body - a launch cannot slip in mid-close",
        );
        closing.await.expect("the close task");
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

    /// The version a UA string carries is its own token, reduced or full:
    /// both a `Chrome/` and a `HeadlessChrome/` string name one.
    #[test]
    fn the_version_a_ua_string_carries_is_its_own_token() {
        let reduced = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                       (KHTML, like Gecko) Chrome/155.0.0.0 Safari/537.36";
        assert_eq!(user_agent_version(reduced), Some("155.0.0.0"));

        let headless = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                        (KHTML, like Gecko) HeadlessChrome/153.0.8010.12 Safari/537.36";
        assert_eq!(user_agent_version(headless), Some("153.0.8010.12"));

        assert_eq!(user_agent_version("Chrome/155.0.0.0"), Some("155.0.0.0"));
        assert_eq!(user_agent_version("no browser here"), None, "no token, no version");
        assert_eq!(user_agent_version("Chrome/"), None, "an empty token is no version");
    }

    /// **The rebuild is the exact list the same browser reports without the
    /// override** (measured: Brave 155.1.97.56 headless on macOS 26.5.2
    /// arm64): every brand at its own major with the frozen tail, the
    /// fullVersion the string carries, and the machine's own platform
    /// version, architecture and bitness.
    #[test]
    fn the_rebuilt_metadata_is_what_the_browser_reports() {
        let capture = HintCapture {
            user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                         (KHTML, like Gecko) Chrome/155.0.0.0 Safari/537.36"
                .to_owned(),
            brands: vec![
                HintBrand { brand: "Brave".to_owned(), version: "155".to_owned() },
                HintBrand { brand: "Chromium".to_owned(), version: "155".to_owned() },
                HintBrand { brand: "Not(A:Brand".to_owned(), version: "24".to_owned() },
            ],
            platform: "macOS".to_owned(),
            mobile: false,
            blanked: true,
        };
        let metadata =
            user_agent_metadata(&capture, "26.5.2", "arm", "64").expect("the capture rebuilds");
        assert_eq!(
            metadata,
            serde_json::json!({
                "brands": [
                    { "brand": "Brave", "version": "155" },
                    { "brand": "Chromium", "version": "155" },
                    { "brand": "Not(A:Brand", "version": "24" },
                ],
                "fullVersionList": [
                    { "brand": "Brave", "version": "155.0.0.0" },
                    { "brand": "Chromium", "version": "155.0.0.0" },
                    { "brand": "Not(A:Brand", "version": "24.0.0.0" },
                ],
                "fullVersion": "155.0.0.0",
                "platform": "macOS",
                "platformVersion": "26.5.2",
                "architecture": "arm",
                "model": "",
                "mobile": false,
                "bitness": "64",
                "wow64": false,
            }),
            "the metadata the page reads must be the browser's own, grease brand included",
        );

        // **A capture that cannot support a truthful rebuild answers None**
        // rather than a half-claim: a browser already reading the five for
        // real (an unmasked launch - derived values must not replace them),
        // no brands, or a string naming no version.
        let real = HintCapture { blanked: false, ..capture.clone() };
        assert_eq!(
            user_agent_metadata(&real, "26.5.2", "arm", "64"),
            None,
            "a browser reporting its own high-entropy hints is left exactly as it is",
        );
        let brandless = HintCapture { brands: Vec::new(), ..capture.clone() };
        assert_eq!(user_agent_metadata(&brandless, "26.5.2", "arm", "64"), None);
        let versionless =
            HintCapture { user_agent: "no version in here".to_owned(), ..capture.clone() };
        assert_eq!(user_agent_metadata(&versionless, "26.5.2", "arm", "64"), None);
    }

    /// The architecture mapping: the names Chromium reports, and `None` for
    /// anything unmeasured rather than a guess.
    #[test]
    fn the_architecture_mapping_names_what_chromium_reports() {
        assert_eq!(client_hint_architecture("aarch64"), Some("arm"));
        assert_eq!(client_hint_architecture("x86_64"), Some("x86"));
        assert_eq!(
            client_hint_architecture("riscv64"),
            None,
            "an unmeasured architecture is no claim at all",
        );
    }

    /// `sw_vers -productVersion`'s answer, by shape: a dotted all-numeric
    /// version, and anything else is a version that cannot be claimed.
    #[test]
    fn the_product_version_is_read_by_shape() {
        assert_eq!(product_version("26.5.2\n"), Some("26.5.2".to_owned()));
        assert_eq!(product_version("15.6\n"), Some("15.6".to_owned()));

        for gibberish in ["", "\n", "not a version\n", "26\n", "26..5\n", "26.5.2beta\n"] {
            assert_eq!(product_version(gibberish), None, "{gibberish:?} names no version");
        }
    }
}
