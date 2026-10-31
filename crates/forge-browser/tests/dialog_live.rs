//! A page dialog is answered on a deadline: one nobody answers is dismissed
//! by the host, and the page runs again.
//!
//! **The measured defect was a `confirm()` no one answered**: the host had no
//! JS dialog handler, so the dialog never closed, the page's own thread
//! stopped, and every page-domain CDP call on it waited for ever (#2732).
//!
//! **These need the real bundle**: they stage the workspace's own host the
//! way the server ships it, then drive it as the server does.

#![cfg(target_os = "macos")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod support;

use std::io::{Read as _, Write as _};
use std::time::{Duration, Instant};

use forge_browser::control::HostEvent;
use forge_browser::window::{Backlog, Cdp};
use serde_json::{Value, json};

/// How long CEF is given to create, paint and answer.
const BOUND: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(50);

/// A page that opens a dialog on its own, then records what the answer was.
/// **The title is the page's own proof that its thread ran again**: it is set
/// only after `confirm()` returns.
const PAGE: &str = "<title>ready</title>\
<script>\
setTimeout(() => { \
  const answer = confirm('forge deadline probe'); \
  document.title = answer ? 'accepted' : 'after'; \
}, 100);\
</script>";

#[test]
fn an_unanswered_dialog_is_dismissed_at_its_deadline() {
    let scratch = tempfile::tempdir().expect("a temp dir");
    let exe = support::host_executable(scratch.path());
    let profile = scratch.path().join("profile");
    std::fs::create_dir_all(&profile).expect("the profile dir");
    let port = support::free_port();
    let ring_name = format!("/forge-dialog-test-{}", std::process::id());
    let log = scratch.path().join("host.log");

    let (page_port, page_server) = serve(PAGE);
    let mut host =
        support::Host::start_with_events(&exe, &profile, port, &ring_name, "800x600", &[], &log);
    let page = support::wait_for_page(port, &log, BOUND);

    let runtime =
        tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a runtime");
    runtime.block_on(async {
        let (mut cdp, _events) = connect(port, &format!("/devtools/page/{page}")).await;
        cdp.call("Page.navigate", json!({ "url": format!("http://127.0.0.1:{page_port}/") }), None)
            .await
            .expect("the page navigates");
        wait_for_title(&mut cdp, "ready", BOUND).await;

        // **The host says a dialog is open**, and nobody answers it.
        let shown = wait_for_dialog(&host, false, BOUND);
        assert_eq!(shown_dialog(&shown), "confirm", "the page's confirm is the dialog: {shown:?}");

        // **The deadline is the host's**, not the page's: the dismissal
        // arrives on its own, and the event says so.
        let dismissed = wait_for_dialog(&host, true, BOUND);
        assert_eq!(shown_dialog(&dismissed), "confirm", "{dismissed:?}");

        // **The page's own thread runs again**: only the dismissal can have
        // set the title, because the page never answers its own dialog.
        wait_for_title(&mut cdp, "after", BOUND).await;
    });

    assert!(host.terminate(Duration::from_secs(10)).success(), "the host ends on SIGTERM");
    drop(page_server);
    forge_browser::osr::Ring::unlink(&ring_name);
}

/// The tab and dialog type of one host event line.
fn shown_dialog(line: &str) -> String {
    match forge_browser::control::parse(line) {
        Some(HostEvent::JsDialog { tab, dialog, .. }) => {
            assert_eq!(tab, 1, "the boot tab raised it: {line}");
            dialog
        }
        other => panic!("not a dialog event: {other:?}"),
    }
}

/// Wait for one JavaScript dialog event with the answer `dismissed` says.
fn wait_for_dialog(host: &support::Host, dismissed: bool, bound: Duration) -> String {
    let deadline = Instant::now() + bound;
    loop {
        if let Some(line) = host.events().into_iter().find(|line| {
            matches!(
                forge_browser::control::parse(line),
                Some(HostEvent::JsDialog { dismissed: event_dismissed, .. })
                    if event_dismissed == dismissed
            )
        }) {
            return line;
        }
        assert!(
            Instant::now() < deadline,
            "no dialog event with dismissed={dismissed}: {:?} (log: {})",
            host.events(),
            host.log_tail(),
        );
        std::thread::sleep(POLL);
    }
}

/// A DevTools connection bounded like every other wait here, so a target that
/// accepts a socket and never upgrades names the failure instead of hanging.
async fn connect(port: u16, path: &str) -> (Cdp, forge_browser::window::Events) {
    tokio::time::timeout(
        Duration::from_secs(5),
        Cdp::connect_watch(port, path, Backlog::EveryEvent),
    )
    .await
    .expect("the page's devtools endpoint answers")
    .expect("the page's devtools endpoint accepts")
}

/// The page's own title, once it reads `want`.
async fn wait_for_title(cdp: &mut Cdp, want: &str, bound: Duration) {
    let deadline = Instant::now() + bound;
    loop {
        let value = tokio::time::timeout(
            Duration::from_secs(5),
            cdp.call(
                "Runtime.evaluate",
                json!({ "expression": "document.title", "returnByValue": true }),
                None,
            ),
        )
        .await;
        let title = match &value {
            Ok(Ok(reading)) => reading.pointer("/result/value").and_then(Value::as_str),
            _ => None,
        };
        if title == Some(want) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the page never read '{want}'; its last reading was {value:?}"
        );
        tokio::time::sleep(POLL).await;
    }
}

/// One page, served over loopback: **`data:` and `srcdoc` wedge CEF**, so a
/// live page is always an http one.
fn serve(body: &str) -> (u16, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("a page port");
    let port = listener.local_addr().expect("the address").port();
    let body = body.to_owned();
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut read = [0_u8; 2048];
            let _ = stream.read(&mut read);
            let answer = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(answer.as_bytes());
        }
    });
    (port, handle)
}
