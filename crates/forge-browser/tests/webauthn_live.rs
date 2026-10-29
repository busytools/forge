//! The in-app browser offers no WebAuthn, live: a fresh page of a real host
//! sees no `PublicKeyCredential`, in its top frame and in a cross-site
//! subframe, while the page still works as an ordinary page.
//!
//! **The subframe is served from `[::1]` while the page is served from
//! `127.0.0.1`**, so it is a different site from the page and in a renderer
//! of its own. Both are loopback literals, so no name resolution can fail a
//! run; it reports back over `postMessage`, which is the only way into a
//! cross-origin frame, and a patch that reached only the top frame would
//! leave this one reporting the API.
//!
//! **The control is the page's own credentials manager and form.** A
//! `chrome-error://` document, or one that never ran, reports the API absent
//! too; the assertions below also say the served secure document is under
//! test, that `navigator.credentials` is still there, and that a form field
//! takes a value and submits - so the absence is the host's switch, not a
//! page that never loaded.
//!
//! **The switch's name is the trap.** Blink's runtime feature is `WebAuth`;
//! `engine`'s unit test pins that name, and this live test is what catches a
//! CEF upgrade that renames it - the switch would then match nothing and the
//! API would return with the unit test still green.

#![cfg(target_os = "macos")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod support;

use std::io::{Read as _, Write as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use forge_browser::osr::Ring;
use forge_browser::window::{Backlog, Cdp};
use serde_json::{Value, json};

const BOUND: Duration = Duration::from_secs(45);
const POLL_EVERY: Duration = Duration::from_millis(100);

/// One tiny loopback HTTP server on `host`: every connection is answered
/// with what `answer` builds from the request's path.
fn serve(
    host: &str,
    answer: impl Fn(&str) -> String + Send + 'static,
) -> (u16, Arc<AtomicBool>, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind((host, 0)).expect("a loopback listener");
    let port = listener.local_addr().expect("the bound address").port();
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let thread = std::thread::spawn(move || {
        for stream in listener.incoming() {
            if flag.load(Ordering::Acquire) {
                return;
            }
            let Ok(mut stream) = stream else { return };
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request);
            let request = String::from_utf8_lossy(&request);
            let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
            let body = answer(&path);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (port, stop, thread)
}

/// The frame's own document: it reports what it sees to the page that
/// embedded it. The listener is registered before the iframe, so the message
/// cannot arrive ahead of it.
fn frame_page() -> String {
    "<html><body><script>parent.postMessage({type: 'forge-frame', \
     secure: isSecureContext, pkc: typeof PublicKeyCredential, \
     present: 'PublicKeyCredential' in window}, '*');</script></body></html>"
        .to_owned()
}

/// The page under test: a form that records its own submit, and the
/// cross-site subframe's URL.
fn page(frame_url: &str) -> String {
    format!(
        "<html><head><script>window.__frame = null; \
         addEventListener('message', function (e) {{ \
         if (e.data && e.data.type === 'forge-frame') window.__frame = e.data; }});\
         </script></head><body><form id=f><input id=i name=q>\
         <button id=b type=submit>go</button></form>\
         <iframe id=fr src=\"{frame_url}\"></iframe><script>\
         window.__submitted = false; \
         document.getElementById('f').addEventListener('submit', function (e) {{ \
         e.preventDefault(); window.__submitted = true; }});\
         </script></body></html>"
    )
}

async fn evaluate(cdp: &mut Cdp, expression: &str) -> Value {
    let reply = cdp
        .call("Runtime.evaluate", json!({ "expression": expression, "returnByValue": true }), None)
        .await
        .expect("the page answers");
    reply.pointer("/result/value").cloned().unwrap_or(Value::Null)
}

/// Open the served page and wait until both it and its cross-site subframe
/// have reported.
async fn open_page(cdp: &mut Cdp, carried: &str) {
    cdp.call("Page.navigate", json!({ "url": carried }), None).await.expect("the page navigates");
    let deadline = Instant::now() + BOUND;
    let ready = format!(
        "location.href === {carried:?} && document.readyState === 'complete' \
         && !!window.__frame"
    );
    loop {
        if evaluate(cdp, &ready).await.as_bool() == Some(true) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the served page and its subframe never finished loading"
        );
        tokio::time::sleep(POLL_EVERY).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_in_the_host_offers_no_webauthn() {
    let scratch = tempfile::tempdir().expect("a scratch dir");
    let exe = support::host_executable(scratch.path());
    let profile = scratch.path().join("profile");
    std::fs::create_dir_all(&profile).expect("the profile dir");
    let ring_name = format!("/forge-webauthn-{}", std::process::id());
    let log = scratch.path().join("host.log");
    let (frame_port, stop_frame, frame_server) = serve("::1", |_| frame_page());
    let page_server = {
        let frame_url = format!("http://[::1]:{frame_port}/frame");
        serve("127.0.0.1", move |_| page(&frame_url))
    };
    let (page_port, stop_page, page) = page_server;
    let carried = format!("http://127.0.0.1:{page_port}/");
    let port = support::free_port();
    let mut host = support::Host::start(&exe, &profile, port, &ring_name, "800x600", &[], &log);
    forge_browser::spawn::wait_ready_for(host.pid(), port, &ring_name)
        .await
        .unwrap_or_else(|why| panic!("the host never came ready: {why} (log: {})", host.log()));

    let websocket = forge_browser::spawn::page_websocket(port).await.expect("the page's endpoint");
    let path = websocket
        .strip_prefix(&forge_browser::spawn::ws_prefix(port))
        .expect("the endpoint is on the host's port");
    let (mut cdp, _events) = Cdp::connect_watch(port, path, Backlog::EveryEvent)
        .await
        .expect("a page connection");
    open_page(&mut cdp, &carried).await;

    // The control: this is the served secure document, with the credentials
    // manager every page has, so the absence below is not a page that failed.
    let control = evaluate(
        &mut cdp,
        "({secure: window.isSecureContext, credentials: typeof navigator.credentials, \
          fetch: typeof fetch})",
    )
    .await;
    assert_eq!(
        control,
        json!({ "secure": true, "credentials": "object", "fetch": "function" }),
        "the served page is a secure document with the ordinary credentials API: {control}",
    );

    // The property, from the top frame and from the cross-site subframe: the
    // whole API is absent, not just its passkey entry points.
    let top = evaluate(
        &mut cdp,
        "({type: typeof PublicKeyCredential, present: 'PublicKeyCredential' in window})",
    )
    .await;
    assert_eq!(
        top,
        json!({ "type": "undefined", "present": false }),
        "the host's page exposes no PublicKeyCredential: {top}",
    );
    let frame = evaluate(&mut cdp, "window.__frame").await;
    assert_eq!(
        frame,
        json!({ "type": "forge-frame", "secure": true, "pkc": "undefined", "present": false }),
        "a cross-site subframe of the host's page exposes none either: {frame}",
    );

    // The page is still an ordinary page: a field takes a value and the form
    // submits through the page's own listener.
    let form = evaluate(
        &mut cdp,
        "(() => { const field = document.getElementById('i'); field.value = 'typed'; \
          document.getElementById('b').click(); \
          return {value: field.value, submitted: window.__submitted}; })()",
    )
    .await;
    assert_eq!(
        form,
        json!({ "value": "typed", "submitted": true }),
        "the page still takes input and submits: {form}",
    );

    let status = host.terminate(Duration::from_secs(15));
    assert!(status.success(), "the host exited cleanly: {status} (log: {})", host.log());
    Ring::unlink(&ring_name);
    stop_page.store(true, Ordering::Release);
    let _ = std::net::TcpStream::connect(("127.0.0.1", page_port));
    page.join().expect("the page server ends");
    stop_frame.store(true, Ordering::Release);
    let _ = std::net::TcpStream::connect(("::1", frame_port));
    frame_server.join().expect("the frame server ends");
}
