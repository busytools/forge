//! The client-hint mask against the vendored stack (#1938): on a page driven
//! through the client's engine, the high-entropy hints read what the same
//! browser reports WITHOUT the override.
//!
//! **Deliberately ignored**, like the other live tests: it needs
//! `just vendor-browser-stack` to have run and it drives the machine's own
//! browser. Run it where the stack is:
//!
//! ```text
//! cargo nextest run --manifest-path client/src-tauri/Cargo.toml \
//!     --run-ignored ignored-only -E 'test(the_high_entropy_hints)'
//! ```
//!
//! Two browsers, ONE binary: the control is launched with the same arguments
//! minus the mask's UA pair and driven by the same driver, so "matching the
//! same browser without the override" is measured, not asserted from memory.
//! Both browsers are headless; no window touches the machine.

mod support;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use forge_client::browser::driver::{Driver, ReplyPart, hints_payload};
use forge_client::browser::{BrowserHost, StackPaths, chromium};
use serde_json::{Value, json};
use support::Launched;

/// Print the host's own tracing to stderr, so a failure on one of the
/// host's best-effort steps says WHICH step and why: without a logger
/// installed every `warn` the host writes is dropped on the floor.
fn capture_logs() {
    struct Stderr;

    impl log::Log for Stderr {
        fn enabled(&self, _: &log::Metadata) -> bool {
            true
        }
        fn log(&self, record: &log::Record) {
            eprintln!("[{}] {}", record.level(), record.args());
        }
        fn flush(&self) {}
    }

    let _ = log::set_boxed_logger(Box::new(Stderr));
    log::set_max_level(log::LevelFilter::Info);
}

/// The high-entropy five the override blanks and the mask rebuilds.
const HINTS: [&str; 5] =
    ["architecture", "bitness", "platformVersion", "uaFullVersion", "fullVersionList"];

/// The read both browsers answer, behind the same marker the host's own
/// capture uses: the URL it ran on, the UA string, the low-entropy hints and
/// the five.
const READ_HINTS: &str = r#"() => {
  const d = navigator.userAgentData;
  return d.getHighEntropyValues(['architecture', 'bitness', 'platformVersion', 'uaFullVersion', 'fullVersionList'])
    .then((h) => 'FORGE-HINTS ' + JSON.stringify({
      url: location.href,
      user_agent: navigator.userAgent,
      brands: d.brands,
      platform: d.platform,
      mobile: d.mobile,
      high: {
        architecture: h.architecture,
        bitness: h.bitness,
        platformVersion: h.platformVersion,
        uaFullVersion: h.uaFullVersion,
        fullVersionList: h.fullVersionList,
      },
    }));
}
"#;

/// The five out of one read, as the acceptance compares them.
fn five(read: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for name in HINTS {
        out.insert(name.to_owned(), read["high"][name].clone());
    }
    Value::Object(out)
}

/// The text one answer carries, which is every part that is not an image.
fn text_of(parts: &[ReplyPart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            ReplyPart::Text { text } => Some(text.clone()),
            ReplyPart::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn read_of(parts: &[ReplyPart], what: &str) -> Value {
    hints_payload(&text_of(parts))
        .unwrap_or_else(|why| panic!("{what}: the read did not parse: {why}"))
}

fn seat() -> forge_client::browser::profiles::Seat {
    forge_client::browser::profiles::Seat {
        org: "Busytools".to_owned(),
        project: "forge".to_owned(),
        label: "browser-hints".to_owned(),
    }
}

/// Every request's `sec-ch-ua*` headers, keyed by its path and query.
type Headers = Arc<Mutex<BTreeMap<String, BTreeMap<String, String>>>>;

/// A local origin that asks for the high-entropy hints: the page's response
/// carries `Accept-CH`, and the test fetches `/second` once the page is up.
/// Every request's client-hint headers are recorded under its path and query.
async fn hints_origin() -> (String, Headers) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let seen: Headers = Arc::new(Mutex::new(BTreeMap::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let port = listener.local_addr().expect("the bound port").port();
    let recorded = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { continue };
            let mut buffer = [0_u8; 8192];
            let read = socket.read(&mut buffer).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..read]).to_owned();
            let target = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
            let mut hints = BTreeMap::new();
            for line in request.lines().skip(1) {
                let Some((name, value)) = line.split_once(':') else { continue };
                let name = name.trim().to_ascii_lowercase();
                if name.starts_with("sec-ch-ua") {
                    hints.insert(name, value.trim().to_owned());
                }
            }
            if !hints.is_empty() {
                recorded.lock().expect("the log").insert(target.clone(), hints);
            }
            let body = "<title>hints</title><h1>again</h1>";
            let mut answer = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\
                 Connection: close\r\n",
                body.len(),
            );
            if target == "/" || target.starts_with("/?") {
                answer.push_str(
                    "Accept-CH: Sec-CH-UA-Arch, Sec-CH-UA-Bitness, Sec-CH-UA-Platform-Version, \
                     Sec-CH-UA-Full-Version-List, Sec-CH-UA-Full-Version\r\n",
                );
            }
            answer.push_str(&format!("\r\n{body}"));
            let _ = socket.write_all(answer.as_bytes()).await;
        }
    });
    (format!("http://127.0.0.1:{port}/"), seen)
}

/// Bring a browser up by hand with the given arguments and answer its port
/// and pid, so the caller can hold it with a reaping guard.
async fn hand_launch(
    binary: &std::path::Path,
    profile: &std::path::Path,
    args: Vec<String>,
) -> (u16, u32) {
    std::fs::create_dir_all(profile).expect("the profile");
    let child = std::process::Command::new(binary)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the browser starts");
    let pid = child.id();
    drop(child);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        if let Some(active) = chromium::read_active_port(profile)
            && chromium::probe(active.port).await
        {
            return (active.port, pid);
        }
        assert!(std::time::Instant::now() < deadline, "the browser never answered");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// **The acceptance.** On a page driven through the client's engine - the
/// host, the vendored driver, the launched browser - `architecture`,
/// `bitness`, `platformVersion`, `uaFullVersion` and `fullVersionList` read
/// their real values, matching the same browser without the override; the
/// launch's masked UA string and the low-entropy hints stay what they were;
/// the high-entropy request headers come back real beside them; and a page
/// opened later is masked the moment it exists.
#[tokio::test]
#[ignore = "drives the vendored stack; needs `just vendor-browser-stack`"]
async fn the_high_entropy_hints_match_the_unmasked_browser() {
    capture_logs();
    let stack = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("browser-stack");
    assert!(
        stack.join("node/bin/node").is_file(),
        "the vendored stack is not there - run `just vendor-browser-stack`",
    );
    let node = forge_client::browser::driver::node_path(&stack);
    let cli = forge_client::browser::driver::cli_path(&stack);
    let binary = chromium::browser_binary().expect("a browser on this machine");

    let dir = tempfile::tempdir().expect("a temp dir");
    let output = dir.path().join("output");
    let (origin, headers) = hints_origin().await;
    let fetch_second =
        |who: &str| format!("() => fetch('/second?who={who}').then((r) => r.text())");

    // The CONTROL: forge's own launch arguments minus the mask's UA pair, the
    // same browser and the same driver - what "without the override" reads.
    let control_profile = dir.path().join("control");
    let (control_port, control_pid) = hand_launch(
        &binary,
        &control_profile,
        chromium::launch_args(&control_profile, false, None, None),
    )
    .await;
    let control = Launched::new(Some(control_pid), control_port, control_profile.clone());
    let control_driver =
        Driver::start(&node, &cli, &format!("http://127.0.0.1:{control_port}"), &output)
            .await
            .unwrap_or_else(|why| panic!("the control driver: {why}"));
    control_driver
        .call("browser_navigate", json!({ "url": format!("{origin}?who=control") }))
        .await
        .unwrap_or_else(|why| panic!("the control page: {why}"));
    let control_read = read_of(
        &control_driver
            .call("browser_evaluate", json!({ "function": READ_HINTS }))
            .await
            .unwrap_or_else(|why| panic!("the control read: {why}")),
        "the control",
    );
    control_driver
        .call("browser_evaluate", json!({ "function": fetch_second("control") }))
        .await
        .unwrap_or_else(|why| panic!("the control fetch: {why}"));

    // The SUBJECT: everything the app does - launch, driver, hint mask.
    let paths = StackPaths {
        stack,
        user_data: dir.path().join("user-data"),
        output: output.clone(),
        profiles: dir.path().join("profiles"),
    };
    let host = BrowserHost::new(paths.clone());
    let active = host.start().await.expect("the browser comes up");
    let browser = Launched::new(active.pid, active.port, paths.user_data.clone());
    host.call(&seat(), "browser_navigate", json!({ "url": format!("{origin}?who=subject") }))
        .await
        .unwrap_or_else(|why| panic!("the masked page: {why}"));
    let subject_read = read_of(
        &host
            .call(&seat(), "browser_evaluate", json!({ "function": READ_HINTS }))
            .await
            .unwrap_or_else(|why| panic!("the masked read: {why}")),
        "the masked page",
    );
    host.call(&seat(), "browser_evaluate", json!({ "function": fetch_second("subject") }))
        .await
        .unwrap_or_else(|why| panic!("the masked fetch: {why}"));

    // A page opened LATER is masked the moment it exists (the context's page
    // listener), not only the pages open at install.
    host.call(
        &seat(),
        "browser_tabs",
        json!({ "action": "new", "url": format!("{origin}?who=fresh") }),
    )
    .await
    .unwrap_or_else(|why| panic!("a new tab: {why}"));
    let fresh_read = read_of(
        &host
            .call(&seat(), "browser_evaluate", json!({ "function": READ_HINTS }))
            .await
            .unwrap_or_else(|why| panic!("the new page's read: {why}")),
        "the new page",
    );

    browser.reap();
    control.reap();

    assert!(
        fresh_read["url"].as_str().unwrap_or_default().contains("who=fresh"),
        "the later-page read must run on the page the driver opened: {fresh_read}",
    );

    // **The five, against the same browser without the override.**
    assert_eq!(
        five(&subject_read),
        five(&control_read),
        "the masked page must read exactly what the same browser reads unmasked",
    );
    for name in ["architecture", "bitness", "platformVersion", "uaFullVersion"] {
        assert_ne!(
            subject_read["high"][name],
            json!(""),
            "the {name} hint is empty - the override's blank is back",
        );
    }
    assert!(
        !subject_read["high"]["fullVersionList"].as_array().unwrap_or(&Vec::new()).is_empty(),
        "fullVersionList must carry the brands, not the override's []: {subject_read}",
    );

    // The launch's own half stays as it was: the masked UA string, and the
    // low-entropy hints real.
    let ua = subject_read["user_agent"].as_str().unwrap_or_default();
    assert!(ua.contains("Chrome/") && !ua.contains("HeadlessChrome"), "the masked UA: {ua}");
    assert_eq!(subject_read["brands"], control_read["brands"], "the low-entropy brands stay real");
    assert_eq!(subject_read["platform"], control_read["platform"]);
    assert_eq!(subject_read["mobile"], control_read["mobile"]);

    // The headers half of the same tell: what a server asked for via
    // `Accept-CH` is real, and equal to the unmasked browser's own.
    let seen = headers.lock().expect("the log").clone();
    let control_headers = seen.get("/second?who=control").cloned().unwrap_or_default();
    let subject_headers = seen.get("/second?who=subject").cloned().unwrap_or_default();
    assert!(
        !control_headers.is_empty(),
        "the control's Accept-CH fetch carried client-hint headers: {seen:?}",
    );
    assert_eq!(
        subject_headers, control_headers,
        "the masked page's high-entropy headers must equal the unmasked browser's: {seen:?}",
    );

    // And the page opened later reads the same real five.
    assert_eq!(five(&fresh_read), five(&control_read), "a page opened later is masked too");
}

/// The newest Chrome for Testing binary in the playwright cache, when one is
/// there: the stand-in for the Chrome arm on a machine whose own browser is
/// Brave.
fn chrome_for_testing() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let cache = PathBuf::from(home).join("Library/Caches/ms-playwright");
    let mut found: Vec<(u64, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(cache).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(age) = name.strip_prefix("chromium-").and_then(|n| n.parse::<u64>().ok()) else {
            continue;
        };
        let binary = entry.path().join(
            "chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/\
             Google Chrome for Testing",
        );
        if binary.is_file() {
            found.push((age, binary));
        }
    }
    found.sort();
    found.pop().map(|(_, binary)| binary)
}

/// **The Chrome arm, re-measured live.** The derivation that keeps Brave's
/// frozen version would be wrong for every other Chromium build, so this
/// drives the same masked-versus-unmasked pair against a Chrome for Testing
/// binary: its string is frozen to `153.0.0.0` while its hints read the true
/// build `153.0.8010.12`, and the rebuild must reproduce exactly that. The
/// branded Google Chrome build stays unmeasured - no Chrome is installed on
/// this machine.
#[tokio::test]
#[ignore = "drives a Chrome for Testing build from the playwright cache"]
async fn the_high_entropy_hints_match_a_chrome_build() {
    capture_logs();
    let stack = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("browser-stack");
    assert!(
        stack.join("node/bin/node").is_file(),
        "the vendored stack is not there - run `just vendor-browser-stack`",
    );
    let node = forge_client::browser::driver::node_path(&stack);
    let cli = forge_client::browser::driver::cli_path(&stack);
    let chrome = chrome_for_testing().unwrap_or_else(|| {
        panic!(
            "no Chrome for Testing build in the playwright cache - install one there \
             (~/Library/Caches/ms-playwright/chromium-*/...) or run this where one is"
        )
    });

    let dir = tempfile::tempdir().expect("a temp dir");
    let output = dir.path().join("output");
    let (origin, _headers) = hints_origin().await;

    // The control: the same Chrome build, unmasked.
    let control_profile = dir.path().join("chrome-control");
    let (control_port, control_pid) = hand_launch(
        &chrome,
        &control_profile,
        chromium::launch_args(&control_profile, false, None, None),
    )
    .await;
    let control = Launched::new(Some(control_pid), control_port, control_profile.clone());
    let control_driver =
        Driver::start(&node, &cli, &format!("http://127.0.0.1:{control_port}"), &output)
            .await
            .unwrap_or_else(|why| panic!("the chrome control driver: {why}"));
    control_driver
        .call("browser_navigate", json!({ "url": origin }))
        .await
        .unwrap_or_else(|why| panic!("the chrome control page: {why}"));
    let control_read = read_of(
        &control_driver
            .call("browser_evaluate", json!({ "function": READ_HINTS }))
            .await
            .unwrap_or_else(|why| panic!("the chrome control read: {why}")),
        "the chrome control",
    );

    // The subject: the same Chrome build under the launch's mask, with the
    // hint mask installed the way a driver start installs it.
    let subject_profile = dir.path().join("chrome-subject");
    let ua = chromium::masked_user_agent(&chrome)
        .await
        .unwrap_or_else(|why| panic!("the chrome UA would not build: {why}"));
    let (subject_port, subject_pid) = hand_launch(
        &chrome,
        &subject_profile,
        chromium::launch_args(&subject_profile, false, None, Some(&ua)),
    )
    .await;
    let subject = Launched::new(Some(subject_pid), subject_port, subject_profile.clone());
    let subject_driver =
        Driver::start(&node, &cli, &format!("http://127.0.0.1:{subject_port}"), &output)
            .await
            .unwrap_or_else(|why| panic!("the chrome subject driver: {why}"));
    subject_driver.install_hint_mask(&output, "chrome-subject").await;
    subject_driver
        .call("browser_navigate", json!({ "url": origin }))
        .await
        .unwrap_or_else(|why| panic!("the chrome subject page: {why}"));
    let subject_read = read_of(
        &subject_driver
            .call("browser_evaluate", json!({ "function": READ_HINTS }))
            .await
            .unwrap_or_else(|why| panic!("the chrome subject read: {why}")),
        "the chrome subject",
    );

    subject.reap();
    control.reap();

    assert_eq!(
        five(&subject_read),
        five(&control_read),
        "the masked Chrome build must read exactly what the same build reads unmasked",
    );
    let ua_read = subject_read["user_agent"].as_str().unwrap_or_default();
    assert!(
        ua_read.contains("Chrome/") && !ua_read.contains("HeadlessChrome"),
        "the launch's masked UA string stays: {ua_read}",
    );
    assert_eq!(subject_read["brands"], control_read["brands"], "the brands stay real");
}

/// **The headed skip, pinned without a window.** The hint mask keeps off a
/// headed launch by the one fact such a launch leaves - the profile's
/// `windowed` marker - and its absence is the headless signal; a gate that
/// ignored the marker (or read the wrong profile's) would install the mask
/// on a person's window. Writing the marker by hand puts the code in exactly
/// the state a headed launch leaves, so the skip itself is asserted on a
/// page the driver really drives: the five stay the override's blanks,
/// while the launch's own UA mask is untouched.
#[tokio::test]
#[ignore = "drives the vendored stack; needs `just vendor-browser-stack`"]
async fn the_hint_mask_keeps_off_a_windowed_launch() {
    capture_logs();
    let stack = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("browser-stack");
    assert!(
        stack.join("node/bin/node").is_file(),
        "the vendored stack is not there - run `just vendor-browser-stack`",
    );
    let dir = tempfile::tempdir().expect("a temp dir");
    let paths = StackPaths {
        stack,
        user_data: dir.path().join("user-data"),
        output: dir.path().join("output"),
        profiles: dir.path().join("profiles"),
    };
    let (origin, _headers) = hints_origin().await;

    let host = BrowserHost::new(paths.clone());
    let active = host.start().await.expect("the browser comes up");
    let browser = Launched::new(active.pid, active.port, paths.user_data.clone());

    // Written AFTER the launch - a launch forgets whatever marker it finds,
    // and a headed relaunch writes it exactly at this point, with the
    // browser answering.
    std::fs::write(paths.user_data.join("windowed"), b"").expect("the marker");

    host.call(&seat(), "browser_navigate", json!({ "url": origin }))
        .await
        .unwrap_or_else(|why| panic!("the page: {why}"));
    let read = read_of(
        &host
            .call(&seat(), "browser_evaluate", json!({ "function": READ_HINTS }))
            .await
            .unwrap_or_else(|why| panic!("the read: {why}")),
        "the windowed launch",
    );
    browser.reap();

    let ua = read["user_agent"].as_str().unwrap_or_default();
    assert!(
        ua.contains("Chrome/") && !ua.contains("HeadlessChrome"),
        "the launch's own UA mask still applies: {ua}",
    );
    assert_eq!(
        five(&read),
        json!({
            "architecture": "",
            "bitness": "",
            "platformVersion": "",
            "uaFullVersion": "",
            "fullVersionList": [],
        }),
        "the marker makes the driver skip the mask - the five stay the override's blanks: {read}",
    );
}

/// A logger that keeps every record, for a test asserting on the client's
/// own log lines. A process runs one test under nextest, so this can be the
/// process's logger.
fn capture_logs_into() -> Arc<Mutex<Vec<String>>> {
    struct Keep(Arc<Mutex<Vec<String>>>);

    impl log::Log for Keep {
        fn enabled(&self, _: &log::Metadata) -> bool {
            true
        }
        fn log(&self, record: &log::Record) {
            self.0.lock().expect("the log").push(format!("[{}] {}", record.level(), record.args()));
        }
        fn flush(&self) {}
    }

    let kept = Arc::new(Mutex::new(Vec::new()));
    let _ = log::set_boxed_logger(Box::new(Keep(Arc::clone(&kept))));
    log::set_max_level(log::LevelFilter::Info);
    kept
}

/// **The mask's failures reach the client's log.** A page that fails to mask
/// after the install has no return of its own to ride, and the sandbox the
/// listener runs in can reach nothing - measured: fresh globals per call and
/// a `console` that writes nowhere - so the failures are accumulated on the
/// context object and taken by the next browser call that returns, which
/// logs them under `browser_hint_mask_failure`. This stages exactly what a
/// listener's catch would record, through the host's own snippet door, and
/// watches it arrive in the host's own log; a later call must not repeat it.
///
/// **What it proves, and what it does not**: the channel - accumulate on the
/// context, take, log once - runs end to end here, and the staging refuses
/// to run unless the install really happened (so a stood-down mask fails
/// with that reason, not with a confusing one). The mask's own catch that
/// RECORDS a failure is pinned by the snippet's string assertion instead: a
/// listener's real failure (a target gone the moment it attached) cannot be
/// staged deterministically, and a test that races for it would flake.
#[tokio::test]
#[ignore = "drives the vendored stack; needs `just vendor-browser-stack`"]
async fn the_hint_masks_failures_reach_the_clients_log() {
    let logs = capture_logs_into();
    let stack = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("browser-stack");
    assert!(
        stack.join("node/bin/node").is_file(),
        "the vendored stack is not there - run `just vendor-browser-stack`",
    );
    let dir = tempfile::tempdir().expect("a temp dir");
    let paths = StackPaths {
        stack,
        user_data: dir.path().join("user-data"),
        output: dir.path().join("output"),
        profiles: dir.path().join("profiles"),
    };
    let (origin, _headers) = hints_origin().await;

    let host = BrowserHost::new(paths.clone());
    let active = host.start().await.expect("the browser comes up");
    let browser = Launched::new(active.pid, active.port, paths.user_data.clone());
    host.call(&seat(), "browser_navigate", json!({ "url": origin }))
        .await
        .unwrap_or_else(|why| panic!("the page: {why}"));

    host.call(
        &seat(),
        "browser_run_code_unsafe",
        json!({
            "code": "async (page) => { const store = page.context(); if \
                     (!Array.isArray(store.__forgeHintFailures)) throw new Error('the hint mask \
                     did not install'); store.__forgeHintFailures.push('staged: a page would not \
                     mask'); return 'staged'; }",
        }),
    )
    .await
    .unwrap_or_else(|why| panic!("the staged failure: {why}"));

    // Calls that return carry it out; one more must not repeat it.
    for _ in 0..2 {
        host.call(&seat(), "browser_snapshot", json!({}))
            .await
            .unwrap_or_else(|why| panic!("a carry call: {why}"));
    }
    browser.reap();

    let seen: Vec<String> = logs
        .lock()
        .expect("the log")
        .iter()
        .filter(|line| line.contains("browser_hint_mask_failure"))
        .cloned()
        .collect();
    assert_eq!(
        seen.len(),
        1,
        "the staged failure must reach the client's log exactly once (taken, so not repeated): \
         {seen:?}",
    );
    assert!(
        seen[0].contains("staged: a page would not mask") && seen[0].contains("profile shared"),
        "the log line carries the reason and the profile: {seen:?}",
    );
}
