//! Page dialogs are held until someone answers them: nothing dismisses one on
//! a clock, and the agent's answer is what lets the page run again.
//!
//! **The measured defect was a `confirm()` nobody answered** (#2732) and then
//! a host that answered it behind the person's back after five seconds
//! (#2754). The host now holds every dialog until it is answered.
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
use forge_browser::window::{Backlog, Cdp};
use serde_json::{Value, json};

/// How long CEF is given to create, paint and answer.
const BOUND: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(50);
/// Longer than the five seconds the host used to wait before answering an
/// unanswered dialog itself. **A held dialog outlives it**: that is the pin
/// this file exists for.
const HELD_PAST: Duration = Duration::from_secs(9);

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

/// A page that navigates away from a real click, with a beforeunload prompt
/// armed. **A person's own click is the only activation CEF counts on every
/// machine**: a CDP `userGesture` did not raise the prompt on the CI runner.
const LEAVING: &str = "<!doctype html><meta charset=utf-8>\
<style>html,body{margin:0;padding:0}button{width:300px;height:120px;font-size:24px}</style>\
<button id=leave>Leave</button>\
<script>\
document.getElementById('leave').onclick = function() { \
  window.onbeforeunload = () => 'leave?'; \
  location.href = '/other'; \
};\
document.title = 'ready';\
</script>";

#[test]
fn an_unanswered_dialog_is_held_past_the_deadline() {
    let mut live = Live::start(PAGE);
    let Live { host, runtime, cdp, .. } = &mut live;

    runtime.block_on(async {
        // **The host says a dialog is open**, and nobody answers it.
        wait_for_dialog(host, "confirm", JsDialogState::Shown);

        // **Nothing answers it, and nothing may**: the host used to give up
        // after five seconds and the page resumed as if the person had said
        // no (#2754). The dialog is still there when that time has passed.
        std::thread::sleep(HELD_PAST);
        assert!(
            !saw_state(host, "confirm", JsDialogState::Answered),
            "an unanswered dialog was answered behind the person: {:?}",
            host.events(),
        );

        // **The agent's answer is the way out**, and it reaches the page:
        // only the answer can set the title, because the page never answers
        // its own dialog.
        host.input(&forge_browser::input::host_line(&HostInput::Dialog {
            tab: 1,
            accept: true,
            prompt: None,
        }));
        wait_for_dialog(host, "confirm", JsDialogState::Answered);
        wait_for_title(cdp, "accepted", BOUND).await;
    });
}

#[test]
fn an_answered_dialog_is_answered_once() {
    let mut live = Live::start(PAGE);
    let Live { host, runtime, cdp, .. } = &mut live;

    runtime.block_on(async {
        wait_for_dialog(host, "confirm", JsDialogState::Shown);

        // **The answer comes from the agent's tool**, which hands it to the
        // host over the worker's socket. **CDP cannot answer a CEF dialog**:
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

        // **A second answer has nothing to answer**: the host took the entry
        // on the first one, so nothing reaches the page a second time.
        host.input(&forge_browser::input::host_line(&HostInput::Dialog {
            tab: 1,
            accept: true,
            prompt: None,
        }));
        std::thread::sleep(Duration::from_millis(750));
        assert_eq!(
            count_state(host, JsDialogState::Answered),
            1,
            "a second answer answered the dialog again: {:?}",
            host.events(),
        );
    });
}

#[test]
fn a_before_unload_dialog_is_held_until_answered() {
    let mut live = Live::start(LEAVING);
    let Live { host, runtime, cdp, .. } = &mut live;

    runtime.block_on(async {
        wait_for_title(cdp, "ready", BOUND).await;

        // **The gesture is the person's own**: a press and release on the
        // host's input pipe. The first click after a host starts can be spent
        // activating the app, so it is retried until the host reports the
        // prompt.
        click_until_dialog(host, "beforeunload", JsDialogState::Shown, BOUND);

        // **Held, not dismissed**: a leaving prompt is the same hold as any
        // other, and only an answer moves the page.
        std::thread::sleep(HELD_PAST);
        assert!(
            !saw_state(host, "beforeunload", JsDialogState::Answered),
            "a held beforeunload was answered on a clock: {:?}",
            host.events(),
        );

        // **Dismissing it leaves the page where it was**, and the renderer
        // runs again because the answer closed the modal.
        host.input(&forge_browser::input::host_line(&HostInput::Dialog {
            tab: 1,
            accept: false,
            prompt: None,
        }));
        wait_for_dialog(host, "beforeunload", JsDialogState::Answered);
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
            Some("ready"),
            "a dismissed beforeunload leaves the page where it was",
        );
    });
}

/// Click the page's leave button until the host reports the dialog. **The
/// first pointer events after a host starts are spent activating it**, so a
/// single click is not the proof a person's gesture is.
fn click_until_dialog(
    host: &mut support::Host,
    wanted: &str,
    state: JsDialogState,
    bound: Duration,
) {
    let deadline = Instant::now() + bound;
    loop {
        support::click(host, 60, 60);
        std::thread::sleep(Duration::from_millis(250));
        if saw_state(host, wanted, state) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "no {wanted} dialog after clicking the page: {:?} (log: {})",
            host.events(),
            host.log_tail(),
        );
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
        // **A host with no viewer is paused.** The service unpauses the ring
        // when a viewer attaches; a click test does the same, so the page
        // paints before the gesture.
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

/// How many dialogs the host reported in `state`.
fn count_state(host: &support::Host, state: JsDialogState) -> usize {
    host.events()
        .into_iter()
        .filter(|line| {
            matches!(
                forge_browser::control::parse(line),
                Some(HostEvent::JsDialog { state: event_state, .. }) if event_state == state
            )
        })
        .count()
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
