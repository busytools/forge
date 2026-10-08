//! The browser host against the vendored stack: launch, drive, answer.
//!
//! **Deliberately ignored.** It needs `just vendor-browser-stack` to have
//! run - node and the driver the stack carries, the browser itself always the
//! machine's own - so it cannot be part of a gate every checkout runs. Run it
//! where the stack is:
//!
//! ```text
//! cargo nextest run --manifest-path client/src-tauri/Cargo.toml \
//!     --run-ignored ignored-only -E 'test(a_session_drives_a_page)'
//! ```
//!
//! What it proves is the whole client half of the pipe in one go: the
//! machine's own browser launches against a data directory nobody has used,
//! the vendored driver attaches to it over CDP, and upstream's own
//! `browser_navigate` and `browser_snapshot` answer through the host - which
//! is exactly what an ask from a session rides.
//!
//! **A test whose name ends `_raises_a_window` puts a REAL window on the
//! machine**, because the windowed path is exactly what it proves - so a
//! routine live run excludes them, and they are run deliberately (Ved,
//! 2026-10-08: no test run may pop windows at whoever is at the machine):
//!
//! ```text
//! cargo nextest run --manifest-path client/src-tauri/Cargo.toml \
//!     --run-ignored ignored-only -E 'not test(/_raises_a_window$/)'
//! ```

mod support;

use std::path::PathBuf;
use std::process::Command;

use forge_client::browser::profiles::Seat;
use forge_client::browser::driver::Driver;
use forge_client::browser::{BrowserHost, StackPaths};
use serde_json::json;
use support::Launched;

/// The seat these calls are made for: one session, as an ask carries it.
fn seat() -> Seat {
    Seat {
        org: "Busytools".to_owned(),
        project: "forge".to_owned(),
        label: "browser-live".to_owned(),
    }
}

#[tokio::test]
#[ignore = "drives the vendored stack; needs `just vendor-browser-stack`"]
async fn a_session_drives_a_page() {
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
    let host = BrowserHost::new(paths.clone());

    // **The app's own start**: the browser is up before any call asks for it,
    // and a second start ATTACHES to that launch rather than making another
    // one - one browser for the app, however many times it is ensured.
    let launched = host.start().await.expect("the browser comes up with the app");
    let browser = Launched::new(launched.pid, launched.port, paths.user_data.clone());
    let attached = host.start().await.expect("a second start attaches");
    assert_eq!(
        attached.port, launched.port,
        "the second start attached to the launch that was already there",
    );
    assert_eq!(attached.pid, None, "and it launched nothing of its own");

    let page = "data:text/html,<h1>forge browser host</h1>";
    let parts = host
        .call(&seat(), "browser_navigate", json!({ "url": page }))
        .await
        .unwrap_or_else(|why| panic!("the host could not navigate: {why}"));
    assert!(!parts.is_empty(), "a navigate answers with something");

    let snapshot = host
        .call(&seat(), "browser_snapshot", json!({}))
        .await
        .unwrap_or_else(|why| panic!("the host could not snapshot: {why}"));
    let text = text_of(&snapshot);
    browser.reap();

    assert!(
        text.contains("forge browser host"),
        "the snapshot is of the page that was navigated to: {text}",
    );
}

/// The text one answer carries, which is every part that is not an image.
fn text_of(parts: &[forge_client::browser::driver::ReplyPart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            forge_client::browser::driver::ReplyPart::Text { text } => Some(text.clone()),
            forge_client::browser::driver::ReplyPart::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The ref a snapshot gave an element whose line contains `needle`.
///
/// Both halves matter: the page's own URL line carries whatever the markup
/// says, so a line is a snapshot row only when it names a ref as well.
fn ref_of(snapshot: &str, needle: &str) -> String {
    snapshot
        .lines()
        .find(|line| line.contains("ref=") && line.contains(needle))
        .and_then(|line| {
            let at = line.find("ref=")? + 4;
            let rest = &line[at..];
            let end = rest.find(|c: char| !c.is_ascii_alphanumeric()).unwrap_or(rest.len());
            Some(rest[..end].to_owned())
        })
        .unwrap_or_else(|| {
            panic!("the snapshot names no element containing {needle:?}: {snapshot}")
        })
}

/// **The additions beyond upstream's surface, driven against a real page.**
/// A forced click, an expression wait, a click whose responses come back with
/// it, and a form's state - each a snippet this host composes, each proven
/// here rather than only asserted as a string.
#[tokio::test]
#[ignore = "drives the vendored stack; needs `just vendor-browser-stack`"]
async fn the_additions_drive_a_real_page() {
    // A page that fetches something: a listener of the test's own serving
    // BOTH halves, so the click's request is same-origin and its body
    // readable - a data: URL's fetch is cross-origin and dies at the CORS
    // layer, where no response is what comes back.
    const PAGE: &str = "<title>before</title>\
         <form><label>Email <input name=email required value=''></label>\
         <div aria-invalid=true><input name=picker value='committed'></div></form>\
         <button onclick=\"fetch('/x').then(() => {{document.title='fetched'}})\">Fetch</button>";
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let port = listener.local_addr().expect("the bound port").port();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        for _ in 0..6 {
            let Ok((mut socket, _)) = listener.accept().await else { continue };
            let mut buffer = [0_u8; 4096];
            let read = socket.read(&mut buffer).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..read]);
            let body = if request.starts_with("GET /x") { "fetched!\n" } else { PAGE };
            let answer = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len(),
            );
            let _ = socket.write_all(answer.as_bytes()).await;
        }
    });

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
    let host = BrowserHost::new(paths.clone());
    let active = host.start().await.expect("the browser comes up");
    let browser = Launched::new(active.pid, active.port, paths.user_data.clone());

    let page = format!("http://127.0.0.1:{port}/");
    host.call(&seat(), "browser_navigate", json!({ "url": page }))
        .await
        .unwrap_or_else(|why| panic!("the page would not navigate: {why}"));
    let snapshot = host
        .call(&seat(), "browser_snapshot", json!({}))
        .await
        .map(|parts| text_of(&parts))
        .unwrap_or_else(|why| panic!("the page would not snapshot: {why}"));
    let button = ref_of(&snapshot, "Fetch");

    // A forced click: Playwright's own click with the actionability gate off.
    let clicked = host
        .call(&seat(), "browser_click", json!({ "target": button, "force": true }))
        .await
        .unwrap_or_else(|why| panic!("a forced click did not run: {why}"));
    assert!(text_of(&clicked).contains("clicked"), "{:?}", text_of(&clicked));

    // The click's responses come back with the click itself.
    let captured = host
        .call(&seat(), "browser_click_and_capture", json!({ "target": button, "settleMs": 1200 }))
        .await
        .map(|parts| text_of(&parts))
        .unwrap_or_else(|why| panic!("click_and_capture did not run: {why}"));
    // The driver's own report carries the snippet's JSON as an escaped
    // string, so the pieces are asserted, not a quoted shape.
    assert!(
        captured.contains("/x") && captured.contains("200") && captured.contains("fetched!"),
        "the click's response - url, status and body - is what came back: {captured}",
    );

    // An expression wait: a condition no text can state.
    let waited = host
        .call(&seat(), "browser_wait_for", json!({ "expression": "document.title === 'fetched'" }))
        .await
        .unwrap_or_else(|why| panic!("the expression wait did not run: {why}"));
    assert!(text_of(&waited).contains("condition holds"), "{:?}", text_of(&waited));

    // The form's state: the values, what is marked invalid, and the control's
    // own text.
    let form = host
        .call(&seat(), "browser_form_state", json!({}))
        .await
        .map(|parts| text_of(&parts))
        .unwrap_or_else(|why| panic!("form_state did not run: {why}"));
    browser.reap();
    // Escaped by the driver's own report, so the pieces are asserted.
    assert!(form.contains("email"), "the empty required input is read: {form}");
    assert!(form.contains("invalid") && form.contains("true"), "and it is marked invalid: {form}");
    assert!(
        form.contains("picker") && form.contains("committed"),
        "and a field's committed value is what comes back, wherever the control keeps it: {form}",
    );
}

/// **Parity is checked against the driver itself, not against the capture.**
/// Spec section 9 asks that the surface equal what the pinned
/// `@playwright/mcp` publishes; the 25 upstream names below are the probe
/// capture's list, and the server side pins OUR surface to the same names -
/// so this test closes the loop by comparing the list to the LIVE
/// `tools/list` of the vendored driver, where a renamed or dropped tool
/// cannot survive.
#[tokio::test]
#[ignore = "drives the vendored stack; needs `just vendor-browser-stack`"]
async fn the_surface_matches_the_drivers_own_tools_list() {
    // The probe capture's 25, in its order - the same list
    // `forge_workspace::mcp::browser::specs` pins ours to.
    const UPSTREAM: [&str; 25] = [
        "browser_close",
        "browser_resize",
        "browser_console_messages",
        "browser_handle_dialog",
        "browser_emulate_media",
        "browser_evaluate",
        "browser_file_upload",
        "browser_drop",
        "browser_find",
        "browser_fill_form",
        "browser_press_key",
        "browser_type",
        "browser_navigate",
        "browser_navigate_back",
        "browser_network_requests",
        "browser_network_request",
        "browser_run_code_unsafe",
        "browser_take_screenshot",
        "browser_snapshot",
        "browser_click",
        "browser_drag",
        "browser_hover",
        "browser_select_option",
        "browser_tabs",
        "browser_wait_for",
    ];

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
    let host = BrowserHost::new(paths.clone());
    let active = host.start().await.expect("the browser comes up");
    let browser = Launched::new(active.pid, active.port, paths.user_data.clone());
    let endpoint = format!("http://127.0.0.1:{}", active.port);
    let driver = match Driver::start(
        &forge_client::browser::driver::node_path(&paths.stack),
        &forge_client::browser::driver::cli_path(&paths.stack),
        &endpoint,
        &paths.output,
    )
    .await
    {
        Ok(driver) => driver,
        Err(why) => {
            browser.reap();
            panic!("the driver did not start: {why}");
        }
    };

    let published = match driver.tool_names().await {
        Ok(names) => names,
        Err(why) => {
            browser.reap();
            panic!("the driver did not list its tools: {why}");
        }
    };
    browser.reap();
    assert_eq!(
        published,
        UPSTREAM.map(str::to_owned).to_vec(),
        "the driver's own tools/list is the surface this family registers",
    );
}

/// **The browser dying under a live driver does not wedge its profile.**
///
/// Round 1's Critical, falsified live: the driver is a node child tied to the
/// browser only by CDP, so killing the browser closes the socket while the
/// driver's MCP transport stays OPEN - `is_running()` keeps saying yes, and
/// every later call re-runs `connectOverCDP` against the endpoint it was
/// spawned with, which is a CLI argument naming a port nobody listens on any
/// more. The profile has to notice the browser moved: this kills the browser
/// by its own pid file and asserts the next call still answers, on a
/// relaunched browser.
#[tokio::test]
#[ignore = "drives the vendored stack; needs `just vendor-browser-stack`"]
async fn a_killed_browser_does_not_wedge_its_profile() {
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
    let host = BrowserHost::new(paths.clone());
    let active = host.start().await.expect("the browser comes up");
    let browser = Launched::new(active.pid, active.port, paths.user_data.clone());

    let page = "data:text/html,<h1>alive</h1>";
    host.call(&seat(), "browser_navigate", json!({ "url": page }))
        .await
        .unwrap_or_else(|why| panic!("the first navigate should run: {why}"));

    // The browser dies under the driver: the pid its own launch wrote down.
    let pid = std::fs::read_to_string(paths.user_data.join("browser.pid"))
        .expect("the launch wrote its pid down")
        .trim()
        .to_owned();
    let killed = Command::new("kill").arg(&pid).status().expect("kill runs");
    assert!(killed.success(), "the browser is killed by its own pid");
    // Bounded wait for the port to stop answering, so the kill landed.
    for _ in 0..40 {
        if forge_client::browser::chromium::probe(active.port).await {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        } else {
            break;
        }
    }
    assert!(
        !forge_client::browser::chromium::probe(active.port).await,
        "the killed browser's port stopped answering",
    );

    // The next call must recover: the profile notices the browser moved,
    // drops its driver, and the call relaunches both.
    let parts = host
        .call(&seat(), "browser_navigate", json!({ "url": page }))
        .await
        .unwrap_or_else(|why| panic!("a call after the browser was killed must recover: {why}"));
    browser.reap();
    assert!(!parts.is_empty(), "the recovery navigate answers with something");
}

/// **A headed relaunch never adopts a headless launch** (round-3 A1). The
/// adopt re-check under the flock answers Ok for any live launch, so a headed
/// relaunch that waited behind another host's headless launch used to answer
/// Ok with NO window and no marker written. A live launch that is not
/// windowed must take the close-first path instead: the port it answered on
/// goes, and the relaunch leaves a real window behind.
#[tokio::test]
#[ignore = "raises a REAL window; run it deliberately - see the module doc"]
async fn a_headed_relaunch_does_not_adopt_a_headless_launch_raises_a_window() {
    let binary = forge_client::browser::chromium::browser_binary()
        .unwrap_or_else(|why| panic!("no browser on this machine: {why}"));
    let dir = tempfile::tempdir().expect("a temp dir");
    let profile = dir.path().join("user-data");
    std::fs::create_dir_all(&profile).expect("the profile");

    // A live HEADLESS launch, as `start()` leaves it.
    let headless = forge_client::browser::chromium::launch(&binary, &profile)
        .await
        .unwrap_or_else(|why| panic!("the headless launch would not come up: {why}"));
    let guard = Launched::new(headless.pid, headless.port, profile.clone());
    assert!(
        !forge_client::browser::chromium::launched_windowed(&profile),
        "a headless launch leaves no windowed marker",
    );

    // A headed relaunch asked for while it runs: it must NOT be adopted - the
    // headless launch is closed first, and a real window comes up on a new
    // port with the marker written.
    let headed = forge_client::browser::chromium::launch_with(&binary, &profile, true, None)
        .await
        .unwrap_or_else(|why| panic!("the headed relaunch would not come up: {why}"));
    assert!(
        forge_client::browser::chromium::launched_windowed(&profile),
        "the headed relaunch writes the windowed marker - an adopted headless launch does not",
    );
    assert_ne!(
        headed.port, headless.port,
        "the headed relaunch is its own launch, not the adopted headless one",
    );
    guard.reap();
}

/// **One browser per profile across hosts, under the guard.** Two clients
/// launching at the same moment produce ONE browser: the first to take the
/// launch lock brings it up, the second waits the lock out and attaches. This
/// is what pins the adopt re-check AND the guard's whole span - without the
/// re-check the second start deletes the live launch's port file and spawns
/// its own; with the guard released early the second start can do the same
/// mid-launch - and the one-port assertion below is what catches both.
#[tokio::test]
#[ignore = "drives the real browser; run with --run-ignored ignored-only"]
async fn two_hosts_launching_at_once_produce_one_browser() {
    let stack = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("browser-stack");
    let dir = tempfile::tempdir().expect("a temp dir");
    let paths = StackPaths {
        stack,
        user_data: dir.path().join("user-data"),
        output: dir.path().join("output"),
        profiles: dir.path().join("profiles"),
    };
    let a = BrowserHost::new(paths.clone());
    let b = BrowserHost::new(paths.clone());

    let (first, second) = tokio::join!(a.start(), b.start());
    let first = first.unwrap_or_else(|why| panic!("the first start must come up: {why}"));
    let second =
        second.unwrap_or_else(|why| panic!("the second start must attach, not fail: {why}"));
    let guard = Launched::new(first.pid.or(second.pid), first.port, paths.user_data.clone());

    assert_eq!(
        first.port, second.port,
        "both starts answer on the one browser the guard made: {first:?} vs {second:?}",
    );
    let pids = [first.pid, second.pid];
    assert_eq!(
        pids.iter().filter(|pid| pid.is_some()).count(),
        1,
        "exactly one start launched the browser: {pids:?}",
    );
    assert_eq!(
        pids.iter().filter(|pid| pid.is_none()).count(),
        1,
        "and the other attached to the launch that was already there: {pids:?}",
    );
    guard.reap();
}

