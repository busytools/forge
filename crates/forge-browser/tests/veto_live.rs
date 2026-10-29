//! The passkey veto against a real page: the live proof the script exists
//! for.
//!
//! The script's shape is pinned by a unit test; what that test cannot say is
//! that the installer actually reaches the page. This one starts the staged
//! host, installs the veto on a served page, and asks the page's own
//! `navigator.credentials` for a passkey - the answer must be the rejection.

#![cfg(target_os = "macos")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod support;

use std::io::{Read as _, Write as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use forge_browser::osr::Ring;
use forge_browser::veto;
use forge_browser::window::{Backlog, Cdp};

/// One page served from loopback: a secure context, so `navigator.credentials`
/// exists and the veto's own answer is what the page sees.
///
/// **Every connection is answered, not just the first.** The document is not
/// always the first socket a browser opens, and one stray connection against a
/// single `accept` left the document refused - the test then ran against a
/// `chrome-error://chromewebdata/` page, where `navigator.credentials` does
/// not exist (measured on CI, run 40311192668).
fn serve_page() -> (u16, Arc<AtomicBool>, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("a loopback listener");
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
            let body = "<html><body>veto</body></html>";
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

#[tokio::test(flavor = "multi_thread")]
async fn the_installed_veto_refuses_a_page_a_passkey_call() {
    let scratch = tempfile::tempdir().expect("a scratch dir");
    let exe = support::host_executable(scratch.path());
    let profile = scratch.path().join("profile");
    std::fs::create_dir_all(&profile).expect("the profile dir");
    let ring_name = format!("/forge-veto-{}", std::process::id());
    let log = scratch.path().join("host.log");
    let (page_port, stop_page, page) = serve_page();
    let carried = format!("http://127.0.0.1:{page_port}/");
    let port = support::free_port();
    let mut host = support::Host::start(
        &exe,
        &profile,
        port,
        &ring_name,
        "800x600",
        &["--profile-id".to_owned(), "1".to_owned()],
        &log,
    );

    // **The endpoint must be the child's before the veto is installed**: a
    // host still starting has no page target, and under a loaded suite the
    // install raced it.
    forge_browser::spawn::wait_ready_for(host.pid(), port, &ring_name)
        .await
        .unwrap_or_else(|why| panic!("the host never came ready: {why} (log: {})", host.log()));

    let installed = veto::install(port, &profile).await;
    assert!(installed.is_ok(), "the veto installs (log: {}): {:?}", host.log(), installed.err());

    // The page a person would see: navigate the boot tab to the served page
    // behind the installed veto, then ask it for a passkey. A page whose
    // script was never reached would resolve, or throw a TypeError for a
    // missing API.
    let websocket = forge_browser::spawn::page_websocket(port).await.expect("the page's endpoint");
    let path = websocket
        .strip_prefix(&forge_browser::spawn::ws_prefix(port))
        .expect("the endpoint is on the host's port");
    let (mut cdp, _events) =
        Cdp::connect_watch(port, path, Backlog::EveryEvent).await.expect("a page connection");
    let navigated = cdp
        .call("Page.navigate", serde_json::json!({ "url": carried }), None)
        .await
        .expect("the page navigates");
    assert!(navigated.get("error").is_none(), "the served page is navigated to: {navigated}");
    // **The document must be the served one before anything is asked of it.**
    // An error page answers the credentials question with its own TypeError,
    // which is a different failure from the veto not having landed.
    let mut at = String::new();
    for _ in 0..50 {
        let reply = cdp
            .call(
                "Runtime.evaluate",
                serde_json::json!({ "expression": "location.href", "returnByValue": true }),
                None,
            )
            .await
            .expect("the page answers");
        at = reply
            .pointer("/result/value")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if at == carried {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(at, carried, "the served page is the document under test");
    // **The new document's context is patched as it is reported**, so the
    // evaluation is retried until that patch has landed rather than racing
    // it once.
    let mut answer = serde_json::Value::Null;
    for _ in 0..50 {
        answer = cdp
            .call(
                "Runtime.evaluate",
                serde_json::json!({
                    "expression": "navigator.credentials.get({ publicKey: { challenge: new Uint8Array([1]) } }) \
                                   .then(() => 'resolved', (why) => why.name)",
                    "awaitPromise": true,
                    "returnByValue": true,
                }),
                None,
            )
            .await
            .expect("the page answers");
        if answer.pointer("/result/value").and_then(serde_json::Value::as_str)
            == Some("NotAllowedError")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        answer.pointer("/result/value").and_then(serde_json::Value::as_str),
        Some("NotAllowedError"),
        "the page's passkey call is refused by the veto: {answer}"
    );
    // **The descriptor is the script's own**: the stock prototype's `get` is
    // configurable, and the veto freezes its replacement - so this says the
    // page was patched, not that a browser without a key happened to refuse.
    let descriptor = cdp
        .call(
            "Runtime.evaluate",
            serde_json::json!({
                "expression": "Object.getOwnPropertyDescriptor(CredentialsContainer.prototype, 'get')?.configurable",
                "returnByValue": true,
            }),
            None,
        )
        .await
        .expect("the page answers");
    assert_eq!(
        descriptor.pointer("/result/value"),
        Some(&serde_json::json!(false)),
        "the page's credentials container carries the frozen replacement: {descriptor}"
    );

    let status = host.terminate(Duration::from_secs(15));
    assert!(status.success(), "the host exited cleanly: {status} (log: {})", host.log());
    Ring::unlink(&ring_name);
    // Wake the fixture's blocking accept so its loop sees the flag and ends.
    stop_page.store(true, Ordering::Release);
    let _ = std::net::TcpStream::connect(("127.0.0.1", page_port));
    page.join().expect("the page server ends");
}

/// **The fixture answers every connection.** With a single `accept`, one
/// stray socket ahead of the document left it refused: the test then ran
/// against `chrome-error://chromewebdata/`, where `navigator.credentials` is
/// undefined, and failed with a TypeError instead of the veto's answer (CI
/// run 40311192668).
#[test]
fn the_page_fixture_answers_more_than_one_connection() {
    let (port, stop, page) = serve_page();
    for _ in 0..3 {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("a connection");
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: probe\r\nConnection: close\r\n\r\n")
            .expect("the request is written");
        let mut body = String::new();
        std::io::Read::read_to_string(&mut stream, &mut body).expect("the answer is read");
        assert!(body.contains("veto"), "every connection is served: {body}");
    }
    stop.store(true, Ordering::Release);
    let _ = std::net::TcpStream::connect(("127.0.0.1", port));
    page.join().expect("the page server ends");
}
