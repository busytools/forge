//! Page dialogs on a deadline: one nobody answers is dismissed by the host,
//! one the agent answers is not, and both leave the page running.
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

use forge_browser::control::{HostEvent, JsDialogState};
use forge_browser::input::HostInput;
use forge_browser::jsdialog::DEADLINE_MS;
use forge_browser::window::{Backlog, Cdp};
use serde_json::{Value, json};

/// How long CEF is given to create, paint and answer.
const BOUND: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(50);
/// The deadline plus the polish a loaded machine may add to the pump. **The
/// pin is a stretch of the real five seconds**: a deadline of twenty-five
/// seconds lands outside it.
const DEADLINE_AND_POLISH: Duration = Duration::from_millis(DEADLINE_MS + 10_000);

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

/// A page that navigates away on a gesture, with a beforeunload prompt armed.
const LEAVING: &str = "<title>leaving</title>\
<script>\
function go() { \
  window.onbeforeunload = () => 'leave?'; \
  location.href = '/other'; \
}\
</script>";

#[test]
fn an_unanswered_dialog_is_dismissed_at_its_deadline() {
    let mut live = Live::start(PAGE);
    let Live { host, runtime, cdp, .. } = &mut live;

    runtime.block_on(async {
        // **The host says a dialog is open**, and nobody answers it.
        wait_for_dialog(host, "confirm", JsDialogState::Shown);
        let shown_at = Instant::now();

        // **The dismissal is the host's deadline**, not the page's: it
        // arrives on its own. **Only the upper bound is the clock's**: the
        // raise is read from the host's pipe, so when this test first sees it
        // can lag the host's own deadline.
        let dismissed = wait_for_dialog(host, "confirm", JsDialogState::Expired);
        let elapsed = shown_at.elapsed();
        assert!(
            elapsed <= DEADLINE_AND_POLISH,
            "the dialog outlived its deadline by too much: {elapsed:?} ({dismissed})",
        );

        // **The page's own thread runs again**: only the dismissal can have
        // set the title, because the page never answers its own dialog.
        wait_for_title(cdp, "after", BOUND).await;
    });
}

#[test]
fn an_answered_dialog_is_not_dismissed_at_the_deadline() {
    let mut live = Live::start(PAGE);
    let Live { host, runtime, cdp, .. } = &mut live;

    runtime.block_on(async {
        wait_for_dialog(host, "confirm", JsDialogState::Shown);

        // **The answer comes from the agent's tool**, which hands it to the
        // host over the driver's socket. **CDP cannot answer a CEF dialog**:
        // `Page.handleJavaScriptDialog` reports "No dialog is showing"
        // whether or not a host handler exists (measured 2026-11-01), so this
        // is the only path an agent's answer can take.
        host.input(&forge_browser::input::host_line(&HostInput::Dialog {
            tab: 1,
            accept: true,
            prompt: None,
        }));
        wait_for_dialog(host, "confirm", JsDialogState::Answered);
        wait_for_title(cdp, "accepted", BOUND).await;

        // **The deadline is cancelled by that answer.** A second dismissal
        // after the deadline would be the host answering a dialog twice.
        std::thread::sleep(Duration::from_millis(DEADLINE_MS + 3_000));
        assert!(
            !saw_state(host, "confirm", JsDialogState::Expired),
            "an answered dialog was dismissed at the deadline: {:?}",
            host.events(),
        );
    });
}

#[test]
fn a_before_unload_dialog_is_dismissed_at_its_deadline() {
    let mut live = Live::start(LEAVING);
    let Live { host, runtime, cdp, .. } = &mut live;

    runtime.block_on(async {
        // **The gesture is the page's own**, so the navigation counts as
        // user-initiated and the browser asks before leaving.
        cdp.call("Runtime.evaluate", json!({ "expression": "go()", "userGesture": true }), None)
            .await
            .expect("the page takes the gesture");

        wait_for_dialog(host, "beforeunload", JsDialogState::Shown);
        wait_for_dialog(host, "beforeunload", JsDialogState::Expired);

        // **The page stays and runs again**: a dismissal is "do not leave",
        // and the renderer is not paused behind it any more.
        let still = tokio::time::timeout(
            Duration::from_secs(5),
            cdp.call(
                "Runtime.evaluate",
                json!({ "expression": "document.title", "returnByValue": true }),
                None,
            ),
        )
        .await
        .expect("the page answers after the dismissal")
        .expect("the page reads its title");
        assert_eq!(
            still.pointer("/result/value").and_then(Value::as_str),
            Some("leaving"),
            "a dismissed beforeunload leaves the page where it was",
        );
    });
}

/// One host, one served page and one DevTools connection, ended together.
struct Live {
    host: support::Host,
    /// The scratch dir holding the profile and the host log; **alive for the
    /// test**, so the running host never loses its cache under it.
    _scratch: tempfile::TempDir,
    runtime: tokio::runtime::Runtime,
    cdp: Cdp,
    ring: String,
    /// The host's DevTools port and log, for reconnecting a page target.
    port: u16,
    log: std::path::PathBuf,
    page_port: u16,
    _server: std::thread::JoinHandle<()>,
}

impl Live {
    fn start(body: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let scratch = tempfile::tempdir().expect("a temp dir");
        let exe = support::host_executable(scratch.path());
        let profile = scratch.path().join("profile");
        std::fs::create_dir_all(&profile).expect("the profile dir");
        let port = support::free_port();
        let ring = format!("/forge-dialog-test-{}-{n}", std::process::id());
        let log = scratch.path().join("host.log");
        let (page_port, server) = serve(body);
        let host =
            support::Host::start_with_events(&exe, &profile, port, &ring, "800x600", &[], &log);
        let page = support::wait_for_page(port, &log, BOUND);
        let runtime =
            tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a runtime");
        let (cdp, _events) = runtime.block_on(connect(port, &format!("/devtools/page/{page}")));
        let mut live = Self {
            host,
            _scratch: scratch,
            runtime,
            cdp,
            ring,
            port,
            log,
            page_port,
            _server: server,
        };
        live.open_page();
        live
    }

    /// Bring the served page in. **A page target that closes between the
    /// attach and the navigation is the fresh session, not the host**: the
    /// connection is remade against the page the endpoint lists now, and the
    /// navigation retried.
    fn open_page(&mut self) {
        let url = format!("http://127.0.0.1:{}/", self.page_port);
        for attempt in 0..3 {
            let result =
                self.runtime.block_on(self.cdp.call("Page.navigate", json!({ "url": url }), None));
            if result.is_ok() {
                return;
            }
            let page = support::wait_for_page(self.port, &self.log, BOUND);
            let (cdp, _events) =
                self.runtime.block_on(connect(self.port, &format!("/devtools/page/{page}")));
            self.cdp = cdp;
            assert!(attempt < 2, "the page never navigated: {result:?}");
        }
    }
}

/// Wait for one JavaScript dialog event in `state`.
fn wait_for_dialog(host: &support::Host, wanted: &str, state: JsDialogState) -> String {
    let deadline = Instant::now() + BOUND;
    loop {
        if let Some(line) = host.events().into_iter().find(|line| {
            matches!(
                forge_browser::control::parse(line),
                Some(HostEvent::JsDialog {
                    tab: 1,
                    ref dialog,
                    state: event_state,
                    ..
                }) if dialog.as_str() == wanted && event_state == state
            )
        }) {
            return line;
        }
        assert!(
            Instant::now() < deadline,
            "no {wanted} dialog event in {state:?}: {:?} (log: {})",
            host.events(),
            host.log_tail(),
        );
        std::thread::sleep(POLL);
    }
}

/// Whether this dialog ever reached `state`.
fn saw_state(host: &support::Host, wanted: &str, state: JsDialogState) -> bool {
    host.events().into_iter().any(|line| {
        matches!(
            forge_browser::control::parse(&line),
            Some(HostEvent::JsDialog {
                ref dialog,
                state: event_state,
                ..
            }) if dialog.as_str() == wanted && event_state == state
        )
    })
}

impl Drop for Live {
    fn drop(&mut self) {
        let _ = self.host.terminate(Duration::from_secs(10));
        forge_browser::osr::Ring::unlink(&self.ring);
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
