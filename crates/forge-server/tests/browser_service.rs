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

/// **A closed tab leaves no page behind.** The driver's tab listing creates a
/// page when it has no current tab, and the history watch used to keep asking
/// after its tab was closed: on 2026-10-26 that made one page and one CEF
/// renderer a second until the host was killed with 821 targets (#2581).
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

    // One navigate, one evaluate, one close: the leak needs no polling page
    // and no second call.
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
    service.call(&seat, "browser_close", json!({})).await.expect("the tab closes");

    let key = service.profile_for(&seat).expect("the profile key");
    let dir = service.profile_dir(&key);
    let record = forge_browser::spawn::read_record(&dir).expect("the host's record");

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
    forge_browser::spawn::wait_ready_for(pid, port, &ring, &profile)
        .await
        .expect("the host is ready");
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
