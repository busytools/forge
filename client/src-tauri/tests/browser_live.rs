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

use forge_client::browser::driver::Driver;
use forge_client::browser::profiles::Seat;
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

    // **A second navigation re-letters the refs**: the same element's ref
    // reads `f1e*` now, and a forced click driven from that spelling has to
    // reach the same engine as the bare one.
    host.call(&seat(), "browser_navigate", json!({ "url": page }))
        .await
        .unwrap_or_else(|why| panic!("the page would not navigate a second time: {why}"));
    let relettered = host
        .call(&seat(), "browser_snapshot", json!({}))
        .await
        .map(|parts| text_of(&parts))
        .unwrap_or_else(|why| panic!("the page would not snapshot again: {why}"));
    let relettered_button = ref_of(&relettered, "Fetch");
    assert!(
        relettered_button.starts_with('f'),
        "the second navigation re-letters the refs: {relettered_button}",
    );
    let clicked = host
        .call(&seat(), "browser_click", json!({ "target": relettered_button, "force": true }))
        .await
        .unwrap_or_else(|why| panic!("a forced click on a relettered ref did not run: {why}"));
    assert!(text_of(&clicked).contains("clicked"), "{:?}", text_of(&clicked));
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

/// **A raise over a launch from before the window model closes it, once, and
/// brings the new shape up.** An upgraded client can adopt a browser an older
/// build left running - a headless launch with no window at all - and a raise
/// at it must not fail: the launch is closed and relaunched on the page the
/// last raise carried, so the next raise finds a window.
#[tokio::test]
#[ignore = "raises a REAL window; run it deliberately - see the module doc"]
async fn a_raise_over_an_older_headless_launch_brings_up_the_new_shape() {
    let binary = forge_client::browser::chromium::browser_binary()
        .unwrap_or_else(|why| panic!("no browser on this machine: {why}"));
    let dir = tempfile::tempdir().expect("a temp dir");
    let profile = dir.path().join("user-data");
    std::fs::create_dir_all(&profile).expect("the profile");

    // The old shape, exactly: headless, no window, and none of this build's
    // born marker.
    let mut command = Command::new(&binary);
    command
        .arg("--remote-debugging-port=0")
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg("--headless")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("about:blank");
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::null());
    command.stderr(std::process::Stdio::null());
    let mut child = command.spawn().expect("the old-shape launch spawns");
    let _guard = Launched::new(Some(child.id()), 0, profile.clone());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let old = loop {
        if let Some(active) = forge_client::browser::chromium::read_active_port(&profile)
            && forge_client::browser::chromium::probe(active.port).await
        {
            break active;
        }
        assert!(std::time::Instant::now() < deadline, "the old-shape browser did not answer");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };
    let guard = Launched::new(Some(child.id()), old.port, profile.clone());
    assert!(!profile.join("has-window").exists(), "the old shape carries no born marker",);

    // The raise: the old launch is closed and the new shape comes up, up.
    let raised = forge_client::browser::chromium::show(&binary, &profile)
        .await
        .unwrap_or_else(|why| panic!("the raise must recover from the old shape: {why}"));
    assert_ne!(raised.port, old.port, "the old headless launch was closed, not raised at");
    assert!(
        profile.join("has-window").exists(),
        "the new launch carries its window's birth marker",
    );
    assert!(
        forge_client::browser::chromium::windowed(&profile).await,
        "and the person's window is up",
    );
    // The old shape is gone (the show closed it): reap its handle.
    let _ = child.wait();
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
    // **The guard BEFORE the unwraps**: a start that fails while the other
    // launched would panic with the browser still running and nothing to
    // reap it. The profile re-read inside the guard is what catches it when
    // neither pid is known.
    let guard = Launched::new(
        first
            .as_ref()
            .ok()
            .and_then(|active| active.pid)
            .or(second.as_ref().ok().and_then(|active| active.pid)),
        first.as_ref().ok().map_or(0, |active| active.port),
        paths.user_data.clone(),
    );
    let first = first.unwrap_or_else(|why| panic!("the first start must come up: {why}"));
    let second =
        second.unwrap_or_else(|why| panic!("the second start must attach, not fail: {why}"));

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

/// **The launch shows nothing, and the raise is the only thing that
/// appears** - the whole window model in one live run. A launch comes up
/// headed (the person's own browser, no `--headless`), holds its window off
/// every screen, and is not "up" to anyone; a show brings that same window
/// into view; an answer takes it back out while the process and its page
/// stay; a second show raises the same launch again.
#[tokio::test]
#[ignore = "raises a REAL window; run it deliberately - see the module doc"]
async fn a_launch_shows_nothing_and_a_raise_is_the_only_thing_that_appears() {
    use forge_client::browser::window as win;

    let binary = forge_client::browser::chromium::browser_binary()
        .unwrap_or_else(|why| panic!("no browser on this machine: {why}"));
    let dir = tempfile::tempdir().expect("a temp dir");
    let profile = dir.path().join("profile");
    std::fs::create_dir_all(&profile).expect("the profile");

    // The launch, as a session's first call leaves it: headed, hidden.
    let launched = forge_client::browser::chromium::launch(&binary, &profile, None)
        .await
        .unwrap_or_else(|why| panic!("the launch would not come up: {why}"));
    let guard = Launched::new(launched.pid, launched.port, profile.clone());
    assert!(
        forge_client::browser::chromium::probe(launched.port).await,
        "a browser answers after the launch",
    );
    assert!(
        !forge_client::browser::chromium::windowed(&profile).await,
        "and no window of its is up: the launch is invisible to the person",
    );

    // The launch's own fact, read off the process: headed means no --headless,
    // and nothing at startup means --no-startup-window.
    if let Some(pid) = launched.pid {
        let listed = Command::new("ps").args(["-p", &pid.to_string(), "-o", "command="]).output();
        if let Ok(listed) = listed {
            let line = String::from_utf8_lossy(&listed.stdout).into_owned();
            eprintln!("the launched browser's command line: {line}");
            assert!(!line.contains("--headless"), "the launch is the person's own browser: {line}");
            assert!(line.contains("--no-startup-window"), "and it starts with no window: {line}");
        }
    }

    // The window exists, born out of the person's view: its own bounds read
    // clear of any usual screen position (the server clamps the ask to a
    // corner; nothing of it is where a window a person opened would sit).
    let mut cdp = win::Cdp::connect(launched.port, &launched.path)
        .await
        .unwrap_or_else(|why| panic!("the devtools endpoint refused: {why}"));
    let page = cdp.page().await.unwrap_or_else(|why| panic!("no page: {why}"));
    let window = win::window(&page, &mut cdp).await.expect("the window answers");
    eprintln!("the hidden window's bounds: {}", window.bounds);
    assert!(
        window.number("left").unwrap_or(0) >= 1000 && window.number("top").unwrap_or(0) >= 600,
        "the window rests clear of the person's view: {}",
        window.bounds,
    );

    // The person's show: the same window, into view.
    let shown = forge_client::browser::chromium::show(&binary, &profile)
        .await
        .unwrap_or_else(|why| panic!("the show failed: {why}"));
    assert_eq!(shown.port, launched.port, "the show raised the launch that was there");
    assert!(forge_client::browser::chromium::windowed(&profile).await, "and the window is up now",);
    let window = win::window(&page, &mut cdp).await.expect("the window answers");
    eprintln!("the raised window's bounds: {}", window.bounds);
    assert_eq!(
        (window.number("left"), window.number("top")),
        (Some(40), Some(60)),
        "placed where the raise puts it: {}",
        window.bounds,
    );

    // The answer: back out of sight, the process and its page staying.
    forge_client::browser::chromium::hide(&profile).await;
    assert!(
        forge_client::browser::chromium::probe(launched.port).await,
        "the answer's hide leaves the browser running - that is the #2107 fix",
    );
    assert!(
        !forge_client::browser::chromium::windowed(&profile).await,
        "and the window is not up any more",
    );
    let window = win::window(&page, &mut cdp).await.expect("the window answers");
    eprintln!("the lowered window's bounds: {}", window.bounds);
    assert!(
        window.number("left").unwrap_or(0) >= 1000,
        "the window went back out of the person's view: {}",
        window.bounds,
    );

    // A second show raises the SAME launch - nothing relaunched.
    let again = forge_client::browser::chromium::show(&binary, &profile)
        .await
        .unwrap_or_else(|why| panic!("the second show failed: {why}"));
    assert_eq!(again.port, launched.port, "the same browser, its same port");
    assert!(forge_client::browser::chromium::windowed(&profile).await, "up again");
    guard.reap();
}

/// **The property this whole cut exists for**: the page a session prepared
/// is the page the person acts on. A session drives a value into the hidden
/// window, the person's raise brings the same window forward, and the value
/// is still there - no relaunch, no reload, nothing restored. Measured
/// through the vendored driver, the way a session's calls arrive.
#[tokio::test]
#[ignore = "drives the vendored stack; needs `just vendor-browser-stack`"]
async fn a_raise_keeps_the_page_a_session_prepared() {
    let stack = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("browser-stack");
    assert!(
        stack.join("node/bin/node").is_file(),
        "the vendored stack is not there - run `just vendor-browser-stack`",
    );
    let binary = forge_client::browser::chromium::browser_binary()
        .unwrap_or_else(|why| panic!("no browser on this machine: {why}"));
    let dir = tempfile::tempdir().expect("a temp dir");
    let profile = dir.path().join("profile");
    std::fs::create_dir_all(&profile).expect("the profile");
    let output = dir.path().join("output");

    let launched = forge_client::browser::chromium::launch(&binary, &profile, None)
        .await
        .unwrap_or_else(|why| panic!("the launch would not come up: {why}"));
    let guard = Launched::new(launched.pid, launched.port, profile.clone());
    let endpoint = format!("http://127.0.0.1:{}", launched.port);
    let driver = Driver::start(
        &forge_client::browser::driver::node_path(&stack),
        &forge_client::browser::driver::cli_path(&stack),
        &endpoint,
        &output,
    )
    .await
    .unwrap_or_else(|why| panic!("the driver did not start: {why}"));

    // The session's work, in the hidden window.
    let navigated = driver
        .call(
            "browser_navigate",
            json!({ "url": "data:text/html,<title>prepared</title><form><label>Email <input name=email></label></form>" }),
        )
        .await;
    assert!(navigated.is_ok(), "the navigate must land: {navigated:?}");
    let filled = driver
        .call("browser_type", json!({ "target": "input[name=email]", "text": "prepared@value" }))
        .await;
    assert!(filled.is_ok(), "the fill must land: {filled:?}");

    // The person's raise, then the read-back on the same page.
    forge_client::browser::chromium::show(&binary, &profile)
        .await
        .unwrap_or_else(|why| panic!("the show failed: {why}"));
    let value = driver
        .call(
            "browser_evaluate",
            json!({ "function": "() => document.querySelector('input[name=email]').value" }),
        )
        .await
        .unwrap_or_else(|why| panic!("the read-back failed: {why}"));
    let read = format!("{value:?}");
    assert!(
        read.contains("prepared@value"),
        "the fill the session made is the page the person is looking at: {read}",
    );

    // And through a lower and a second raise, the same page still holds it.
    forge_client::browser::chromium::hide(&profile).await;
    forge_client::browser::chromium::show(&binary, &profile)
        .await
        .unwrap_or_else(|why| panic!("the second show failed: {why}"));
    let value = driver
        .call(
            "browser_evaluate",
            json!({ "function": "() => document.querySelector('input[name=email]').value" }),
        )
        .await
        .unwrap_or_else(|why| panic!("the second read-back failed: {why}"));
    let read = format!("{value:?}");
    assert!(
        read.contains("prepared@value"),
        "a lower and a raise later, the page is still the prepared one: {read}",
    );
    guard.reap();
}

/// Where each candidate hide position actually lands. The browser reports
/// the bounds it took, and the test prints the part of the window that still
/// sits inside the 2048x1152 screen - the answer to "is this really out of
/// sight", which is geometry and not an assumption.
#[tokio::test]
#[ignore = "raises a REAL window; run it deliberately - see the module doc"]
async fn where_the_move_actually_lands() {
    use forge_client::browser::window as win;

    let binary = forge_client::browser::chromium::browser_binary()
        .unwrap_or_else(|why| panic!("no browser on this machine: {why}"));
    let dir = tempfile::tempdir().expect("a temp dir");
    let profile = dir.path().join("profile");
    std::fs::create_dir_all(&profile).expect("the profile");
    let launched = forge_client::browser::chromium::launch(&binary, &profile, None)
        .await
        .unwrap_or_else(|why| panic!("the launch would not come up: {why}"));
    let guard = Launched::new(launched.pid, launched.port, profile.clone());
    let mut cdp = win::Cdp::connect(launched.port, &launched.path)
        .await
        .unwrap_or_else(|why| panic!("the devtools endpoint refused: {why}"));
    let page = cdp.page().await.unwrap_or_else(|why| panic!("no page: {why}"));

    // The screen the window server reports: one display, its own frame.
    let screen = (0_i64, 0_i64, 2048_i64, 1152_i64);
    let at = win::window(&page, &mut cdp).await.expect("the window answers");
    let (area, whole) = visible(&at, screen);
    eprintln!("at launch: {:?}, on-screen area {area} of {whole} sq pt", at.bounds);
    // **The birth move is clamped too, and the clamp is the hide's residual**:
    // the window is mostly off every screen, and some of it stays in the
    // frame. An engine that ever let it go entirely would make the design's
    // residual disappear - and this is where that would be noticed.
    assert!(
        area > 0 && area < whole / 10,
        "the born-hidden window sits mostly off the screen, with a sliver kept in the frame \
         (measured 40x40 in a corner): {}",
        at.bounds,
    );

    for (left, top) in [
        (-10000, -10000),
        (-10000, -10000),
        (-20000, -20000),
        (-10000, 10000),
        (10000, 10000),
        (-10000, -40),
        (10, -10000),
    ] {
        win::move_to(&page, &mut cdp, left, top).await.expect("the move");
        std::thread::sleep(std::time::Duration::from_millis(400));
        let landed = win::window(&page, &mut cdp).await.expect("the window answers");
        let (area, whole) = visible(&landed, screen);
        eprintln!(
            "asked ({left}, {top}): {} bounds, on-screen area {area} of {whole} sq pt",
            landed.bounds,
        );
        // **The clamp, asserted**: a far ask still leaves part of the window
        // in the frame and most of it out - what makes the hide a sliver
        // rather than nothing, and the position answerable rather than
        // assumed. `(10, -10000)` is the one ask the clamp refuses outright,
        // leaving the window fully in view.
        if (left, top) == (10, -10000) {
            assert!(
                whole > 0 && area == whole,
                "a sideways-and-up ask keeps the whole window in view: {}",
                landed.bounds,
            );
        } else {
            assert!(
                area > 0 && area < whole / 10,
                "a far ask leaves a sliver in the frame, not nothing and not the window: {}",
                landed.bounds,
            );
        }
    }

    guard.reap();
}

/// The window's bounds and the part of them inside the screen, in points:
/// the overlap area, and the window's own whole area.
fn visible(
    window: &forge_client::browser::window::Window,
    screen: (i64, i64, i64, i64),
) -> (i64, i64) {
    let (l, t) = (window.number("left"), window.number("top"));
    let (w, h) = (window.number("width"), window.number("height"));
    let (Some(l), Some(t), Some(w), Some(h)) = (l, t, w, h) else {
        return (0, 0);
    };
    let (sx, sy, sw, sh) = screen;
    let overlap_w = (l + w).min(sx + sw) - l.max(sx);
    let overlap_h = (t + h).min(sy + sh) - t.max(sy);
    (overlap_w.max(0) * overlap_h.max(0), w * h)
}

/// **The decisive question for a minimized hide**: can the vendored driver
/// still drive the page while the window is minimized? Frames stop when it
/// is, and the click path waits on animation frames - if the real click dies,
/// minimize cannot be the hide, whatever it does for the eye.
#[tokio::test]
#[ignore = "drives the vendored stack; needs `just vendor-browser-stack`"]
async fn a_minimized_window_can_still_be_driven() {
    use forge_client::browser::window as win;

    let stack = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("browser-stack");
    assert!(
        stack.join("node/bin/node").is_file(),
        "the vendored stack is not there - run `just vendor-browser-stack`",
    );
    let binary = forge_client::browser::chromium::browser_binary()
        .unwrap_or_else(|why| panic!("no browser on this machine: {why}"));
    let dir = tempfile::tempdir().expect("a temp dir");
    let profile = dir.path().join("profile");
    std::fs::create_dir_all(&profile).expect("the profile");
    let output = dir.path().join("output");

    let launched = forge_client::browser::chromium::launch(&binary, &profile, None)
        .await
        .unwrap_or_else(|why| panic!("the launch would not come up: {why}"));
    let guard = Launched::new(launched.pid, launched.port, profile.clone());
    let endpoint = format!("http://127.0.0.1:{}", launched.port);
    let driver = Driver::start(
        &forge_client::browser::driver::node_path(&stack),
        &forge_client::browser::driver::cli_path(&stack),
        &endpoint,
        &output,
    )
    .await
    .unwrap_or_else(|why| panic!("the driver did not start: {why}"));

    // A page with a button to click and a form to fill.
    let navigated = driver
        .call(
            "browser_navigate",
            json!({ "url": "data:text/html,<title>minimized</title><button onclick=\"document.title='clicked'\">Press</button><form><label>Email <input name=email></label></form>" }),
        )
        .await;
    eprintln!("navigate while visible: {navigated:?}");
    assert!(navigated.is_ok(), "the navigate must land first");

    // Minimize the window, then drive it.
    let mut cdp = win::Cdp::connect(launched.port, &launched.path)
        .await
        .unwrap_or_else(|why| panic!("the devtools endpoint refused: {why}"));
    let page = cdp.page().await.unwrap_or_else(|why| panic!("no page: {why}"));
    win::minimize(&page, &mut cdp).await.expect("the minimize");
    std::thread::sleep(std::time::Duration::from_millis(900));
    let state = awake(&mut cdp, &page).await;
    eprintln!("minimized: {state}");
    assert!(
        state.contains("hidden") && state.contains("frames stopped"),
        "a minimized window's page reads hidden and its frames stop - the fact the hide rests \
         on: {state}",
    );

    let snapshot = driver.call("browser_snapshot", json!({})).await;
    assert!(snapshot.is_ok(), "a snapshot still answers while minimized: {snapshot:?}");
    let filled = driver
        .call("browser_type", json!({ "target": "input[name=email]", "text": "a@b.c" }))
        .await;
    assert!(filled.is_ok(), "a fill still lands while minimized: {filled:?}");

    // **The click must die, and die the way the design says** - the
    // stability wait on animation frames, against its own timeout - because
    // that death is the whole reason minimize is not the hide.
    let clicked = driver.call("browser_click", json!({ "target": "button" })).await;
    eprintln!("click while minimized: {clicked:?}");
    let Err(why) = &clicked else {
        panic!("a click into a minimized window must not answer Ok: {clicked:?}");
    };
    assert!(
        why.contains("Timeout") && why.contains("stable"),
        "the click dies in the stability wait on frames, by its own bound: {why}",
    );

    // The forced click skips the actionability gate - and still dies, which
    // is why nothing can route around the freeze.
    let forced = driver.call("browser_click", json!({ "target": "button", "force": true })).await;
    eprintln!("forced click while minimized: {forced:?}");
    let Err(why) = &forced else {
        panic!("even a forced click must die behind the freeze: {forced:?}");
    };
    assert!(
        why.contains("Timeout") && why.contains("stable"),
        "and die in the same stability wait - the freeze is not routed around: {why}",
    );

    // The other hide, with the page awake: the SAME click works, and lands.
    win::bring_forward(&page, &mut cdp, 40, 60).await.expect("back to normal");
    win::move_to(&page, &mut cdp, -10000, -10000).await.expect("the move");
    std::thread::sleep(std::time::Duration::from_millis(700));
    let state = awake(&mut cdp, &page).await;
    eprintln!("off-screen: {state}");
    assert!(
        state.contains("visible") && state.contains("frames live"),
        "an off-screen NORMAL window's page stays a visible page: {state}",
    );
    let clicked = driver.call("browser_click", json!({ "target": "button" })).await;
    assert!(clicked.is_ok(), "the click lands while off-screen: {clicked:?}");
    let title = driver
        .call("browser_evaluate", json!({ "function": "() => document.title" }))
        .await
        .unwrap_or_else(|why| panic!("the read-back failed: {why}"));
    assert!(
        format!("{title:?}").contains("clicked"),
        "and it really fired - the page's own title says so: {title:?}",
    );

    guard.reap();
}

/// **Can the driver work in the profile's hidden window?** The whole
/// pre-raise life of a profile's browser sits here: the launch is the
/// production one, its window was born off every screen, and nothing is
/// raised - if the driver's click lands there, the design's "a session drives
/// the browser the person has not seen" is real.
#[tokio::test]
#[ignore = "drives the vendored stack; needs `just vendor-browser-stack`"]
async fn the_driver_works_in_a_window_that_was_never_shown() {
    use forge_client::browser::window as win;

    let stack = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("browser-stack");
    assert!(
        stack.join("node/bin/node").is_file(),
        "the vendored stack is not there - run `just vendor-browser-stack`",
    );
    let binary = forge_client::browser::chromium::browser_binary()
        .unwrap_or_else(|why| panic!("no browser on this machine: {why}"));
    let dir = tempfile::tempdir().expect("a temp dir");
    let profile = dir.path().join("profile");
    std::fs::create_dir_all(&profile).expect("the profile");
    let output = dir.path().join("output");

    let launched = forge_client::browser::chromium::launch(&binary, &profile, None)
        .await
        .unwrap_or_else(|why| panic!("the launch would not come up: {why}"));
    let guard = Launched::new(launched.pid, launched.port, profile.clone());
    let endpoint = format!("http://127.0.0.1:{}", launched.port);
    let driver = Driver::start(
        &forge_client::browser::driver::node_path(&stack),
        &forge_client::browser::driver::cli_path(&stack),
        &endpoint,
        &output,
    )
    .await
    .unwrap_or_else(|why| panic!("the driver did not start: {why}"));

    // The driver's work, against the hidden window the launch made.
    let navigated = driver
        .call(
            "browser_navigate",
            json!({ "url": "data:text/html,<title>background</title><button onclick=\"document.title='clicked'\">Press</button><form><label>Email <input name=email></label></form>" }),
        )
        .await;
    assert!(navigated.is_ok(), "the navigate must land: {navigated:?}");
    let clicked = driver.call("browser_click", json!({ "target": "button" })).await;
    assert!(clicked.is_ok(), "the click must land in a hidden window: {clicked:?}");
    let filled = driver
        .call("browser_type", json!({ "target": "input[name=email]", "text": "a@b.c" }))
        .await;
    assert!(filled.is_ok(), "the fill must land: {filled:?}");

    // **Nothing raised itself.** A session's calls work in a window the
    // person has not seen, and none of them bring it into view: the strip
    // still reads its launch as hidden, and the window server still finds it
    // off the screens.
    assert!(
        !forge_client::browser::chromium::windowed(&profile).await,
        "a session's own work must never raise the window",
    );
    let mut cdp = win::Cdp::connect(launched.port, &launched.path)
        .await
        .unwrap_or_else(|why| panic!("the devtools endpoint refused: {why}"));
    let page = cdp.page().await.unwrap_or_else(|why| panic!("no page: {why}"));
    let window = win::window(&page, &mut cdp).await.expect("the window answers");
    eprintln!("the hidden window after the driver's work: {}", window.bounds);
    assert!(
        window.number("left").unwrap_or(0) >= 1000,
        "the window stayed clear of the person's view: {}",
        window.bounds,
    );

    guard.reap();
}

/// What the page reports about itself: its visibility state, and whether
/// animation frames are still running. Three frames or a two-second verdict.
async fn awake(
    cdp: &mut forge_client::browser::window::Cdp,
    page: &forge_client::browser::window::Page,
) -> String {
    let expression = "(async () => { \
         const frames = await new Promise((resolve) => { \
             let n = 0; \
             const stop = setTimeout(() => resolve('frames stopped'), 2000); \
             const step = () => { \
                 n += 1; \
                 if (n >= 3) { clearTimeout(stop); resolve('frames live'); } \
                 else { requestAnimationFrame(step); } \
             }; \
             requestAnimationFrame(step); \
         }); \
         return document.visibilityState + ', ' + frames; \
     })()";
    let answer = cdp
        .call(
            "Runtime.evaluate",
            serde_json::json!({ "expression": expression, "awaitPromise": true, "returnByValue": true }),
            Some(&page.session),
        )
        .await
        .expect("the page evaluates");
    answer
        .get("result")
        .and_then(|result| result.get("value"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("no answer")
        .to_owned()
}
