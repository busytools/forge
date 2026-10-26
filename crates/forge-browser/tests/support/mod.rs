//! Pieces the browser-host integration tests share: staging the real bundle
//! and starting a host from it.
//!
//! Each test file is its own crate, so this module is compiled per file and
//! a helper only one of them uses is not dead code worth chasing.

#![allow(dead_code, reason = "each test crate uses a different subset of these helpers")]
#![allow(unsafe_code, reason = "a test ends its own child with a signal")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// The workspace root this test binary was built from.
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The CEF distribution this workspace's build unpacked.
///
/// **Found by walking, not by a fixed layout**: a test binary's own path has
/// moved between cargo and nextest, and the distribution is whichever
/// `cef-dll-sys` build output sits under an ancestor's `build` directory.
pub fn cef_distribution() -> PathBuf {
    let exe = std::env::current_exe().expect("the test binary's path");
    for ancestor in exe.ancestors() {
        let build = ancestor.join("build");
        if !build.is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&build) else { continue };
        for entry in entries.flatten() {
            if !entry.file_name().to_string_lossy().starts_with("cef-dll-sys") {
                continue;
            }
            let Ok(hashes) = std::fs::read_dir(entry.path()) else { continue };
            for hash in hashes.flatten() {
                let out = hash.path().join("out/cef_macos_aarch64");
                if out.join("Chromium Embedded Framework.framework").is_dir() {
                    return out;
                }
            }
        }
    }
    // **CI unpacks the distribution into its download cache**, outside the
    // target tree the walk above covers: cef-dll-sys puts it under
    // `CEF_PATH/<version>` when that variable is set.
    if let Ok(cache) = std::env::var("CEF_PATH")
        && let Some(found) = find_distribution(std::path::Path::new(&cache))
    {
        return found;
    }
    panic!("the CEF distribution is not under any ancestor of {}", exe.display());
}

/// The first `cef_macos_aarch64` directory with a framework under `root`.
fn find_distribution(root: &std::path::Path) -> Option<PathBuf> {
    let mut queue = vec![root.to_path_buf()];
    while let Some(dir) = queue.pop() {
        if dir.file_name().is_some_and(|name| name == "cef_macos_aarch64")
            && dir.join("Chromium Embedded Framework.framework").is_dir()
        {
            return Some(dir);
        }
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        queue.extend(
            entries
                .flatten()
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                .map(|entry| entry.path()),
        );
    }
    None
}

/// The app's executable: an installed bundle when one is named, otherwise a
/// scratch staging of the workspace's own binary.
pub fn host_executable(scratch: &Path) -> PathBuf {
    match std::env::var("FORGE_BROWSER_TEST_BUNDLE") {
        Ok(app) => Path::new(&app).join("Contents/MacOS/Forge Browser"),
        Err(_) => stage_bundle(scratch),
    }
}

fn stage_bundle(scratch: &Path) -> PathBuf {
    let script = repo_root().join("scripts/stage-browser-host.sh");
    let output = Command::new("bash")
        .arg(&script)
        .env("CEF_DIR", cef_distribution())
        .env("FORGE_BROWSER_BIN", env!("CARGO_BIN_EXE_forge-browser"))
        .env("FORGE_BROWSER_HOST_DIR", scratch)
        .output()
        .expect("the staging script runs");
    assert!(
        output.status.success(),
        "the staging script failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    scratch.join("Forge Browser.app/Contents/MacOS/Forge Browser")
}

/// A port nothing holds, chosen now and handed to the host.
pub fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("a free port");
    listener.local_addr().expect("the bound address").port()
}

/// A host this test started: ended if the test fails before it does.
pub struct Host {
    child: Child,
    log: PathBuf,
    /// The host's control lines, when the test reads them.
    events: Option<std::sync::Arc<std::sync::Mutex<Vec<String>>>>,
}

impl Host {
    /// Start a host from `exe`, with its stderr in `log` and `args` after
    /// the ones every test passes.
    pub fn start(
        exe: &Path,
        profile: &Path,
        port: u16,
        ring: &str,
        size: &str,
        args: &[String],
        log: &Path,
    ) -> Self {
        Self::launch(exe, profile, port, ring, size, args, log, false)
    }

    /// Start a host whose control lines the test reads, one per line.
    pub fn start_with_events(
        exe: &Path,
        profile: &Path,
        port: u16,
        ring: &str,
        size: &str,
        args: &[String],
        log: &Path,
    ) -> Self {
        Self::launch(exe, profile, port, ring, size, args, log, true)
    }

    fn launch(
        exe: &Path,
        profile: &Path,
        port: u16,
        ring: &str,
        size: &str,
        args: &[String],
        log: &Path,
        capture_events: bool,
    ) -> Self {
        let mut command = Command::new(exe);
        command
            .arg("--cef-host")
            .arg("--profile")
            .arg(profile)
            .arg("--port")
            .arg(port.to_string())
            .arg("--ring")
            .arg(ring)
            .arg("--size")
            .arg(size)
            .args(args)
            // **The pipe is the server's input path**, so a test can drive a
            // gesture the way the server does. A test that never writes
            // leaves it open and idle.
            .stdin(Stdio::piped())
            .stdout(if capture_events { Stdio::piped() } else { Stdio::null() })
            .stderr(Stdio::from(std::fs::File::create(log).expect("the host log")));
        let mut child = command.spawn().expect("the host starts");
        let events = child.stdout.take().map(|stdout| {
            let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = std::sync::Arc::clone(&lines);
            std::thread::spawn(move || {
                use std::io::BufRead as _;
                for line in std::io::BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    sink.lock().expect("the event lines").push(line);
                }
            });
            lines
        });
        Self { child, log: log.to_path_buf(), events }
    }

    /// Every control line the host has written so far.
    pub fn events(&self) -> Vec<String> {
        self.events
            .as_ref()
            .map(|lines| lines.lock().expect("the event lines").clone())
            .unwrap_or_default()
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn try_wait(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().expect("the child's status")
    }

    /// Write one pointer gesture on the host's own input pipe, exactly as
    /// the server does.
    pub fn input(&mut self, line: &str) {
        use std::io::Write as _;
        let stdin = self.child.stdin.as_mut().expect("the host's input pipe");
        writeln!(stdin, "{line}").expect("the input line is written");
        stdin.flush().expect("the input line is flushed");
    }

    pub fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// End the host the way the server does, and wait for it to go.
    pub fn terminate(&mut self, bound: std::time::Duration) -> std::process::ExitStatus {
        // SAFETY: a plain signal to this test's own child.
        unsafe { libc::kill(i32::try_from(self.pid()).unwrap_or(i32::MAX), libc::SIGTERM) };
        let deadline = std::time::Instant::now() + bound;
        loop {
            if let Some(status) = self.try_wait() {
                return status;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the host did not exit within {bound:?} of SIGTERM (log: {})",
                self.log()
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One HTTP GET over a plain socket.
///
/// **A server that holds the socket open is an answer too**: the read is
/// bounded and stops as soon as the body is a whole JSON array, so
/// `/json/list` does not wait out the host.
pub fn http_get(url: &str) -> Option<String> {
    use std::io::{Read as _, Write as _};
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = rest.split_once('/').map_or((rest, ""), |(a, p)| (a, p));
    let mut stream = std::net::TcpStream::connect(authority).ok()?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(2))).ok()?;
    stream
        .write_all(
            format!("GET /{path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .ok()?;
    let mut answer = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => answer.extend_from_slice(&chunk[..read]),
            Err(why) if why.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
        if let Some((_, body)) = String::from_utf8_lossy(&answer).split_once("\r\n\r\n")
            && body.trim_end().ends_with(']')
        {
            break;
        }
    }
    String::from_utf8(answer).ok()
}

/// The page target's DevTools id, once the host answers `/json/list`.
pub fn wait_for_page(port: u16, log: &Path, bound: std::time::Duration) -> String {
    let deadline = std::time::Instant::now() + bound;
    loop {
        if let Some(answer) = http_get(&format!("http://127.0.0.1:{port}/json/list"))
            && let Some((_, body)) = answer.split_once("\r\n\r\n")
            && let Ok(entries) = serde_json::from_str::<serde_json::Value>(body)
            && let Some(page) = entries
                .as_array()
                .and_then(|entries| {
                    entries.iter().find(|entry| {
                        entry.get("type").and_then(|kind| kind.as_str()) == Some("page")
                    })
                })
                .and_then(|page| page.get("id").and_then(|id| id.as_str()))
        {
            return page.to_owned();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the host's page target never answered on port {port} (log: {})",
            std::fs::read_to_string(log).unwrap_or_default(),
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}
