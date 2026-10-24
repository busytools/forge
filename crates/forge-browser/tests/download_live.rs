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
use forge_browser::input::{HostInput, host_line};
use forge_browser::window::Cdp;
use serde_json::json;

/// How long CEF is given to boot, paint and download.
const BOUND: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(50);

/// One full click, written the way the server writes it. **The first pointer
/// event after a host starts is swallowed by activation**, so a test that
/// means to click something clicks away first.
fn click(host: &mut support::Host, x: i32, y: i32) {
    for action in [
        forge_primitives::browser::MouseAction::Press,
        forge_primitives::browser::MouseAction::Release,
    ] {
        host.input(&host_line(&HostInput::Mouse {
            tab: 1,
            action,
            x,
            y,
            button: 0,
            clicks: 1,
            modifiers: forge_primitives::browser::InputModifiers::default(),
        }));
    }
}

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
        Cdp::connect_watch(port, &format!("/devtools/page/{id}")).await.expect("devtools");
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

    click(&mut host, 600, 500);
    tokio::time::sleep(Duration::from_millis(300)).await;
    click(&mut host, 200, 120);

    let HostEvent::Download { name, path, size } =
        wait_event(&host, |event| matches!(event, HostEvent::Download { .. }))
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
