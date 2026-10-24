//! Screen recording, live: the host takes the viewport cues the server writes
//! and answers with a finished MP4.
//!
//! **The whole path is pinned here**: the page is laid out at the file's own
//! size, the host's encoder takes the composed frames it paints, and the
//! stop's answer names a file whose container is real - `ftyp` at the head and
//! a `moov` written by the finish - rather than a file still being written.
//!
//! **These need the real bundle**: they stage the workspace's own host the
//! way the server ships it, then drive it as the server does.

#![cfg(target_os = "macos")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod support;

use std::time::{Duration, Instant};

use forge_browser::control::{HostEvent, parse};
use forge_browser::input::{HostInput, host_line};
use forge_browser::osr::Ring;
use forge_browser::window::Cdp;
use serde_json::json;

/// How long CEF is given to boot, paint and answer a take.
const BOUND: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(50);

/// Wait for the host's answer that `wanted` accepts, failing with what it
/// wrote otherwise.
fn wait_event(host: &support::Host, wanted: impl Fn(&HostEvent) -> bool) -> HostEvent {
    let deadline = Instant::now() + BOUND;
    loop {
        if let Some(event) =
            host.events().iter().filter_map(|line| parse(line)).find(|event| wanted(event))
        {
            return event;
        }
        assert!(
            Instant::now() < deadline,
            "the host never answered; it wrote {:?} (log: {})",
            host.events(),
            host.log(),
        );
        std::thread::sleep(POLL);
    }
}

/// **A take is a real MP4 of the recording's viewport.** The page is red at
/// the file's size, the take runs for a moment, and the stop answers a file
/// with an MP4 container and a finished movie atom.
#[tokio::test(flavor = "multi_thread")]
async fn a_recording_writes_the_viewport_to_an_mp4() {
    let scratch = tempfile::tempdir().expect("a scratch dir");
    let exe = support::host_executable(scratch.path());
    let profile = scratch.path().join("profile");
    std::fs::create_dir_all(&profile).expect("the profile dir");
    let port = support::free_port();
    let ring_name = format!("/forge-rec-{}", std::process::id());
    let log = scratch.path().join("host.log");
    let mut host =
        support::Host::start_with_events(&exe, &profile, port, &ring_name, "800x600", &[], &log);
    let id = support::wait_for_page(port, &log, BOUND);
    let (mut cdp, _events) =
        Cdp::connect_watch(port, &format!("/devtools/page/{id}")).await.expect("devtools");
    cdp.call(
        "Runtime.evaluate",
        json!({ "expression": "document.body.style.margin='0'; \
            document.body.style.background='#c81e1e'; \
            document.body.innerHTML='<h1 style=\"font:48px sans-serif;color:#fff\">take</h1>'; 0" }),
        None,
    )
    .await
    .expect("the page paints");

    let ring = {
        let deadline = Instant::now() + BOUND;
        loop {
            match Ring::open(&ring_name) {
                Ok(ring) => break ring,
                Err(why) => {
                    assert!(
                        Instant::now() < deadline,
                        "the host's ring never appeared: {why} (log: {})",
                        host.log()
                    );
                    std::thread::sleep(POLL);
                }
            }
        }
    };
    // **The take's layout, as the service asks for it**: the page is laid out
    // at the file's own size for the duration.
    ring.request(1280, 720, 1.0);
    let path = scratch.path().join("take.mp4");
    host.input(&host_line(&HostInput::RecordStart {
        tab: 1,
        path: path.to_string_lossy().into_owned(),
    }));

    let HostEvent::Recording { started, error, .. } =
        wait_event(&host, |event| matches!(event, HostEvent::Recording { .. }))
    else {
        panic!("the answer is a recording event");
    };
    assert!(started, "the writer opened: {error:?}");

    // A second of paint, so the file holds frames rather than a bare header.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    host.input(&host_line(&HostInput::RecordStop));

    let HostEvent::Recording { started, seconds, bytes, capped, error } =
        wait_event(&host, |event| matches!(event, HostEvent::Recording { started: false, .. }))
    else {
        panic!("the answer is a recording event");
    };
    assert!(!started);
    assert_eq!(error, None, "the writer finished cleanly");
    assert!(!capped, "a short take is not capped");
    assert!(bytes > 0, "the file has bytes");
    let file = std::fs::read(&path).expect("the recording file");
    assert_eq!(&file[4..8], b"ftyp", "the file is an MP4 container");
    assert!(
        file.windows(4).any(|window| window == b"moov"),
        "the finish wrote the movie atom ({seconds}s, {bytes} bytes)",
    );

    let status = host.terminate(Duration::from_secs(15));
    assert!(status.success(), "the host exited cleanly: {status} (log: {})", host.log());
    Ring::unlink(&ring_name);
}
