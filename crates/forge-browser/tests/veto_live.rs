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
use std::time::Duration;

use forge_browser::osr::Ring;
use forge_browser::veto::{self, Carried};
use forge_browser::window::Cdp;

/// One page served from loopback: a secure context, so `navigator.credentials`
/// exists and the veto's own answer is what the page sees.
fn serve_page() -> (u16, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("a loopback listener");
    let port = listener.local_addr().expect("the bound address").port();
    let thread = std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else { return };
        let mut request = [0_u8; 4096];
        let _ = stream.read(&mut request);
        let body = "<html><body>veto</body></html>";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes());
    });
    (port, thread)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_installed_veto_refuses_a_page_a_passkey_call() {
    let scratch = tempfile::tempdir().expect("a scratch dir");
    let exe = support::host_executable(scratch.path());
    let profile = scratch.path().join("profile");
    std::fs::create_dir_all(&profile).expect("the profile dir");
    let ring_name = format!("/forge-veto-{}", std::process::id());
    let log = scratch.path().join("host.log");
    let (page_port, page) = serve_page();
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
    forge_browser::spawn::wait_ready_for(host.pid(), port, &ring_name, &profile)
        .await
        .unwrap_or_else(|why| panic!("the host never came ready: {why} (log: {})", host.log()));

    let installed = veto::install(port, &profile, Some(carried)).await;
    assert!(
        matches!(installed, Ok(Carried::Opened)),
        "the veto installs and the carried page opens (log: {}): {:?}",
        host.log(),
        installed.err()
    );

    // The page a person would see: ask it for a passkey and read the answer
    // back. A page whose script was never reached would resolve, or throw a
    // TypeError for a missing API.
    let websocket = forge_browser::spawn::page_websocket(port).await.expect("the page's endpoint");
    let path = websocket
        .strip_prefix(&forge_browser::spawn::ws_prefix(port))
        .expect("the endpoint is on the host's port");
    let (mut cdp, _events) = Cdp::connect_watch(port, path).await.expect("a page connection");
    let answer = cdp
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
    page.join().expect("the page server ends");
}
