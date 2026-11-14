//! The page's focused node reaches the host's control line, and focus
//! leaving it reports the drop.
//!
//! **The phone's keyboard rests on this one assumption**: that CEF calls the
//! renderer's `on_focused_node_changed` through this host's chain - the
//! render-process handler forwards it as a process message, the browser
//! process settles it per frame - and that the answer reaches the server as
//! a `Focus` line. Everything else about the feature is unit-tested; this is
//! the live half that no unit test can pin.
//!
//! **These need the real bundle**: they stage the workspace's own host the
//! way the server ships it, then drive it as the server does.

#![cfg(target_os = "macos")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod support;

use std::io::{Read as _, Write as _};
use std::time::{Duration, Instant};

use forge_browser::control::{HostEvent, parse};
use forge_browser::window::{Backlog, Cdp};
use serde_json::json;

/// How long CEF is given to create, paint and answer.
const BOUND: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(50);

/// A page with one field and one button. **The field is focused and blurred
/// by script**, which is the same DOM focus change a person's tap makes.
const PAGE: &str = "<!doctype html><meta charset=utf-8>\
<style>html,body{margin:0;padding:0}</style>\
<input id=f>\
<button id=b>a button</button>\
<script>document.title='ready';</script>";

#[test]
fn an_editable_focus_and_its_drop_reach_the_control_line() {
    let mut live = Live::start(PAGE);
    let Live { host, runtime, cdp, .. } = &mut live;

    runtime.block_on(async {
        // **The input takes focus**: the field's caret is what the phone's
        // keyboard follows, so the host owes a `true`.
        evaluate(cdp, "document.getElementById('f').focus()").await;
        let at = wait_for_focus(host, 0, true);

        // **Focus leaves it**: the page is owed the drop, and so is the
        // phone's keyboard.
        evaluate(cdp, "document.getElementById('f').blur()").await;
        wait_for_focus(host, at, false);
    });
}

/// **A person's own click is the witness the phone's keyboard rests on.**
/// The gesture reaches the page as CEF's trusted mouse calls, the field takes
/// focus, and the host's line is what the server routes; a navigation then
/// drops it through the load reset.
#[test]
fn a_click_into_a_field_raises_the_line_and_a_navigation_drops_it() {
    let mut live = Live::start(PAGE);
    let Live { host, runtime, cdp, page_port, .. } = &mut live;

    runtime.block_on(async {
        // **The first click after a host starts can be spent activating the
        // app**, so it is retried until the host reports the focus.
        let at = click_until_focus(host, 0, true, BOUND);

        // **A new document has no focused node**: the host's load reset owes
        // the drop even before the new page paints.
        let other = format!("http://127.0.0.1:{page_port}/other");
        cdp.call("Page.navigate", json!({ "url": other }), None)
            .await
            .expect("the navigation is taken");
        wait_for_focus(host, at, false);
    });
}

/// A page with a same-origin frame carrying a field. **The frame is the
/// key**: the host must file its report under the frame that sent it.
const FRAME_PAGE: &str = "<!doctype html><meta charset=utf-8>\
<style>html,body{margin:0;padding:0}iframe{width:600px;height:400px;border:0}</style>\
<title>ready</title>\
<iframe id=f src=\"/inner\"></iframe>";

const FRAME_INNER: &str = "<!doctype html><meta charset=utf-8>\
<style>html,body{margin:0;padding:0}input{width:300px;height:40px}</style>\
<input id=g>";

/// **A frame's field is keyed by the frame that sent it.** The tab's line
/// follows the iframe's field; a regression to a constant key would let the
/// main frame's answer erase it, and the frame's next document must not
/// keep its old `true`.
#[test]
fn an_iframes_field_is_keyed_by_its_frame_and_its_navigation_drops_it() {
    let mut live = Live::start_with_inner(FRAME_PAGE, FRAME_INNER);
    let Live { host, runtime, cdp, .. } = &mut live;
    runtime.block_on(async {
        // The frame's document loads with the main page; the retrying
        // evaluate waits for it.
        evaluate(cdp, "document.getElementById('f').contentDocument.getElementById('g').focus()")
            .await;
        let at = wait_for_focus(host, 0, true);
        // The frame's field is the tab's editable node, and losing it is the
        // drop.
        evaluate(cdp, "document.getElementById('f').contentDocument.getElementById('g').blur()")
            .await;
        let at = wait_for_focus(host, at, false);
        // **A frame's next document is the old one gone.** Focusing again
        // then navigating the frame drops the answer, though the main page
        // never loads.
        evaluate(cdp, "document.getElementById('f').contentDocument.getElementById('g').focus()")
            .await;
        let at = wait_for_focus(host, at, true);
        evaluate(cdp, "document.getElementById('f').src='/inner?again'").await;
        let _ = wait_for_focus(host, at, false);
    });
}

/// **A renderer's death takes the tab's answer with it.** A focused field in
/// a dead renderer is no focused node, so the tab must drop rather than
/// leave the phone's keyboard up over a dead page.
#[test]
fn a_crashed_renderer_drops_the_tabs_answer() {
    let mut live = Live::start(PAGE);
    let Live { host, runtime, cdp, .. } = &mut live;
    runtime.block_on(async {
        evaluate(cdp, "document.getElementById('f').focus()").await;
        let at = wait_for_focus(host, 0, true);
        // **The crash never answers**: the target dies mid-call, so the
        // timeout is expected and the host's request handler is the witness.
        let _ =
            tokio::time::timeout(Duration::from_secs(2), cdp.call("Page.crash", json!({}), None))
                .await;
        let _ = wait_for_focus(host, at, false);
    });
}

/// Click the page's field until the host reports an editable focus at or
/// after `from`, answering the index the next wait starts from.
fn click_until_focus(
    host: &mut support::Host,
    from: usize,
    editable: bool,
    bound: Duration,
) -> usize {
    let deadline = Instant::now() + bound;
    loop {
        support::click(host, 40, 10);
        std::thread::sleep(Duration::from_millis(250));
        if let Some(at) = focus_after(host, from, editable) {
            return at + 1;
        }
        assert!(
            Instant::now() < deadline,
            "no focus line with editable={editable} after clicking the field: {:?} (log: {})",
            host.events(),
            host.log_tail(),
        );
    }
}

/// Run one expression and wait for it to answer. **A renderer that is still
/// loading is retried**: an expression that throws (a subframe's document not
/// there yet) answers with `exceptionDetails`, and only the deadline fails
/// the run.
async fn evaluate(cdp: &mut Cdp, expression: &str) {
    let deadline = Instant::now() + BOUND;
    loop {
        let attempt = tokio::time::timeout(
            Duration::from_secs(5),
            cdp.call("Runtime.evaluate", json!({ "expression": expression }), None),
        )
        .await;
        let ran = matches!(
            &attempt,
            Ok(Ok(reading)) if reading.get("exceptionDetails").is_none()
        );
        if ran {
            return;
        }
        assert!(Instant::now() < deadline, "the page never ran '{expression}': {attempt:?}");
        tokio::time::sleep(POLL).await;
    }
}

/// Whether a `Focus` line with `editable` appeared at or after `from`.
///
/// **The sequence matters**: a drop that follows a raise is a different fact
/// from one already in the log, so every wait starts after the line before
/// it.
fn focus_after(host: &support::Host, from: usize, editable: bool) -> Option<usize> {
    host.events()
        .iter()
        .enumerate()
        .skip(from)
        .find(|(_, line)| {
            matches!(
                parse(line),
                Some(HostEvent::Focus { tab: 1, editable: seen }) if seen == editable
            )
        })
        .map(|(at, _)| at)
}

/// Wait for a `Focus` line with `editable` at or after `from`, answering the
/// index the next wait starts from.
fn wait_for_focus(host: &support::Host, from: usize, editable: bool) -> usize {
    let deadline = Instant::now() + BOUND;
    loop {
        if let Some(at) = focus_after(host, from, editable) {
            return at + 1;
        }
        assert!(
            Instant::now() < deadline,
            "no focus line with editable={editable} after {from}: {:?} (log: {})",
            host.events(),
            host.log_tail(),
        );
        std::thread::sleep(POLL);
    }
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
        Self::start_with_inner(body, body)
    }

    /// One host whose page embeds `inner` at `/inner`, for the frame-keying
    /// case.
    fn start_with_inner(main: &str, inner: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let scratch = tempfile::tempdir().expect("a temp dir");
        let exe = support::host_executable(scratch.path());
        let profile = scratch.path().join("profile");
        std::fs::create_dir_all(&profile).expect("the profile dir");
        let port = support::free_port();
        let ring = format!("/forge-focus-test-{}-{n}", std::process::id());
        let log = scratch.path().join("host.log");
        let (page_port, server) = serve(main, inner);
        let host =
            support::Host::start_with_events(&exe, &profile, port, &ring, "800x600", &[], &log);
        // **A host with no viewer is paused.** The service unpauses the ring
        // when a viewer attaches, and a page that never paints never settles
        // its focus, so the test does the same.
        let mapped = wait_for_ring(&ring, &host);
        mapped.pause(false);
        mapped.key();
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

    /// Bring the served page in, reconnecting a target that closed between
    /// the attach and the navigation.
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

impl Drop for Live {
    fn drop(&mut self) {
        let _ = self.host.terminate(Duration::from_secs(10));
        forge_browser::osr::Ring::unlink(&self.ring);
    }
}

/// The host's ring, once the host has mapped it.
fn wait_for_ring(name: &str, host: &support::Host) -> forge_browser::osr::Ring {
    let deadline = Instant::now() + BOUND;
    loop {
        match forge_browser::osr::Ring::open(name) {
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

/// A DevTools connection bounded like every other wait here.
async fn connect(port: u16, path: &str) -> (Cdp, forge_browser::window::Events) {
    tokio::time::timeout(
        Duration::from_secs(5),
        Cdp::connect_watch(port, path, Backlog::EveryEvent, None, ""),
    )
    .await
    .expect("the page's devtools endpoint answers")
    .expect("the page's devtools endpoint accepts")
}

/// Two pages, served over loopback: `/inner` answers the frame's document
/// and every other path answers the main one. **`data:` and `srcdoc` wedge
/// CEF**, so a live page is always an http one.
fn serve(main: &str, inner: &str) -> (u16, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("a page port");
    let port = listener.local_addr().expect("the address").port();
    let main = main.to_owned();
    let inner = inner.to_owned();
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut read = [0_u8; 2048];
            let taken = stream.read(&mut read).unwrap_or_default();
            let request = String::from_utf8_lossy(&read[..taken]);
            let body = if request.contains(" /inner") { &inner } else { &main };
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
