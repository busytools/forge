//! Two tabs in one host: each is a windowless browser of its own, each
//! paints, and closing one leaves the other live.
//!
//! **This is the spike the whole tabbed browser rests on.** CEF must create a
//! second windowless browser in one host process with its own client and
//! render handler, give it its own page target, and take it down without the
//! first. A host that cannot do that cannot serve a profile more than one tab.
//!
//! **These need the real bundle**: they stage the workspace's own host the
//! way the server ships it, then drive it as the server does.

#![cfg(target_os = "macos")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod support;

use std::time::{Duration, Instant};

use forge_browser::channel;
use forge_browser::osr::Ring;

/// How long CEF is given to create, paint and close a tab.
const BOUND: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(50);

#[test]
fn a_second_tab_paints_and_the_first_survives_its_close() {
    let scratch = tempfile::tempdir().expect("a temp dir");
    let exe = support::host_executable(scratch.path());
    let profile = scratch.path().join("profile");
    std::fs::create_dir_all(&profile).expect("the profile dir");
    let port = support::free_port();
    let ring_name = format!("/forge-tabs-test-{}", std::process::id());
    let log = scratch.path().join("host.log");

    // **Every tab encodes into its own stream and the server sees them
    // all.** The frames pin the paint half of the spike: a page target that
    // never hands a renderer a buffer would still appear in `/json/list`.
    let (tx, rx) = std::sync::mpsc::channel();
    let socket = channel::socket_path(&profile);
    let _reader = channel::serve(&socket, move |message| {
        if let channel::ChannelMessage::Frame(frame) = message {
            let _ = tx.send(frame);
        }
    })
    .expect("the frame socket is served");

    let mut host =
        support::Host::start_with_events(&exe, &profile, port, &ring_name, "800x600", &[], &log);
    let first = support::wait_for_page(port, &log, BOUND);

    // **A host with no viewer is paused**, exactly as the service leaves it.
    let ring = wait_for_ring(&ring_name, &host);
    ring.pause(false);
    ring.key();
    let first_frame = rx.recv_timeout(BOUND).expect("the first tab paints");
    assert_eq!(first_frame.tab, 1, "the first tab's frame names tab 1");

    host.input(r#"{"kind":"tab_new","url":"data:text/html,<h1>second</h1>"}"#);
    let deadline = Instant::now() + BOUND;
    let second = loop {
        let pages = pages(port);
        let found = pages.iter().find(|id| id.as_str() != first.as_str()).cloned();
        if pages.len() == 2
            && let Some(found) = found
        {
            break found;
        }
        assert!(Instant::now() < deadline, "the second tab never appeared: {:?}", host.log());
        std::thread::sleep(POLL);
    };
    let created = host.events().into_iter().find(|line| line.contains("tab_created"));
    assert!(created.is_some(), "the host reports the tab it made: {:?}", host.events());

    // **The second tab paints its own stream.** Its frame names tab 2, so
    // the two renderers are not one renderer shared by two page targets.
    let deadline = Instant::now() + BOUND;
    let second_frame = loop {
        match rx.recv_timeout(POLL) {
            Ok(frame) if frame.tab == 2 => break frame,
            Ok(_) => {}
            Err(_) => {
                assert!(
                    Instant::now() < deadline,
                    "the second tab never painted: {:?}",
                    host.log()
                );
            }
        }
    };
    assert!(second_frame.key, "a tab's first frame is a key frame");
    println!(
        "first page {first}, second page {second}, second frame tab {} ({}x{}), events {:?}",
        second_frame.tab,
        second_frame.width,
        second_frame.height,
        host.events(),
    );

    host.input(r#"{"kind":"tab_close","tab":2}"#);
    let deadline = Instant::now() + BOUND;
    while pages(port).iter().any(|id| id == &second) {
        assert!(Instant::now() < deadline, "the second tab never closed");
        std::thread::sleep(POLL);
    }
    let after = pages(port);
    assert!(after.iter().any(|id| id == &first), "the first tab is still live");
    println!("after the second tab closed, pages {after:?}");
    assert!(host.terminate(Duration::from_secs(10)).success(), "the host ends on SIGTERM");
    Ring::unlink(&ring_name);
}

/// The host's ring, once the host has mapped it.
fn wait_for_ring(name: &str, host: &support::Host) -> Ring {
    let deadline = Instant::now() + BOUND;
    loop {
        match Ring::open(name) {
            Ok(ring) => return ring,
            Err(why) => {
                assert!(
                    Instant::now() < deadline,
                    "the host's ring never appeared: {why} (log: {})",
                    host.log(),
                );
                std::thread::sleep(POLL);
            }
        }
    }
}

/// The page targets the host's DevTools endpoint lists.
fn pages(port: u16) -> Vec<String> {
    let answer =
        support::http_get(&format!("http://127.0.0.1:{port}/json/list")).unwrap_or_default();
    let body = answer.split_once("\r\n\r\n").map(|(_, body)| body).unwrap_or_default();
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|parsed| parsed.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter(|entry| entry.get("type").and_then(serde_json::Value::as_str) == Some("page"))
        .filter_map(|entry| entry.get("id")?.as_str().map(str::to_owned))
        .collect()
}
