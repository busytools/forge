//! A page's downloads, live: CEF's handler accepts the download, the file
//! lands beside the profile, and the host announces it on its control pipe.
//!
//! **The `can_download` allowance is what this pins.** CEF asks before a
//! download starts, and a handler that leaves the answer to the bindings'
//! default (`0`) never sees one at all.

#![cfg(target_os = "macos")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod support;

use std::time::{Duration, Instant};

use forge_browser::control::{HostEvent, parse};
use forge_browser::window::{Backlog, Cdp};
use serde_json::json;

/// How long CEF is given to boot, paint and download.
const BOUND: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(50);

/// Wait for the host's answer that `wanted` accepts, failing with what it
/// wrote otherwise. The deadline is the caller's, so priming and the answer
/// share one bound rather than each starting its own sixty seconds.
fn wait_event(
    host: &support::Host,
    wanted: impl Fn(&HostEvent) -> bool,
    deadline: Instant,
) -> HostEvent {
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

/// **A click on a download link brings the file to rest beside the profile,
/// and the host says so.** The server reads that file and hands it to the
/// controlling viewer; what this pins is that the host ever learns of it.
#[tokio::test(flavor = "multi_thread")]
async fn a_pages_download_lands_and_is_announced() {
    let scratch = tempfile::tempdir().expect("a scratch dir");
    let exe = support::host_executable(scratch.path());
    let profile = scratch.path().join("profile");
    let port = support::free_port();
    let ring_name = format!("/forge-dl-{}", std::process::id());
    let log = scratch.path().join("host.log");
    let mut host =
        support::Host::start_with_events(&exe, &profile, port, &ring_name, "800x600", &[], &log);
    let id = support::wait_for_page(port, &log, BOUND);
    let (mut cdp, _events) =
        Cdp::connect_watch(port, &format!("/devtools/page/{id}"), Backlog::EveryEvent)
            .await
            .expect("devtools");
    cdp.call(
        "Runtime.evaluate",
        json!({ "expression": "document.documentElement.style.margin='0'; \
            document.body.style.margin='0'; document.body.innerHTML = '<a id=\"d\" \
            download=\"take.txt\" href=\"data:text/plain,hello\" \
            style=\"position:absolute;left:100px;top:100px;width:200px;height:40px;\
            display:block\">take</a>'; 0" }),
        None,
    )
    .await
    .expect("the link is on the page");

    // **The clicks that come first activate the window rather than reaching
    // the page**, so the one that means something waits on the page having
    // seen a gesture. One deadline covers both halves, so priming cannot spend
    // the answer's own bound.
    let deadline = Instant::now() + BOUND;
    support::prime_clicks(&mut host, &mut cdp, (600, 500), deadline).await;
    support::click(&mut host, 200, 120);

    let HostEvent::Download { tab: _, name, path, size } =
        wait_event(&host, |event| matches!(event, HostEvent::Download { .. }), deadline)
    else {
        panic!("the answer is a download event");
    };
    assert_eq!(name, "take.txt", "the page's suggested name");
    assert_eq!(size, 5, "the file's bytes");
    let path = std::path::Path::new(&path);
    assert!(
        path.starts_with(profile.join("downloads")),
        "the file lands beside the profile: {}",
        path.display(),
    );
    assert_eq!(std::fs::read(path).expect("the downloaded file"), b"hello");

    let status = host.terminate(Duration::from_secs(15));
    assert!(status.success(), "the host exited cleanly: {status} (log: {})", host.log());
}
