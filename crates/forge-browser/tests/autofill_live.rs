//! A submitted credit-card form does not kill the host.
//!
//! **The property: a page's card-form submission leaves the host alive.**
//! CEF's alloy runtime has no tab model, and Chrome's save-card bubble assumes
//! one: `SaveCardBubbleControllerImpl::GetAutofillBubbleHandler()` asks
//! `tabs::TabInterface::GetFromContents` for the tab behind the WebContents,
//! that lookup is empty in this host, and the dereference faults on
//! CrBrowserMain. CEF's own patch (chromiumembedded/cef#4075) guards a null
//! browser but not this tab, so the host turns the autofill prefs off instead -
//! the workaround CEF names for the crash class - and this test holds it.
//!
//! **These need the real bundle**: they stage the workspace's own host the way
//! the server ships it, then drive it as the server does.

#![cfg(target_os = "macos")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod support;

use std::io::{Read as _, Write as _};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use forge_browser::window::Cdp;
use serde_json::{Value, json};

/// How long CEF is given to boot, paint and answer.
const BOUND: Duration = Duration::from_secs(60);
const POLL_EVERY: Duration = Duration::from_millis(50);
/// How long the form is given to submit after the click.
const SUBMIT_BOUND: Duration = Duration::from_secs(20);
/// How long the host is watched for after the submission reached the server.
const SETTLE: Duration = Duration::from_secs(3);

/// A payment form the autofill importer recognises as a credit card.
fn payment_page() -> String {
    "<!doctype html><meta charset=utf-8><title>pay</title>\
     <form id=pay method=get action=\"/submitted\">\
     <p><label for=cc>Card number</label>\
     <input id=cc name=cardnumber autocomplete=cc-number></p>\
     <p><label for=name>Name on card</label>\
     <input id=name name=ccname autocomplete=cc-name></p>\
     <p><label for=exp>Expiry</label>\
     <input id=exp name=exp-date autocomplete=cc-exp></p>\
     <p><label for=cvc>Security code</label>\
     <input id=cvc name=cvc autocomplete=cc-csc></p>\
     <p><button id=submit type=submit>Pay</button></p>\
     </form>\
     <script>\
     window.__events=[];\
     addEventListener('mousedown',function(e){__events.push('down:'+e.clientX+','+e.clientY);},true);\
     addEventListener('click',function(e){__events.push('click:'+(e.target&&e.target.id));},true);\
     addEventListener('submit',function(){__events.push('submit');},true);\
     </script>"
        .to_owned()
}

/// A page server for one test: `/pay` is the form, every other path is the
/// submitted page, and every request path is recorded.
struct Page {
    url: String,
    hits: Arc<Mutex<Vec<String>>>,
    _thread: std::thread::JoinHandle<()>,
}

impl Page {
    fn serve(page: String) -> Self {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("a page port");
        let port = listener.local_addr().expect("the page's address").port();
        let hits = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&hits);
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut request = Vec::new();
                let mut chunk = [0_u8; 1024];
                loop {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            request.extend_from_slice(&chunk[..read]);
                            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                                break;
                            }
                        }
                    }
                }
                let request = String::from_utf8_lossy(&request);
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_owned();
                recorded.lock().expect("the page's hits").push(path.clone());
                let body = if path.starts_with("/pay") { page.clone() } else { "ok".to_owned() };
                let answer = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(answer.as_bytes());
                let _ = stream.flush();
            }
        });
        Self { url: format!("http://127.0.0.1:{port}/pay"), hits, _thread: thread }
    }

    fn requested(&self, path: &str) -> bool {
        self.hits.lock().expect("the page's hits").iter().any(|hit| hit.starts_with(path))
    }
}

async fn try_evaluate(cdp: &mut Cdp, expression: &str) -> Option<Value> {
    let reply = cdp
        .call("Runtime.evaluate", json!({ "expression": expression, "returnByValue": true }), None)
        .await
        .ok()?;
    reply.pointer("/result/value").cloned()
}

async fn evaluate(cdp: &mut Cdp, expression: &str) -> Value {
    let reply = cdp
        .call("Runtime.evaluate", json!({ "expression": expression, "returnByValue": true }), None)
        .await
        .expect("the page evaluates");
    reply.pointer("/result/value").cloned().unwrap_or(Value::Null)
}

/// Put text into one field the way a person does, so the renderer reports it
/// as an edit rather than an autofilled value.
async fn type_into(cdp: &mut Cdp, field: &str, text: &str) {
    evaluate(cdp, &format!("document.querySelector('{field}').focus()")).await;
    cdp.call("Input.insertText", json!({ "text": text }), None)
        .await
        .expect("the field takes text");
}

/// The point the page lays the submit button's centre on, once the document
/// is fully parsed and a hit test at that point reaches the button.
///
/// **A page with the form is not yet a page that takes a click**: the inline
/// script runs mid-parse, so a click sent the moment `#cc` exists can land
/// before the document is interactive and never reach a listener.
async fn pay_point(cdp: &mut Cdp, host: &support::Host) -> (i32, i32) {
    let deadline = Instant::now() + BOUND;
    loop {
        let point = evaluate(
            cdp,
            "(() => { \
             if (document.readyState !== 'complete') return null; \
             const button = document.querySelector('#submit'); \
             if (!button) return null; \
             const r = button.getBoundingClientRect(); \
             const x = Math.round(r.x + r.width / 2); \
             const y = Math.round(r.y + r.height / 2); \
             const hit = document.elementFromPoint(x, y); \
             return hit === button || (hit !== null && button.contains(hit)) ? { x, y } : null; \
             })()",
        )
        .await;
        if let Some(x) = point.get("x").and_then(Value::as_i64)
            && let Some(y) = point.get("y").and_then(Value::as_i64)
        {
            return (i32::try_from(x).expect("a sane x"), i32::try_from(y).expect("a sane y"));
        }
        assert!(
            Instant::now() < deadline,
            "the submit button never became hittable (log: {})",
            host.log()
        );
        tokio::time::sleep(POLL_EVERY).await;
    }
}

/// One trusted press and release at `x`, `y`, the way a driver clicks.
async fn click(cdp: &mut Cdp, x: i32, y: i32) {
    for kind in ["mouseMoved", "mousePressed", "mouseReleased"] {
        cdp.call(
            "Input.dispatchMouseEvent",
            json!({ "type": kind, "x": x, "y": y, "button": "left", "clickCount": 1 }),
            None,
        )
        .await
        .expect("the click is dispatched");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_submitted_card_form_does_not_kill_the_host() {
    let scratch = tempfile::tempdir().expect("a scratch dir");
    let exe = support::host_executable(scratch.path());
    let profile = scratch.path().join("profile");
    let port = support::free_port();
    let ring = format!("/forge-autofill-{}", std::process::id());
    let log = scratch.path().join("host.log");
    let page = Page::serve(payment_page());

    let mut host = support::Host::start(&exe, &profile, port, &ring, "1440x900", &[], &log);
    let id = support::wait_for_page(port, &log, BOUND);
    let (mut cdp, _events) = Cdp::connect_watch(port, &format!("/devtools/page/{id}"))
        .await
        .expect("the page's devtools endpoint accepts");
    cdp.call("Page.navigate", json!({ "url": page.url }), None).await.expect("the page navigates");

    // **The form has to be there before a click can mean it.**
    let deadline = Instant::now() + BOUND;
    loop {
        let ready = evaluate(&mut cdp, "!!document.querySelector('#cc')").await;
        if ready.as_bool() == Some(true) {
            break;
        }
        assert!(Instant::now() < deadline, "the form never loaded (log: {})", host.log());
        tokio::time::sleep(POLL_EVERY).await;
    }

    type_into(&mut cdp, "#cc", "4111111111111111").await;
    type_into(&mut cdp, "#name", "Test Person").await;
    type_into(&mut cdp, "#exp", "12/2030").await;
    type_into(&mut cdp, "#cvc", "123").await;
    let (x, y) = pay_point(&mut cdp, &host).await;

    // **A click the page never saw is retried**: a loaded machine can lose one
    // input, so a click only counts once the page records an event of its own,
    // and three is the cap on a button that is there but never submits.
    let deadline = Instant::now() + SUBMIT_BOUND;
    let mut clicks = 0_u8;
    loop {
        if page.requested("/submitted") || host.try_wait().is_some() || Instant::now() >= deadline {
            break;
        }
        let landed = try_evaluate(&mut cdp, "window.__events")
            .await
            .and_then(|events| events.as_array().map(Vec::is_empty))
            .is_some_and(|empty| !empty);
        if !landed && clicks < 3 {
            click(&mut cdp, x, y).await;
            clicks = clicks.saturating_add(1);
        }
        tokio::time::sleep(POLL_EVERY).await;
    }
    let events = try_evaluate(&mut cdp, "window.__events").await.unwrap_or(Value::Null);
    if let Some(status) = host.try_wait() {
        panic!(
            "the host died on a card-form submission (status {status:?}, page events {events}): {}",
            host.log()
        );
    }
    // **The crash's task is posted behind the submission**, so the host is
    // watched for a moment after the request rather than judged on the
    // instant it lands.
    let deadline = Instant::now() + SETTLE;
    while Instant::now() < deadline {
        if let Some(status) = host.try_wait() {
            panic!(
                "the host died on a card-form submission (status {status:?}, page events \
                 {events}): {}",
                host.log()
            );
        }
        tokio::time::sleep(POLL_EVERY).await;
    }
    assert!(
        page.requested("/submitted"),
        "the form never submitted, so the repro never reached autofill (page events {events}, \
         log: {})",
        host.log(),
    );
    // **The fix's own evidence**: the crash path is gone because the host
    // turned it off, not because the form missed autofill's heuristics.
    assert!(
        host.log().contains("autofill is off (event_name cef_autofill_off)"),
        "the host never turned autofill off, so the crash path is still reachable (log: {})",
        host.log(),
    );
    println!("the card form submitted and the host stayed up");
}
