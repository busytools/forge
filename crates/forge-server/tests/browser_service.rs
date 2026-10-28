//! The server's browser fleet: a profile's browser is driven from the
//! server, its frames arrive, and exactly one viewer holds the control role.
//!
//! **A live browser, not a mock.** The test spawns the installed host bundle
//! and the vendored Playwright driver, so it needs both in place:
//! `scripts/stage-browser-host.sh` and `scripts/vendor_browser_stack.sh`
//! (which installs to `FORGE_BROWSER_STACK_DIR`); the test reads that
//! directory from **`FORGE_BROWSER_TEST_STACK`**.

#![cfg(target_os = "macos")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::PathBuf;
use std::time::Duration;

use forge_browser::service::BrowserService;
use forge_primitives::SessionSlot;
use forge_primitives::browser::{BrowserInputEvent, InputModifiers, MouseAction};
use serde_json::json;

/// The vendored driver stack: an override for a checkout under test, else the
/// installed one beside the server's profiles.
fn stack() -> PathBuf {
    match std::env::var("FORGE_BROWSER_TEST_STACK") {
        Ok(stack) => PathBuf::from(stack),
        Err(_) => forge_browser::host_bundle::app_support()
            .expect("the app-support directory")
            .join("browser/stack"),
    }
}

/// End a host this test started itself, however the test ends.
struct HostGuard {
    pid: u32,
    ring: String,
}

impl Drop for HostGuard {
    fn drop(&mut self) {
        let _ = std::process::Command::new("kill").args(["-TERM", &self.pid.to_string()]).status();
        forge_browser::osr::Ring::unlink(&self.ring);
    }
}

/// The next frame for `tab`, waiting past `after`'s sequence number.
async fn next_frame(
    frames: &mut tokio::sync::watch::Receiver<Option<forge_primitives::browser::BrowserFrame>>,
    tab: u16,
    after: Option<u32>,
) -> forge_primitives::browser::BrowserFrame {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(frame) = frames.borrow_and_update().clone()
            && frame.tab == tab
            && after.is_none_or(|seq| frame.seq > seq)
        {
            return frame;
        }
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        tokio::time::timeout(left, frames.changed())
            .await
            .expect("a frame arrives")
            .expect("the frame channel is open");
    }
}

/// Serve `page` on a loopback port for the length of the test.
fn serve_page(page: &'static str) -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("a loopback port");
    let port = listener.local_addr().expect("the bound address").port();
    std::thread::spawn(move || {
        use std::io::{Read as _, Write as _};
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request);
            let answer = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{page}",
                page.len()
            );
            let _ = stream.write_all(answer.as_bytes());
            let _ = stream.flush();
        }
    });
    port
}

/// Every text part of a tool answer, joined.
fn text_of(parts: &[forge_primitives::browser::BrowserPart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            forge_primitives::browser::BrowserPart::Text { text } => Some(text.as_str()),
            forge_primitives::browser::BrowserPart::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn move_event() -> BrowserInputEvent {
    BrowserInputEvent::Mouse {
        action: MouseAction::Move,
        x: 10.0,
        y: 10.0,
        button: 0,
        clicks: 0,
        modifiers: InputModifiers::default(),
    }
}

/// **A profile opens, drives a page and streams frames, and the controller
/// role is one viewer's at a time.** The second viewer's take moves the role
/// away from the first, and a release stops input the moment it happens.
///
/// **Ignored by default**: it needs a machine with the built host bundle and
/// the vendored driver stack installed, which CI does not have. The gate for
/// a browser change is `cargo nextest run -p forge-server --test
/// browser_service --run-ignored ignored-only` on a configured machine.
#[ignore = "needs the built host bundle and the vendored driver stack; run with --run-ignored ignored-only"]
#[tokio::test(flavor = "multi_thread")]
async fn a_profile_drives_a_page_serves_frames_and_has_one_controller() {
    let stack = stack();
    assert!(
        stack.join("node/bin/node").is_file(),
        "the vendored driver stack is not installed at {} - run \
         scripts/vendor_browser_stack.sh with FORGE_BROWSER_STACK_DIR and point \
         FORGE_BROWSER_TEST_STACK at the directory it installs",
        stack.display()
    );
    let scratch = tempfile::tempdir().expect("a scratch root");
    let service = BrowserService::at(scratch.path().to_path_buf(), stack);
    let seat = SessionSlot::lead("TestOrg", "proj");

    // **A fresh profile has no session tab until a page call adopts one.**
    // The call starts the host, the driver and the tab; attach resolves to
    // the seat's current tab, which this call made.
    service
        .call(&seat, "browser_snapshot", json!({}))
        .await
        .expect("the first page call opens the session's tab");
    let attachment = service.attach(7, &seat, None).await.expect("the seat's profile opens");
    let key = attachment.key.clone();
    let tab = attachment.tab;
    let mut frames = attachment.frames;
    let frame = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if let Some(frame) = frames.borrow_and_update().clone() {
                break frame;
            }
            frames.changed().await.expect("the frame channel is open");
        }
    })
    .await
    .expect("a frame arrives");
    assert!(!frame.encoded.is_empty(), "the frame carries the host's encoded pixels");
    assert_eq!(frame.profile, attachment.id, "the frame names the profile's wire id");
    assert_eq!(frame.tab, tab, "and the tab the viewer attached to");

    // The page is really driven: a navigation answers from the driver.
    let answer = service
        .call(
            &seat,
            "browser_navigate",
            json!({ "url": "data:text/html,<title>Forge Probe</title><h1 id=p>probe</h1>" }),
        )
        .await
        .expect("the page navigates");
    assert!(!answer.is_empty(), "the driver answered with the page's own words");

    // **One controller at a time.** A take moves the role; the previous
    // holder's input is refused from that moment on.
    service.take_control(&key, tab, 7).await.expect("the first viewer takes control");
    service
        .input(&key, tab, 7, 1, &move_event())
        .await
        .expect("the controller's input is accepted");
    service.take_control(&key, tab, 8).await.expect("a second take moves the role");
    assert!(
        service.input(&key, tab, 7, 2, &move_event()).await.is_err(),
        "the previous controller's input stops when the role moves",
    );
    service
        .input(&key, tab, 8, 1, &move_event())
        .await
        .expect("the new controller's input is accepted");
    service.release_control(&key, tab, 8).await;
    assert!(
        service.input(&key, tab, 8, 2, &move_event()).await.is_err(),
        "input stops the moment the role is released",
    );

    service.detach(7).await;
    service.close(&seat).await.expect("the profile closes");
}

/// **A requested tab is paired through the real host and driver.** The
/// cfg(test) seam pushes a listing instead; this is the production path -
/// a host `TabNew`, the echoed id, the fresh driver's listing and the
/// navigation that renames the row.
#[ignore = "needs the built host bundle and the vendored driver stack; run with --run-ignored ignored-only"]
#[tokio::test(flavor = "multi_thread")]
async fn a_requested_tab_is_created_and_named_by_the_real_host() {
    let stack = stack();
    assert!(
        stack.join("node/bin/node").is_file(),
        "the vendored driver stack is not installed at {} - run \
         scripts/vendor_browser_stack.sh with FORGE_BROWSER_STACK_DIR and point \
         FORGE_BROWSER_TEST_STACK at the directory it installs",
        stack.display()
    );
    let scratch = tempfile::tempdir().expect("a scratch root");
    let service = BrowserService::at(scratch.path().to_path_buf(), stack);
    let seat = SessionSlot::lead("TestOrg", "proj");

    service.call(&seat, "browser_snapshot", json!({})).await.expect("the first tab opens");
    let before = service.tabs(&seat).await.expect("rows").rows;
    assert_eq!(before.len(), 1, "one boot tab is adopted by the first call");

    service
        .call(
            &seat,
            "browser_tabs",
            json!({
                "action": "new",
                "url": "data:text/html,<title>Pairing Probe</title><h1>pairing</h1>",
            }),
        )
        .await
        .expect("the requested tab is created");
    let rows = service.tabs(&seat).await.expect("rows").rows;
    assert_eq!(rows.len(), 2, "the new tab joins the profile without displacing the first");
    assert!(
        rows.iter().any(|row| row.title.contains("Pairing Probe")),
        "the new row takes the page's own title: {rows:?}",
    );
    assert!(rows.iter().all(|row| row.owner == seat), "both tabs belong to the asking seat");
    let listed = service
        .call(&seat, "browser_tabs", json!({ "action": "list" }))
        .await
        .expect("the list answers");
    let text = listed.iter().find_map(|part| match part {
        forge_primitives::browser::BrowserPart::Text { text } => Some(text.clone()),
        forge_primitives::browser::BrowserPart::Image { .. } => None,
    });
    assert!(text.as_deref().unwrap_or_default().contains("Pairing Probe"), "{listed:?}");

    service.close(&seat).await.expect("the profile closes");
}

/// **A host killed under a viewer takes its rows with it, and the next call
/// brings a fresh host up.** The monitor's own path - the real process
/// ending - is what publishes the empty list and forgets the host's parks.
#[ignore = "needs the built host bundle and the vendored driver stack; run with --run-ignored ignored-only"]
#[tokio::test(flavor = "multi_thread")]
async fn a_killed_host_publishes_no_tabs_and_its_successor_starts_empty() {
    let stack = stack();
    assert!(
        stack.join("node/bin/node").is_file(),
        "the vendored driver stack is not installed at {} - run \
         scripts/vendor_browser_stack.sh with FORGE_BROWSER_STACK_DIR and point \
         FORGE_BROWSER_TEST_STACK at the directory it installs",
        stack.display()
    );
    let scratch = tempfile::tempdir().expect("a scratch root");
    let service = BrowserService::at(scratch.path().to_path_buf(), stack);
    let seat = SessionSlot::lead("TestOrg", "proj");
    service.call(&seat, "browser_snapshot", json!({})).await.expect("the first tab opens");
    let tab = service.tabs(&seat).await.expect("rows").rows[0].number;
    service.hand_off_started(&seat, tab).await.expect("the tab is parked");
    assert_eq!(
        service.tabs(&seat).await.expect("rows").rows[0].state,
        forge_primitives::browser::BrowserTabState::Held,
        "a park reads as held before the kill",
    );

    let key = service.profile_for(&seat).expect("the profile key");
    let dir = service.profile_dir(&key);
    let record = forge_browser::spawn::read_record(&dir).expect("the host's record");
    let killed = std::process::Command::new("kill")
        .args(["-9", &record.pid.to_string()])
        .status()
        .expect("kill runs");
    assert!(killed.success(), "the host is signalled");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let rows = service.tabs(&seat).await.expect("rows");
        if rows.rows.is_empty() && !rows.up {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the dead host's rows and up flag outlived it: {rows:?}",
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The next call starts a fresh host; nothing of the old host's tabs --
    // or its park -- comes back.
    service.call(&seat, "browser_snapshot", json!({})).await.expect("the next call warms");
    let rows = service.tabs(&seat).await.expect("rows").rows;
    assert_eq!(rows.len(), 1, "the successor starts with one adopted tab");
    assert_eq!(
        rows[0].state,
        forge_primitives::browser::BrowserTabState::Idle,
        "the old park is not carried onto the successor",
    );

    service.close(&seat).await.expect("the profile closes");
}

/// **An end-to-end smoke check that a closed tab leaves no page behind.** On
/// the patched stack it cannot fail on a one-half revert, so the real
/// discriminators are `a_nav_watch_whose_focused_page_is_gone_stops_listing`
/// (the watch stops) and `a_tab_listing_never_creates_a_page` with the python
/// patch cases (a listing is a read). This one proves the two together on a
/// live host: on 2026-10-26 the released build made one page and CEF renderer
/// a second here until the host was killed with 821 targets (#2581).
#[ignore = "needs the built host bundle and the vendored driver stack; run with --run-ignored ignored-only"]
#[tokio::test(flavor = "multi_thread")]
async fn a_closed_tab_leaves_no_page_behind() {
    let stack = stack();
    assert!(
        stack.join("node/bin/node").is_file(),
        "the vendored driver stack is not installed at {} - run \
         scripts/vendor_browser_stack.sh with FORGE_BROWSER_STACK_DIR and point \
         FORGE_BROWSER_TEST_STACK at the directory it installs",
        stack.display()
    );
    let scratch = tempfile::tempdir().expect("a scratch root");
    let service = BrowserService::at(scratch.path().to_path_buf(), stack);
    let seat = SessionSlot::lead("TestOrg", "proj");

    // One navigate, one evaluate: the leak needs no polling page and no
    // second call.
    service
        .call(
            &seat,
            "browser_navigate",
            json!({ "url": "data:text/html,<title>probe</title><h1>probe</h1>" }),
        )
        .await
        .expect("the page opens");
    service
        .call(&seat, "browser_evaluate", json!({ "function": "() => 1 + 1" }))
        .await
        .expect("the page is driven");

    let key = service.profile_for(&seat).expect("the profile key");
    let dir = service.profile_dir(&key);
    let record = forge_browser::spawn::read_record(&dir).expect("the host's record");
    // **The guard goes on before the close**: a failed assertion must not
    // leave the host running.
    let _guard = HostGuard { pid: record.pid, ring: record.ring.clone() };

    // **The baseline proves the endpoint answers.** `page_targets` returns an
    // empty vec on every failure path, so a zero after the close means
    // nothing unless the same read saw the page before it.
    let before = forge_browser::spawn::page_targets(record.port).await;
    assert_eq!(before.len(), 1, "the tab's page is there before the close: {before:?}");
    service.call(&seat, "browser_close", json!({})).await.expect("the tab closes");

    // The history watch retries every 500 ms, so five seconds is many ticks.
    tokio::time::sleep(Duration::from_secs(5)).await;
    let targets = forge_browser::spawn::page_targets(record.port).await;
    assert!(
        targets.is_empty(),
        "a closed tab leaves no page behind, and every page here is a renderer: {targets:?}",
    );

    service.close(&seat).await.expect("the profile closes");
}

/// **A tab listing never creates a page.** Upstream's `browser_tabs` list arm
/// called `ensureTab`, which makes a page whenever the driver has none; the
/// vendored patch keeps the listing a read (see
/// `scripts/patch_browser_stack.mjs`). Without it every listing of an empty
/// browser adds a page target and a renderer (#2581).
#[ignore = "needs the built host bundle and the vendored driver stack; run with --run-ignored ignored-only"]
#[tokio::test(flavor = "multi_thread")]
async fn a_tab_listing_never_creates_a_page() {
    use forge_browser::driver::{Driver, cli_path, node_path};

    let stack = stack();
    assert!(
        stack.join("node/bin/node").is_file(),
        "the vendored driver stack is not installed at {} - run \
         scripts/vendor_browser_stack.sh with FORGE_BROWSER_STACK_DIR and point \
         FORGE_BROWSER_TEST_STACK at the directory it installs",
        stack.display()
    );
    let scratch = tempfile::tempdir().expect("a scratch root");
    let profile = scratch.path().join("listing").join("cef");
    let exe = forge_browser::host_bundle::checked_executable().expect("the host bundle");
    let port = forge_browser::spawn::free_port().expect("a free port");
    let ring = format!("/forge-listing-{}", std::process::id());
    let frames = scratch.path().join("frames.sock");
    let (pid, _input, _events) = forge_browser::spawn::spawn_with(
        &exe,
        &profile,
        port,
        &ring,
        800,
        600,
        1,
        (15, 75),
        &frames,
    )
    .expect("the host spawns");
    let _guard = HostGuard { pid, ring: ring.clone() };
    forge_browser::spawn::wait_ready_for(pid, port, &ring).await.expect("the host is ready");
    let chooser =
        std::path::PathBuf::from(format!("/tmp/forge-listing-{}.sock", std::process::id()));
    let driver = Driver::start(
        &node_path(&stack),
        &cli_path(&stack),
        &format!("http://127.0.0.1:{port}"),
        &scratch.path().join("output"),
        &chooser,
    )
    .await
    .expect("the driver starts");

    // No tab is open, so every listing is a listing of nothing. The driver
    // sees the close when its own page event lands, so a listing in that
    // window may report the close; what it must never do is make a page.
    for (target, url) in forge_browser::spawn::page_targets(port).await {
        assert!(
            forge_browser::spawn::close_target(port, &target).await,
            "the boot page {url} closes",
        );
    }
    let settle = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match driver.tabs_within(Duration::from_secs(20)).await {
            Ok(rows) => {
                assert!(rows.is_empty(), "a closed page is not listed: {rows:?}");
                break;
            }
            Err(why) => assert!(
                tokio::time::Instant::now() < settle,
                "the listing never answered an empty browser: {why}",
            ),
        }
        let targets = forge_browser::spawn::page_targets(port).await;
        assert!(targets.is_empty(), "a listing of an empty browser creates no page: {targets:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    for round in 0..3 {
        let listed = driver.tabs_within(Duration::from_secs(20)).await;
        assert!(listed.is_ok(), "round {round}: the listing answers: {listed:?}");
        assert!(listed.unwrap_or_default().is_empty(), "round {round}: there is nothing to list");
        let targets = forge_browser::spawn::page_targets(port).await;
        assert!(targets.is_empty(), "round {round}: a listing creates no page: {targets:?}");
    }
}

/// **A challenge frame's presence is not its liveness, and a region the probe
/// could not read is never "no CAPTCHA here".** The measured case is Google's
/// EXPIRED reCAPTCHA overlay: an iframe parked at `top: -10000px` with
/// `visibility: hidden`, whose rect is still a real rect at viewport
/// y = -13576, so presence and geometry both read it as live.
///
/// The frames here are the states the signals have to separate: a widget the
/// page is showing, the expired one, one covered by another element (the hit
/// test), one hidden by an ancestor's opacity, one hidden by an ancestor's
/// `filter: opacity(0)`, one inside an ancestor's `display: none`,
/// Turnstile's own frame, a Cloudflare-branded frame that is neither
/// Turnstile's host nor its name, an unrelated ad frame that must not be
/// classified, an object and an embed whose documents are not read, and a
/// challenge inside a shadow root the top-level query cannot see.
#[ignore = "needs the built host bundle and the vendored driver stack; run with --run-ignored ignored-only"]
#[tokio::test(flavor = "multi_thread")]
async fn a_challenge_state_separates_a_live_widget_from_an_expired_one() {
    let stack = stack();
    assert!(
        stack.join("node/bin/node").is_file(),
        "the vendored driver stack is not installed at {} - run \
         scripts/vendor_browser_stack.sh with FORGE_BROWSER_STACK_DIR and point \
         FORGE_BROWSER_TEST_STACK at the directory it installs",
        stack.display()
    );
    let scratch = tempfile::tempdir().expect("a scratch root");
    let service = BrowserService::at(scratch.path().to_path_buf(), stack);
    let seat = SessionSlot::lead("TestOrg", "proj");

    // The expired frame carries the vendor in its SRC, the way the measured
    // one did, and Turnstile's carries the challenge host with no "turnstile"
    // in it, so the host marker is the one that has to match; the rest carry
    // it in the title, which is how each vendor titles its frame.
    let fixture = "<iframe src='about:blank' title='reCAPTCHA live' style='position:absolute;\
            left:100px;top:100px;width:200px;height:100px;border:0'></iframe>\
        <iframe src='about:blank#recaptcha' title='expired overlay' \
            style='position:absolute;left:120px;top:-10000px;width:200px;height:100px;\
            border:0;visibility:hidden'></iframe>\
        <iframe src='about:blank' title='hCaptcha covered' style='position:absolute;\
            left:100px;top:300px;width:200px;height:100px;border:0'></iframe>\
        <div style='position:absolute;left:100px;top:300px;width:200px;height:100px;\
            background:rgb(238,238,238)'></div>\
        <iframe src='about:blank#https://challenges.cloudflare.com/cdn-cgi/\
            challenge-platform/h/b/orchestrate/chl_page/v1' \
            title='Widget containing a Cloudflare security challenge' \
            style='position:absolute;left:700px;top:100px;width:200px;height:100px;\
            border:0'></iframe>\
        <div style='position:absolute;left:400px;top:300px;width:200px;height:100px;\
            opacity:0'>\
            <iframe src='about:blank' title='reCAPTCHA faded' \
                style='width:200px;height:100px;border:0'></iframe>\
        </div>\
        <div style='position:absolute;left:700px;top:300px;width:200px;height:100px;\
            filter:opacity(0)'>\
            <iframe src='about:blank' title='reCAPTCHA filtered' \
                style='width:200px;height:100px;border:0'></iframe>\
        </div>\
        <div style='position:absolute;left:1100px;top:500px;width:200px;height:100px;\
            display:none'>\
            <iframe src='about:blank' title='reCAPTCHA displayless' \
                style='width:200px;height:100px;border:0'></iframe>\
        </div>\
        <iframe src='about:blank' title='Sponsored placement' style='position:absolute;\
            left:1100px;top:300px;width:200px;height:100px;border:0'></iframe>\
        <iframe src='about:blank' title='Cloudflare analytics beacon' \
            style='position:absolute;left:400px;top:500px;width:200px;height:100px;\
            border:0'></iframe>\
        <object data='about:blank' type='text/html' title='Plugin surface' \
            style='position:absolute;left:100px;top:600px;width:200px;height:100px'></object>\
        <embed src='about:blank' type='text/html' title='Embedded surface' \
            style='position:absolute;left:400px;top:600px;width:200px;height:100px'>\
        <div id='shadow-host' style='position:absolute;left:1100px;top:100px;\
            width:200px;height:100px'></div>";
    // **The fixture is installed into a blank page** rather than navigated
    // to as a data URL: the challenge probe reads a settled document, and a
    // data-URL navigation snapshots the page as part of its own answer.
    service
        .call(
            &seat,
            "browser_navigate",
            json!({ "url": "data:text/html,<title>fixture</title><h1>fixture</h1>" }),
        )
        .await
        .expect("a blank page opens");
    // A script cannot be installed through `innerHTML`, so the shadow root is
    // attached here: its reCAPTCHA frame is the one the top-level query misses.
    let install = format!(
        "() => {{ document.body.innerHTML = {}; \
         const host = document.getElementById('shadow-host'); \
         host.attachShadow({{ mode: 'open' }}).innerHTML = \
         \"<iframe src='about:blank' title='reCAPTCHA in shadow' \
         style='width:200px;height:100px;border:0'></iframe>\"; }}",
        serde_json::to_string(&fixture).expect("the fixture is a JSON string"),
    );
    service
        .call(&seat, "browser_evaluate", json!({ "function": install }))
        .await
        .expect("the fixture is installed");

    let answer = service
        .call(&seat, "browser_challenge_state", json!({}))
        .await
        .expect("the challenge state answers");
    let text = answer
        .iter()
        .find_map(|part| match part {
            forge_primitives::browser::BrowserPart::Text { text } => Some(text.clone()),
            forge_primitives::browser::BrowserPart::Image { .. } => None,
        })
        .unwrap_or_default();
    let state: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|why| panic!("the answer is the report and nothing else ({why}): {text}"));

    assert_eq!(state["challengePresent"], json!(true), "{state}");
    assert_eq!(state["complete"], json!(false), "five regions went unread: {state}");
    let unseen = state["unseen"].as_array().expect("the answer lists unseen regions");
    assert_eq!(
        unseen.len(),
        5,
        "an ad frame, a Cloudflare bead, an object, an embed and a shadow root: {state}",
    );
    for (kind, title, why) in [
        ("iframe", "Sponsored placement", "an ad frame is not a challenge"),
        (
            "iframe",
            "Cloudflare analytics beacon",
            "a Cloudflare-branded frame that is not the challenge host is not a challenge",
        ),
        ("object", "Plugin surface", "an object document is not read either"),
        ("embed", "Embedded surface", "an embed document is not read either"),
    ] {
        assert!(
            unseen
                .iter()
                .any(|region| region["kind"] == json!(kind) && region["title"] == json!(title)),
            "{why}, and it is named as unread: {state}",
        );
    }
    assert!(
        unseen.iter().any(|region| region["kind"] == json!("shadow-root")
            && region["host"].as_str().unwrap_or_default().contains("shadow-host")),
        "the shadow root is named as unread: {state}",
    );

    let challenges = state["challenges"].as_array().expect("the answer lists challenges");
    assert_eq!(challenges.len(), 7, "only the vendor frames are classified: {state}");
    for title in ["Sponsored placement", "Cloudflare analytics beacon"] {
        assert!(
            challenges.iter().all(|challenge| challenge["title"] != json!(title)),
            "{title} is not a challenge: {state}",
        );
    }
    let frame = |title: &str| {
        challenges
            .iter()
            .find(|challenge| challenge["title"] == json!(title))
            .unwrap_or_else(|| panic!("{title} is reported: {state}"))
    };

    let live = frame("reCAPTCHA live");
    assert_eq!(live["vendor"], json!("reCAPTCHA"), "{live}");
    assert_eq!(live["present"], json!(true), "{live}");
    assert_eq!(live["visible"], json!(true), "{live}");
    assert_eq!(live["onScreen"], json!(true), "{live}");
    assert_eq!(live["hitTestable"], json!(true), "nothing covers it: {live}");
    assert_eq!(live["live"], json!(true), "{live}");

    let expired = frame("expired overlay");
    assert_eq!(expired["vendor"], json!("reCAPTCHA"), "the src names the vendor: {expired}");
    assert_eq!(
        expired["present"],
        json!(true),
        "the expired frame IS present - presence is the trap, not the answer: {expired}",
    );
    assert_eq!(expired["visible"], json!(false), "visibility: hidden: {expired}");
    assert_eq!(expired["onScreen"], json!(false), "a rect at y = -10000 is off screen: {expired}");
    assert_eq!(expired["hitTestable"], json!(false), "{expired}");
    assert_eq!(expired["live"], json!(false), "{expired}");
    assert!(
        expired["rect"]["y"].as_f64().unwrap_or_default() < 0.0,
        "the off-screen rect is still reported, which is why geometry alone reads it as live: \
         {expired}",
    );

    let covered = frame("hCaptcha covered");
    assert_eq!(covered["visible"], json!(true), "{covered}");
    assert_eq!(covered["onScreen"], json!(true), "{covered}");
    assert_eq!(
        covered["hitTestable"],
        json!(false),
        "a frame under another element is not what a click would reach: {covered}",
    );
    assert_eq!(covered["live"], json!(false), "{covered}");

    // The frame's own computed opacity is 1; only the ancestor walk sees it.
    let faded = frame("reCAPTCHA faded");
    assert_eq!(
        faded["visible"],
        json!(false),
        "an ancestor's opacity: 0 leaves the frame on screen and hit-testable but invisible: \
         {faded}",
    );
    assert_eq!(faded["onScreen"], json!(true), "the frame is on screen: {faded}");
    assert_eq!(faded["hitTestable"], json!(true), "a click would reach it: {faded}");
    assert_eq!(faded["live"], json!(false), "an invisible widget is not live: {faded}");

    let displayless = frame("reCAPTCHA displayless");
    assert_eq!(
        displayless["visible"],
        json!(false),
        "an ancestor's display: none hides the frame: {displayless}",
    );
    assert_eq!(displayless["onScreen"], json!(false), "a hidden frame has no rect: {displayless}");
    assert_eq!(displayless["hitTestable"], json!(false), "{displayless}");
    assert_eq!(displayless["live"], json!(false), "{displayless}");

    let filtered = frame("reCAPTCHA filtered");
    assert_eq!(
        filtered["visible"],
        json!(false),
        "filter: opacity(0) on an ancestor hides the frame too: {filtered}",
    );
    assert_eq!(filtered["onScreen"], json!(true), "the frame is on screen: {filtered}");
    assert_eq!(filtered["hitTestable"], json!(true), "a click would reach it: {filtered}");
    assert_eq!(filtered["live"], json!(false), "an invisible widget is not live: {filtered}");

    let turnstile = frame("Widget containing a Cloudflare security challenge");
    assert_eq!(turnstile["vendor"], json!("Turnstile"), "{turnstile}");
    assert_eq!(turnstile["live"], json!(true), "{turnstile}");

    service.close(&seat).await.expect("the profile closes");
}

/// **A driver rebuild renames every page, and a session keeps its own.**
/// Creating a tab rebuilds the driver, and a fresh Playwright backend gives
/// every page a new guid. The first seat's next evaluate must still be its
/// own document, not a new about:blank, and the tab number it was shown must
/// still be the one a hand-off accepts (row c00bd4de).
#[ignore = "needs the built host bundle and the vendored driver stack; run with --run-ignored ignored-only"]
#[tokio::test(flavor = "multi_thread")]
async fn a_driver_rebuild_keeps_a_sessions_page() {
    let stack = stack();
    assert!(
        stack.join("node/bin/node").is_file(),
        "the vendored driver stack is not installed at {} - run \
         scripts/vendor_browser_stack.sh with FORGE_BROWSER_STACK_DIR and point \
         FORGE_BROWSER_TEST_STACK at the directory it installs",
        stack.display()
    );
    let scratch = tempfile::tempdir().expect("a scratch root");
    let service = BrowserService::at(scratch.path().to_path_buf(), stack);
    let one = SessionSlot::worker("TestOrg", "proj", "one");
    let two = SessionSlot::worker("TestOrg", "proj", "two");
    let page = "data:text/html,<title>kept</title><h1 id=kept>kept</h1>";

    service
        .call(&one, "browser_navigate", json!({ "url": page }))
        .await
        .expect("the first seat's page opens");
    let tab = service.tabs(&one).await.expect("rows").rows[0].number;

    // **A second seat's first call creates a tab**, and creating one rebuilds
    // the driver, renaming every page the first seat holds.
    service.call(&two, "browser_snapshot", json!({})).await.expect("the second seat opens");

    let answer = service
        .call(
            &one,
            "browser_evaluate",
            json!({ "function": "() => [location.href, !!document.getElementById('kept')]" }),
        )
        .await
        .expect("the first seat's page is still driven");
    let text = answer
        .iter()
        .find_map(|part| match part {
            forge_primitives::browser::BrowserPart::Text { text } => Some(text.clone()),
            forge_primitives::browser::BrowserPart::Image { .. } => None,
        })
        .unwrap_or_default();
    assert!(text.contains(page), "the first seat's own document answers: {text}");
    assert!(text.contains("true"), "and its content is there, not a blank tab: {text}");

    let rows = service.tabs(&one).await.expect("rows").rows;
    assert!(
        rows.iter().any(|row| row.number == tab),
        "the seat keeps the tab it was shown: {rows:?}",
    );
    service.hand_off_started(&one, tab).await.expect("the number a tool printed is accepted");
    service.hand_off_finished(&one, tab).await;

    service.close(&one).await.expect("the profile closes");
}

/// The page's own report of its surface: the viewport it lays out at, the
/// outer size, and the ratio it paints at.
const LAYOUT: &str = "() => ({ innerWidth: window.innerWidth, innerHeight: window.innerHeight, \
                      outerWidth: window.outerWidth, outerHeight: window.outerHeight, \
                      dpr: window.devicePixelRatio })";

/// One `browser_evaluate` of [`LAYOUT`], as the object the page returned.
///
/// **The driver frames an answer as `### Result` followed by `### Ran
/// Playwright code`**, so the object is cut out of the framing before it is
/// parsed.
async fn page_layout(service: &BrowserService, seat: &SessionSlot) -> serde_json::Value {
    let answer = service
        .call(seat, "browser_evaluate", json!({ "function": LAYOUT }))
        .await
        .expect("the page evaluates");
    let text = answer
        .iter()
        .filter_map(|part| match part {
            forge_primitives::browser::BrowserPart::Text { text } => Some(text.as_str()),
            forge_primitives::browser::BrowserPart::Image { .. } => None,
        })
        .collect::<String>();
    let result = text.split_once("### Result").map_or(text.as_str(), |(_, result)| result);
    let result = result.split_once("\n### ").map_or(result, |(object, _)| object);
    let (Some(start), Some(end)) = (result.find('{'), result.rfind('}')) else {
        panic!("the page's answer carries an object: {text}");
    };
    serde_json::from_str(&result[start..=end])
        .unwrap_or_else(|why| panic!("the page's answer is JSON ({why}): {text}"))
}

/// The layout the page settles on. **A ring request reaches the page on the
/// host's own pump**, so a read taken right after a resize can still see the
/// size before it.
async fn wait_for_layout(
    service: &BrowserService,
    seat: &SessionSlot,
    what: &str,
    want: serde_json::Value,
) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let layout = page_layout(service, seat).await;
        if layout == want {
            return layout;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{what}: the page never reported {want}, it reported {layout}",
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// **An agent's `browser_resize` moves the painted surface, and the page
/// follows it.** The tool used to go to the driver's `page.setViewportSize`:
/// the page was told one size while the host painted another, and the device
/// pixel ratio was left at 1 for the rest of the host's life. It now moves
/// the profile's ring, the same path a viewer's own resize takes, so the
/// viewport, the outer size and the ratio all move together.
///
/// **That window is then the person's to replace**: their own surface report
/// is a resize like any other, and the last one wins.
#[ignore = "needs the built host bundle and the vendored driver stack; run with --run-ignored ignored-only"]
#[tokio::test(flavor = "multi_thread")]
async fn an_agent_resize_is_the_window_until_the_next_resize() {
    let stack = stack();
    assert!(
        stack.join("node/bin/node").is_file(),
        "the vendored driver stack is not installed at {} - run \
         scripts/vendor_browser_stack.sh with FORGE_BROWSER_STACK_DIR and point \
         FORGE_BROWSER_TEST_STACK at the directory it installs",
        stack.display()
    );
    let scratch = tempfile::tempdir().expect("a scratch root");
    let service = BrowserService::at(scratch.path().to_path_buf(), stack);
    let seat = SessionSlot::lead("TestOrg", "proj");
    service
        .call(
            &seat,
            "browser_navigate",
            json!({ "url": "data:text/html,<h1 id=probe>resize probe</h1>" }),
        )
        .await
        .expect("the page opens");
    let attachment = service.attach(7, &seat, None).await.expect("the seat's profile attaches");
    let (key, tab) = (attachment.key.clone(), attachment.tab);

    // **A viewer's surface first**, so the page is painted at a ratio of 2
    // the way a Retina surface paints it.
    service.take_control(&key, tab, 7).await.expect("the viewer takes control");
    service.resize(&key, tab, 7, 2880, 2128, 2.0).await;
    wait_for_layout(
        &service,
        &seat,
        "a viewer's resize paints the page at its own size and ratio",
        json!({
            "innerWidth": 1440, "innerHeight": 1064,
            "outerWidth": 1440, "outerHeight": 1064, "dpr": 2,
        }),
    )
    .await;

    // **The agent's resize is the same window.** The page's viewport, its
    // outer size and its ratio move together, and the ratio stays 2.
    service
        .call(&seat, "browser_resize", json!({ "width": 1120, "height": 760 }))
        .await
        .expect("the agent resizes the window");
    wait_for_layout(
        &service,
        &seat,
        "an agent's resize moves the painted surface and keeps the ratio",
        json!({
            "innerWidth": 1120, "innerHeight": 760,
            "outerWidth": 1120, "outerHeight": 760, "dpr": 2,
        }),
    )
    .await;

    // **The person's own surface report is a resize, and the last one wins.**
    // The client re-sends its region on a layout change and when it takes
    // control; either puts its own window back.
    service.resize(&key, tab, 7, 2880, 2128, 2.0).await;
    wait_for_layout(
        &service,
        &seat,
        "a viewer's report after the tool's resize puts the viewer's window back",
        json!({
            "innerWidth": 1440, "innerHeight": 1064,
            "outerWidth": 1440, "outerHeight": 1064, "dpr": 2,
        }),
    )
    .await;

    service.close(&seat).await.expect("the profile closes");
}

/// **A page's popup joins the profile as a tab under its opener's session**,
/// and the session goes back to the opener when the popup closes.
///
/// The join is the server's half: the host's `TabCreated` carries an opener,
/// the driver's listing names the popup's page, and the registry files the tab
/// under the opener's session and makes it that session's current tab.
///
/// **This is the red the driver's auto-attach cost.** A CEF popup whose target
/// an attached DevTools client auto-attaches to never starts its own first
/// document: the popup stayed at an empty URL, answered no page-domain
/// command, and the driver's next listing hung on it until the call's bound.
#[ignore = "needs the built host bundle and the vendored driver stack; run with --run-ignored ignored-only"]
#[tokio::test(flavor = "multi_thread")]
async fn a_pages_popup_joins_the_profile_and_hands_the_session_back() {
    // **A page whose button keeps the handle `window.open` answered**, the way
    // a sign-in library does, and opens its popup **straight at an address** -
    // the shape a sign-in flow really arrives in, and the one navigation this
    // engine commits for a popup an auto-attached client holds. The popup's
    // own page says what it is, so the row and the call are both checked.
    const POPUP: &str = "<!doctype html><meta charset=utf-8>\
        <title>popup-target</title><h1>sign-in page</h1>";
    let popup_port = serve_page(POPUP);
    let page = format!(
        "<!doctype html><meta charset=utf-8>\
         <style>html,body{{margin:0;padding:0}}\
         #b{{position:absolute;left:0;top:0;width:300px;height:120px;font-size:20px}}</style>\
         <button id=b onclick=\"window.__popup=window.open(\
         'http://127.0.0.1:{popup_port}/','signin');\
         document.title=window.__popup?'popup-open':'popup-blocked'\">sign in</button>"
    );

    let stack = stack();
    assert!(
        stack.join("node/bin/node").is_file(),
        "the vendored driver stack is not installed at {} - run \
         scripts/vendor_browser_stack.sh with FORGE_BROWSER_STACK_DIR and point \
         FORGE_BROWSER_TEST_STACK at the directory it installs",
        stack.display()
    );
    let scratch = tempfile::tempdir().expect("a scratch root");
    // **No pairing call here.** A popup has no call behind it, so the pass
    // that turns its birth into a row must be the one the service starts for
    // itself; asking for it in the test would let a service that never starts
    // one pass.
    let service = BrowserService::at(scratch.path().to_path_buf(), stack);
    let seat = SessionSlot::lead("TestOrg", "proj");
    let url = format!("http://127.0.0.1:{}/", serve_page(Box::leak(page.into_boxed_str())));

    service
        .call(&seat, "browser_snapshot", json!({}))
        .await
        .expect("the first page call opens the session's tab");
    let attachment = service.attach(7, &seat, None).await.expect("the seat's profile opens");
    let key = attachment.key.clone();
    let opener = attachment.tab;
    let mut frames = attachment.frames;
    let first = next_frame(&mut frames, opener, None).await;
    // **The strip's half of the row is the pushed list.** A viewer holds this
    // stream open, and a popup's row has to arrive on it with no read behind
    // it; `tabs()` would be the pull a person never makes.
    let mut strip = service.tab_stream(&seat).await.expect("the strip's list");

    service
        .call(&seat, "browser_navigate", json!({ "url": url }))
        .await
        .expect("the page navigates");
    // **A gesture only lands on a page that has painted since it navigated.**
    let _painted = next_frame(&mut frames, opener, Some(first.seq)).await;

    service.take_control(&key, opener, 7).await.expect("the viewer takes control");
    for (seq, action) in [(1_u64, MouseAction::Press), (2, MouseAction::Release)] {
        service
            .input(
                &key,
                opener,
                7,
                seq,
                &BrowserInputEvent::Mouse {
                    action,
                    x: 60.0,
                    y: 60.0,
                    button: 0,
                    clicks: 1,
                    modifiers: InputModifiers::default(),
                },
            )
            .await
            .expect("the click is accepted");
    }

    // **Nobody calls: the row appears on its own.** A person clicking a sign-in
    // button makes no request of the service, so the birth's own pairing pass
    // is what has to produce it - and it has to reach the strip's stream.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let rows = loop {
        let rows = strip.borrow_and_update().rows.clone();
        if rows.len() == 2 {
            break rows;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the popup's row never reached the strip without a call: {rows:?}"
        );
        let _ = tokio::time::timeout(Duration::from_millis(100), strip.changed()).await;
    };
    let popup = rows.iter().map(|row| row.number).max().expect("two rows");
    assert_ne!(popup, opener, "the popup has a number of its own");
    assert!(rows.iter().all(|row| row.owner == seat), "both tabs are the opener's session's");
    // **The popup's row names the address it asked for**, which is the page a
    // person sees after clicking sign in - not a blank the host left behind.
    let popup_url = format!("http://127.0.0.1:{popup_port}/");
    assert!(
        rows.iter().any(|row| row.number == popup && row.url == popup_url),
        "the popup's row is the page it opened: {rows:?}",
    );

    // **The list is bounded like a call.** A listing that never returns is the
    // wedge a stuck popup target used to cause; a future one names it here
    // rather than running into nextest's own kill.
    let listed = tokio::time::timeout(
        Duration::from_secs(30),
        service.call(&seat, "browser_tabs", json!({ "action": "list" })),
    )
    .await
    .expect("the tab listing answers within its bound")
    .expect("the list answers");
    let text = text_of(&listed);
    assert!(
        text.lines().any(|line| line.starts_with(&format!("- {popup}: (current) "))),
        "the popup is the session's current tab: {text}",
    );

    // **The popup is driven like any other page, through the driver's own
    // page**. That is the claim the book makes, and a snapshot is the call
    // that proves it: CEF re-reports the target as a page once its document
    // starts, and the driver's backend answers for it as one of its tabs.
    let snapshot = tokio::time::timeout(
        Duration::from_secs(30),
        service.call(&seat, "browser_snapshot", json!({})),
    )
    .await
    .expect("a call against the popup answers within its bound")
    .expect("the popup answers a page call");
    let snapshot = text_of(&snapshot);
    assert!(
        snapshot.contains("sign-in page"),
        "the popup is driven like any other page: {snapshot}",
    );

    service
        .call(&seat, "browser_tabs", json!({ "action": "close", "index": popup }))
        .await
        .expect("the popup closes");
    let rows = service.tabs(&seat).await.expect("rows").rows;
    assert_eq!(rows.len(), 1, "the opener is the tab left: {rows:?}");
    assert_eq!(rows[0].number, opener);
    let listed = service
        .call(&seat, "browser_tabs", json!({ "action": "list" }))
        .await
        .expect("the list answers");
    let text = text_of(&listed);
    assert!(
        text.lines().any(|line| line.starts_with(&format!("- {opener}: (current) "))),
        "the session is back on the opener: {text}",
    );

    service.detach(7).await;
    service.close(&seat).await.expect("the profile closes");
}
