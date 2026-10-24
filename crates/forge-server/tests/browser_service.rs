//! The server's browser fleet: a profile's browser is driven from the
//! server, its frames arrive, and exactly one viewer holds the control role.
//!
//! **A live browser, not a mock.** The test spawns the installed host bundle
//! and the vendored Playwright driver, so it needs both in place:
//! `scripts/stage-browser-host.sh` and `scripts/vendor_browser_stack.sh`
//! (with `FORGE_BROWSER_STACK_DIR` naming the server's stack directory).

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
         scripts/vendor_browser_stack.sh with FORGE_BROWSER_STACK_DIR",
        stack.display()
    );
    let scratch = tempfile::tempdir().expect("a scratch root");
    let service = BrowserService::at(scratch.path().to_path_buf(), stack);
    let seat = SessionSlot::lead("TestOrg", "proj");

    // Opening the shared profile starts its host and driver; the page paints.
    let attachment = service.attach(7, &seat, None).await.expect("the shared profile opens");
    let key = attachment.key.clone();
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
    service.take_control(&key, 7).await.expect("the first viewer takes control");
    service.input(&key, 7, 1, &move_event()).await.expect("the controller's input is accepted");
    service.take_control(&key, 8).await.expect("a second take moves the role");
    assert!(
        service.input(&key, 7, 2, &move_event()).await.is_err(),
        "the previous controller's input stops when the role moves",
    );
    service.input(&key, 8, 1, &move_event()).await.expect("the new controller's input is accepted");
    service.release_control(&key, 8).await;
    assert!(
        service.input(&key, 8, 2, &move_event()).await.is_err(),
        "input stops the moment the role is released",
    );

    service.detach(7).await;
    service.close(&seat, None).await.expect("the profile closes");
}
