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
//! this is measured rather than assumed.** Chrome for Testing 155 writes
//! `DevToolsActivePort` when it is asked for `--remote-debugging-port=0` and
//! NOT when it is given a port number: asked for 9333 by hand it listened on
//! 9333 and wrote no file at all. A fixed number would therefore have been a
//! host that could never find its browser again, and it would also collide
//! with whichever browser-automation tooling already holds the conventional
//! ports (the user's own Brave is on 9222).

use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a launch is given to write its port file and answer.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(15);

/// Where the stack's Chromium is, inside the vendored tree.
pub fn chrome_binary(stack: &Path) -> PathBuf {
    stack
        .join("browser/chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing")
}

/// The argument list one launch runs with.
///
/// `profile` is the browser's own directory, which is what carries logins,
/// cookies and the HTTP cache across everything. Headless is the default and
/// not a setting yet: the visible window is a switch the status row will
/// carry, and until it exists nothing here needs a window.
pub fn launch_args(profile: &Path) -> Vec<String> {
    vec![
        // Let the browser choose, and read the choice from its port file:
        // handed a number it writes no file, which is a browser nothing can
        // find again.
        "--remote-debugging-port=0".to_owned(),
        format!("--user-data-dir={}", profile.display()),
        "--headless".to_owned(),
        // **Never the OS keychain.** This profile is forge's own and holds
        // nothing worth a keychain entry, while reaching for one raises a
        // system dialog - "Google Chrome for Testing wants to use your
        // confidential information stored in Chromium Safe Storage", once per
        // ask - that nobody is sitting in front of a headless browser to
        // answer, and that spams whoever is at the machine. The mock store
        // keeps the same shape with a key that goes nowhere.
        "--use-mock-keychain".to_owned(),
        "--password-store=basic".to_owned(),
        // A first-run flow is a dialog nothing can see and nothing answers.
        "--no-first-run".to_owned(),
        "--no-default-browser-check".to_owned(),
        // A page to drive: with no tab at all, the first navigation depends
        // on the driver inventing one.
        "about:blank".to_owned(),
    ]
}

/// One launch, as `DevToolsActivePort` records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivePort {
    pub port: u16,
    /// The browser target's path, `/devtools/browser/<uuid>`.
    pub path: String,
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
    Some(ActivePort { port, path: path.to_owned() })
}

/// Whether the port a launch asked for is answered.
///
/// A real probe rather than a socket that accepts: `/json/version` is the
/// endpoint Chrome serves for exactly this question, and something else
/// holding the port answers it with nothing that parses.
pub async fn probe(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{port}/json/version");
    let answer = tokio::task::spawn_blocking(move || http_get(&url)).await;
    answer.ok().flatten().is_some_and(|body| body.contains("\"webSocketDebuggerUrl\""))
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

/// Launch the vendored Chromium against `profile`, and answer once it is
/// answering on its port.
///
/// Detached on purpose: the child is left running when this drops - its
/// stdout and stderr go nowhere a client reads, and its lifetime is the
/// machine's, not the client run's.
pub async fn launch(binary: &Path, profile: &Path) -> Result<ActivePort, String> {
    if !binary.is_file() {
        return Err(format!(
            "the vendored browser is not there at {} - run `just vendor-browser-stack`",
            binary.display(),
        ));
    }
    std::fs::create_dir_all(profile)
        .map_err(|why| format!("the browser profile directory cannot be made: {why}"))?;
    // A port file from an older launch would be read as this one's.
    let _ = std::fs::remove_file(profile.join("DevToolsActivePort"));

    let mut command = tokio::process::Command::new(binary);
    for arg in launch_args(profile) {
        command.arg(arg);
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let child = command
        .spawn()
        .map_err(|why| format!("the vendored browser would not start: {why}"))?;
    // Dropped rather than waited on: the browser is meant to outlive this
    // call, and this process reaps nothing it did not spawn as its own work.
    drop(child);

    let deadline = tokio::time::Instant::now() + LAUNCH_TIMEOUT;
    loop {
        if let Some(active) = read_active_port(profile)
            && probe(active.port).await
        {
            return Ok(active);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "the vendored browser did not answer on its port within {} s",
                LAUNCH_TIMEOUT.as_secs(),
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The browser to talk to: the one already running on `profile`, launched if
/// there is none.
pub async fn ensure(binary: &Path, profile: &Path) -> Result<ActivePort, String> {
    if let Some(active) = read_active_port(profile)
        && probe(active.port).await
    {
        return Ok(active);
    }
    launch(binary, profile).await
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
            Some(ActivePort { port: 9333, path: "/devtools/browser/9d1f-77aa".to_owned() }),
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
    /// Chrome for Testing 155 writes no port file and nothing can find the
    /// launch again - carries the profile that keeps logins, opens a page
    /// (because a browser with no tab makes the first navigation depend on
    /// the driver inventing one), and **never reaches for the OS keychain**:
    /// that dialog is raised at whoever is at the machine, once per ask, and
    /// a headless browser has nobody to answer it.
    #[test]
    fn a_launch_lets_the_browser_choose_its_port_and_carries_its_profile_and_a_page() {
        let args = launch_args(Path::new("/tmp/forge-profile"));
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

        assert_eq!(whole_body_len(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n"), None, "no body yet");
        assert_eq!(whole_body_len(b"HTTP/1.1 200 OK\r\n\r\nhello"), None, "no length named");
    }

    /// The stack's browser is where the vendoring puts it: a path that moved
    /// is a host that cannot start, so it is pinned by name.
    #[test]
    fn the_chromium_is_the_bundle_the_vendoring_unpacks() {
        assert_eq!(
            chrome_binary(Path::new("/stack")),
            Path::new(
                "/stack/browser/chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing"
            ),
        );
    }
}
