//! The browser host against the vendored stack: launch, drive, answer.
//!
//! **Deliberately ignored.** It needs `just vendor-browser-stack` to have
//! run - half a gigabyte of browser, node and driver - so it cannot be part
//! of a gate every checkout runs. Run it where the stack is:
//!
//! ```text
//! cargo nextest run --manifest-path client/src-tauri/Cargo.toml \
//!     --run-ignored ignored-only -E 'test(a_session_drives_a_page)'
//! ```
//!
//! What it proves is the whole client half of the pipe in one go: the
//! vendored Chromium launches against a profile nobody has used, the vendored
//! driver attaches to it over CDP, and upstream's own `browser_navigate` and
//! `browser_snapshot` answer through the host - which is exactly what an ask
//! from a session rides.

use std::path::PathBuf;
use std::process::Command;

use forge_client::browser::contexts::Seat;
use forge_client::browser::{BrowserHost, StackPaths};
use serde_json::json;

/// The seat these calls are made for: one session, as an ask carries it.
fn seat() -> Seat {
    Seat {
        org: "Busytools".to_owned(),
        project: "forge".to_owned(),
        label: "browser-live".to_owned(),
    }
}

/// Kill the browser this test launched, by the PORT it holds - never by a
/// pattern: this machine runs other browsers, and one of them belongs to the
/// person sitting at it.
fn kill_the_browser_on(port: u16) {
    let Ok(listed) =
        Command::new("lsof").args(["-t", &format!("-iTCP:{port}"), "-sTCP:LISTEN"]).output()
    else {
        return;
    };
    let pids = String::from_utf8_lossy(&listed.stdout);
    for pid in pids.split_whitespace() {
        let _ = Command::new("kill").arg(pid).status();
    }
}

#[tokio::test]
#[ignore = "drives the vendored stack; needs `just vendor-browser-stack`"]
async fn a_session_drives_a_page() {
    let stack = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("browser-stack");
    assert!(
        stack.join("node/bin/node").is_file(),
        "the vendored stack is not there - run `just vendor-browser-stack`",
    );
    let dir = tempfile::tempdir().expect("a temp dir");
    let paths = StackPaths {
        stack,
        profile: dir.path().join("profile"),
        output: dir.path().join("output"),
        contexts: dir.path().join("contexts"),
    };
    let host = BrowserHost::new(paths.clone());

    // **The app's own start**: the browser is up before any call asks for it,
    // and a second start ATTACHES to that launch rather than making another
    // one - one browser for the app, however many times it is ensured.
    host.start().await.expect("the browser comes up with the app");
    let launched = forge_client::browser::chromium::read_active_port(&paths.profile)
        .expect("the start wrote its port file");
    host.start().await.expect("a second start attaches");
    assert_eq!(
        forge_client::browser::chromium::read_active_port(&paths.profile).map(|active| active.port),
        Some(launched.port),
        "the second start attached to the launch that was already there",
    );

    let page = "data:text/html,<h1>forge browser host</h1>";
    let navigated = host.call(&seat(), "browser_navigate", json!({ "url": page })).await;
    let port = forge_client::browser::chromium::read_active_port(&paths.profile)
        .expect("the launch wrote its port file")
        .port;
    let parts = match navigated {
        Ok(parts) => parts,
        Err(why) => {
            kill_the_browser_on(port);
            panic!("the host could not navigate: {why}");
        }
    };
    assert!(!parts.is_empty(), "a navigate answers with something");

    let snapshot = match host.call(&seat(), "browser_snapshot", json!({})).await {
        Ok(parts) => parts,
        Err(why) => {
            kill_the_browser_on(port);
            panic!("the host could not snapshot: {why}");
        }
    };
    let text = text_of(&snapshot);
    kill_the_browser_on(port);

    assert!(
        text.contains("forge browser host"),
        "the snapshot is of the page that was navigated to: {text}",
    );
}

/// The text one answer carries, which is every part that is not an image.
fn text_of(parts: &[forge_client::browser::driver::ReplyPart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            forge_client::browser::driver::ReplyPart::Text { text } => Some(text.clone()),
            forge_client::browser::driver::ReplyPart::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The ref a snapshot gave an element whose line contains `needle`.
///
/// Both halves matter: the page's own URL line carries whatever the markup
/// says, so a line is a snapshot row only when it names a ref as well.
fn ref_of(snapshot: &str, needle: &str) -> String {
    snapshot
        .lines()
        .find(|line| line.contains("ref=") && line.contains(needle))
        .and_then(|line| {
            let at = line.find("ref=")? + 4;
            let rest = &line[at..];
            let end = rest.find(|c: char| !c.is_ascii_alphanumeric()).unwrap_or(rest.len());
            Some(rest[..end].to_owned())
        })
        .unwrap_or_else(|| {
            panic!("the snapshot names no element containing {needle:?}: {snapshot}")
        })
}

/// **The additions beyond upstream's surface, driven against a real page.**
/// A forced click, an expression wait, a click whose responses come back with
/// it, and a form's state - each a snippet this host composes, each proven
/// here rather than only asserted as a string.
#[tokio::test]
#[ignore = "drives the vendored stack; needs `just vendor-browser-stack`"]
async fn the_additions_drive_a_real_page() {
    // A page that fetches something: a listener of the test's own serving
    // BOTH halves, so the click's request is same-origin and its body
    // readable - a data: URL's fetch is cross-origin and dies at the CORS
    // layer, where no response is what comes back.
    const PAGE: &str = "<title>before</title>\
         <form><label>Email <input name=email required value=''></label>\
         <div aria-invalid=true><input name=picker value='committed'></div></form>\
         <button onclick=\"fetch('/x').then(() => {{document.title='fetched'}})\">Fetch</button>";
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let port = listener.local_addr().expect("the bound port").port();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        for _ in 0..6 {
            let Ok((mut socket, _)) = listener.accept().await else { continue };
            let mut buffer = [0_u8; 4096];
            let read = socket.read(&mut buffer).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..read]);
            let body = if request.starts_with("GET /x") { "fetched!\n" } else { PAGE };
            let answer = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len(),
            );
            let _ = socket.write_all(answer.as_bytes()).await;
        }
    });

    let stack = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("browser-stack");
    assert!(
        stack.join("node/bin/node").is_file(),
        "the vendored stack is not there - run `just vendor-browser-stack`",
    );
    let dir = tempfile::tempdir().expect("a temp dir");
    let paths = StackPaths {
        stack,
        profile: dir.path().join("profile"),
        output: dir.path().join("output"),
        contexts: dir.path().join("contexts"),
    };
    let host = BrowserHost::new(paths.clone());
    let browser_port = {
        host.start().await.expect("the browser comes up");
        forge_client::browser::chromium::read_active_port(&paths.profile)
            .expect("the start wrote its port file")
            .port
    };
    /// Out through the browser's port, so a panic never leaves a browser
    /// behind, and diverging so a match arm reads as an arm.
    fn finish(browser_port: u16, why: String) -> ! {
        kill_the_browser_on(browser_port);
        panic!("{why}");
    }

    let page = format!("http://127.0.0.1:{port}/");
    if let Err(why) = host.call(&seat(), "browser_navigate", json!({ "url": page })).await {
        finish(browser_port, format!("the page would not navigate: {why}"));
    }
    let snapshot = match host.call(&seat(), "browser_snapshot", json!({})).await {
        Ok(parts) => text_of(&parts),
        Err(why) => finish(browser_port, format!("the page would not snapshot: {why}")),
    };
    let button = ref_of(&snapshot, "Fetch");

    // A forced click: Playwright's own click with the actionability gate off.
    match host.call(&seat(), "browser_click", json!({ "target": button, "force": true })).await {
        Ok(parts) => assert!(text_of(&parts).contains("clicked"), "{:?}", text_of(&parts)),
        Err(why) => finish(browser_port, format!("a forced click did not run: {why}")),
    }

    // The click's responses come back with the click itself.
    let captured = match host
        .call(&seat(), "browser_click_and_capture", json!({ "target": button, "settleMs": 1200 }))
        .await
    {
        Ok(parts) => text_of(&parts),
        Err(why) => finish(browser_port, format!("click_and_capture did not run: {why}")),
    };
    // The driver's own report carries the snippet's JSON as an escaped
    // string, so the pieces are asserted, not a quoted shape.
    assert!(
        captured.contains("/x") && captured.contains("200") && captured.contains("fetched!"),
        "the click's response - url, status and body - is what came back: {captured}",
    );

    // An expression wait: a condition no text can state.
    match host
        .call(&seat(), "browser_wait_for", json!({ "expression": "document.title === 'fetched'" }))
        .await
    {
        Ok(parts) => assert!(text_of(&parts).contains("condition holds"), "{:?}", text_of(&parts)),
        Err(why) => finish(browser_port, format!("the expression wait did not run: {why}")),
    }

    // The form's state: the values, what is marked invalid, and the control's
    // own text.
    let form = match host.call(&seat(), "browser_form_state", json!({})).await {
        Ok(parts) => text_of(&parts),
        Err(why) => finish(browser_port, format!("form_state did not run: {why}")),
    };
    kill_the_browser_on(browser_port);
    // Escaped by the driver's own report, so the pieces are asserted.
    assert!(form.contains("email"), "the empty required input is read: {form}");
    assert!(form.contains("invalid") && form.contains("true"), "and it is marked invalid: {form}");
    assert!(
        form.contains("picker") && form.contains("committed"),
        "and a field's committed value is what comes back, wherever the control keeps it: {form}",
    );
}
