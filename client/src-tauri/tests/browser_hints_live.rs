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
