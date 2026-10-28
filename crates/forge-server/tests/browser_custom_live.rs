//! The browser calls the host answers itself, live: the two tools upstream
//! does not have and the two arguments it does not take, driven through the
//! real service against a real page.
//!
//! **A live browser, not a mock.** The test drives `BrowserService`, which
//! spawns the staged host bundle and the vendored Playwright driver, so both
//! must be in place: `scripts/stage-browser-host.sh` and
//! `scripts/vendor_browser_stack.sh` (which installs to
//! `FORGE_BROWSER_STACK_DIR`); the stack is read from
//! **`FORGE_BROWSER_TEST_STACK`**, exactly as `browser_service.rs` reads it.

#![cfg(target_os = "macos")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::io::{Read as _, Write as _};
use std::path::PathBuf;
use std::time::Duration;

use forge_browser::service::BrowserService;
use forge_primitives::SessionSlot;
use forge_primitives::browser::BrowserPart;
use serde_json::{Value, json};

/// The vendored driver stack: an override for a checkout under test, else the
/// installed one beside the server's profiles.
fn stack() -> PathBuf {
    match std::env::var("FORGE_BROWSER_TEST_STACK") {
        Ok(stack) => PathBuf::from(stack),
        Err(_) => forge_browser::host_bundle::app_support()
            .expect("the app-support directory")
            .join("browser/stack"),
    }
}

fn seat() -> SessionSlot {
    SessionSlot::lead("TestOrg", "custom")
}

/// A service on a scratch root, with the vendored stack checked first so a
/// machine without it fails by name rather than inside the driver.
fn service() -> (tempfile::TempDir, std::sync::Arc<BrowserService>) {
    let stack = stack();
    assert!(
        stack.join("node/bin/node").is_file(),
        "the vendored driver stack is not installed at {} - run \
         scripts/vendor_browser_stack.sh with FORGE_BROWSER_STACK_DIR and point \
         FORGE_BROWSER_TEST_STACK at the directory it installs",
        stack.display()
    );
    let scratch = tempfile::tempdir().expect("a scratch root");
    let service = BrowserService::at(scratch.path().to_path_buf(), stack);
    (scratch, service)
}

/// The answer's text parts, joined: a snippet's result is text.
fn text(parts: &[BrowserPart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            BrowserPart::Text { text } => Some(text.as_str()),
            BrowserPart::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The first JSON value the driver answered with, out of its `### Result`
/// section. A plain evaluate's answer is the value itself; a snippet's is the
/// JSON-encoded string it returned.
fn driver_result(answer: &str) -> Value {
    let body = answer.split("### Result").nth(1).unwrap_or(answer).trim_start();
    let mut values = serde_json::Deserializer::from_str(body).into_iter::<Value>();
    values
        .next()
        .unwrap_or_else(|| panic!("the driver answered with a result: {answer}"))
        .unwrap_or_else(|why| panic!("the result decodes ({why}): {answer}"))
}

/// The JSON a snippet returned, decoded out of its result string.
fn snippet_value(answer: &str) -> Value {
    let Value::String(inner) = driver_result(answer) else {
        panic!("a snippet returns a JSON string: {answer}");
    };
    serde_json::from_str(&inner)
        .unwrap_or_else(|why| panic!("the snippet returned JSON ({why}): {inner}"))
}

/// A page read's number, out of the driver's result wrapper.
fn result_number(answer: &str) -> u64 {
    driver_result(answer)
        .as_u64()
        .unwrap_or_else(|| panic!("the page's answer is a number: {answer}"))
}

/// The snapshot's ref for the button the page names.
///
/// **The `[ref=` is part of the match**: a `data:` page's URL quotes its own
/// HTML, so "button" and the name appear in the URL line too.
fn ref_for_button(snapshot: &str, name: &str) -> String {
    let line = snapshot
        .lines()
        .find(|line| line.contains("[ref=") && line.contains("button") && line.contains(name))
        .unwrap_or_else(|| panic!("the snapshot carries the {name} button: {snapshot}"));
    let start = line.find("[ref=").unwrap_or_else(|| panic!("the button carries a ref: {line}"))
        + "[ref=".len();
    let rest = &line[start..];
    let end = rest.find(']').unwrap_or_else(|| panic!("the ref closes: {line}"));
    rest[..end].to_owned()
}

/// A page whose form carries one of each shape the tool has to read back.
fn form_page() -> String {
    let html = "<!doctype html><meta charset=utf-8>\
        <form id=f>\
        <input name=first value=Ada>\
        <input name=email value=nope aria-invalid=true>\
        <input name=zip required>\
        <select name=country><option value=US>United States</option>\
        <option value=IN selected>India</option></select>\
        <input type=checkbox name=terms checked>\
        </form>\
        <div id=picker role=combobox>Canada</div>";
    format!("data:text/html;charset=utf-8,{}", percent_encode(html))
}

/// A page whose button fires a request, and one that fires nothing.
///
/// Served over the same loopback origin as the capture endpoint, because a
/// `data:` page has an opaque origin and the fetch it makes is not the same
/// request a real page would send.
fn capture_page() -> String {
    "<!doctype html><meta charset=utf-8>\
     <button id=chatty onclick=\"fetch('/capture')\">go</button>\
     <button id=quiet>quiet</button>"
        .to_owned()
}

fn percent_encode(text: &str) -> String {
    text.chars()
        .map(|character| match character {
            'a'..='z'
            | 'A'..='Z'
            | '0'..='9'
            | '-'
            | '_'
            | '.'
            | '!'
            | '~'
            | '*'
            | '\''
            | '('
            | ')'
            | ';'
            | ':'
            | '@'
            | '&'
            | '='
            | '+'
            | '$'
            | ','
            | '/'
            | '?' => character.to_string(),
            other => format!("%{:02X}", u32::from(other)),
        })
        .collect()
}

/// A loopback server for the capture test: the page and one endpoint that
/// answers a JSON body, so the click's request has a real status and body.
fn serve_capture() -> (u16, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("a loopback port");
    let port = listener.local_addr().expect("the bound address").port();
    let handle = std::thread::spawn(move || {
        // Two requests are expected: the page, then the click's capture.
        for _ in 0..8 {
            let Ok((mut stream, _peer)) = listener.accept() else { return };
            let mut request = [0u8; 2048];
            let read = stream.read(&mut request).unwrap_or(0);
            let first =
                String::from_utf8_lossy(&request[..read]).lines().next().unwrap_or("").to_owned();
            let path = first.split_whitespace().nth(1).unwrap_or("/").to_owned();
            let (status, mime, body) = if path == "/capture" {
                ("200 OK", "application/json", "{\"ok\":true}".to_owned())
            } else if path == "/favicon.ico" {
                ("204 No Content", "text/plain", String::new())
            } else {
                ("200 OK", "text/html; charset=utf-8", capture_page())
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    (port, handle)
}

/// **`browser_form_state` reads the page's own state**: every field's value,
/// the invalid flag, and the text of a control whose committed value lives
/// outside its input.
#[ignore = "needs the built host bundle and the vendored driver stack; run with --run-ignored ignored-only"]
#[tokio::test(flavor = "multi_thread")]
async fn a_forms_state_is_read_from_the_page() {
    let (_scratch, service) = service();
    let seat = seat();
    service
        .call(&seat, "browser_navigate", json!({ "url": form_page() }))
        .await
        .expect("the page loads");
    // **The tool reports the CURRENT state**, so the fixture moves after the
    // load: a value a page script typed, a select moved off its initial
    // option, and a `required` empty field that is `:invalid` with no aria
    // flag at all. A read of the attributes instead of the live properties
    // would still say Ada, IN, and nothing.
    service
        .call(
            &seat,
            "browser_evaluate",
            json!({ "function": "() => { const f = document.getElementById('f'); \
                f.querySelector('[name=first]').value = 'Grace'; \
                f.querySelector('[name=country]').value = 'US'; return 'edited'; }" }),
        )
        .await
        .expect("the page is edited after it loads");

    let answer = service
        .call(&seat, "browser_form_state", json!({}))
        .await
        .expect("the form's state is read");
    let state = snippet_value(&text(&answer));
    let form = &state["roots"][0];
    let fields = form["fields"].as_array().expect("the form has fields");
    let field = |name: &str| {
        fields
            .iter()
            .find(|field| field["name"] == name)
            .unwrap_or_else(|| panic!("{name} is among the form's fields: {form}"))
            .clone()
    };
    assert_eq!(
        field("first")["value"],
        "Grace",
        "a typed value is read, not the attribute: {form}"
    );
    assert_eq!(field("country")["value"], "US", "a moved select reads its current option: {form}");
    assert_eq!(field("terms")["checked"], true, "a checkbox's state is read: {form}");
    assert_eq!(field("email")["invalid"], true, "an aria-invalid field is flagged: {form}");
    assert_eq!(
        field("zip")["invalid"],
        true,
        "and a `:invalid` required field is flagged without an aria one: {form}",
    );

    // A picker's committed value lives in the control's text, not an input.
    let picker = service
        .call(&seat, "browser_form_state", json!({ "target": "#picker" }))
        .await
        .expect("a picker's state is read");
    let scoped = snippet_value(&text(&picker));
    let roots = scoped["roots"].as_array().expect("the read is a list of roots");
    assert_eq!(roots.len(), 1, "a scoped read returns the one control, not the page: {scoped}");
    assert_eq!(roots[0]["text"], "Canada", "the control's own text is the value");
}

/// **`browser_click_and_capture` answers with the requests the click fired**,
/// and a click that fired none answers with an empty list.
#[ignore = "needs the built host bundle and the vendored driver stack; run with --run-ignored ignored-only"]
#[tokio::test(flavor = "multi_thread")]
async fn a_clicks_requests_are_captured() {
    let (port, _server) = serve_capture();
    let (_scratch, service) = service();
    let seat = seat();
    service
        .call(&seat, "browser_navigate", json!({ "url": format!("http://127.0.0.1:{port}/") }))
        .await
        .expect("the page loads");

    let answer = service
        .call(&seat, "browser_click_and_capture", json!({ "target": "#chatty" }))
        .await
        .expect("the click runs");
    let seen = snippet_value(&text(&answer));
    let responses = seen.as_array().expect("the capture is a list of responses");
    let captured = responses
        .iter()
        .find(|response| response["url"].as_str().is_some_and(|url| url.ends_with("/capture")))
        .unwrap_or_else(|| panic!("the click's request is captured: {seen}"));
    assert_eq!(captured["status"], 200, "its status is the server's: {seen}");
    assert!(
        captured["body"].as_str().is_some_and(|body| body.contains("ok")),
        "and its body is read: {seen}",
    );

    let quiet = service
        .call(&seat, "browser_click_and_capture", json!({ "target": "#quiet" }))
        .await
        .expect("the quiet click runs");
    assert_eq!(
        snippet_value(&text(&quiet)),
        json!([]),
        "a click that fired nothing captures nothing",
    );
}

/// **`force` clicks what the actionability gate would refuse**: a button that
/// never stops moving does not take a plain click and does take a forced one.
///
/// **The failure `force` exists for is an unstable element, not a covered
/// one.** A forced click is still a real mouse event at the element's current
/// box, so an overlay would take the click instead; an animation that never
/// holds still keeps the gate waiting while the element itself stays the
/// hit target.
#[ignore = "needs the built host bundle and the vendored driver stack; run with --run-ignored ignored-only"]
#[tokio::test(flavor = "multi_thread")]
async fn a_forced_click_lands_where_a_plain_one_cannot() {
    let (_scratch, service) = service();
    let seat = seat();
    let page = "<!doctype html><meta charset=utf-8><style>\
        body{margin:0}#target{position:absolute;left:40px;top:40px;width:120px;height:40px;\
        animation:drift 1s infinite alternate}\
        @keyframes drift{from{transform:translateX(0)}to{transform:translateX(24px)}}</style>\
        <button id=target>drifting</button>\
        <script>window.__clicks=0;\
        document.getElementById('target').addEventListener('mousedown',function(){\
        window.__clicks+=1;});</script>";
    service
        .call(
            &seat,
            "browser_navigate",
            json!({ "url": format!("data:text/html;charset=utf-8,{}", percent_encode(page)) }),
        )
        .await
        .expect("the page loads");

    // The plain click waits for the button to hold still and never gets it:
    // the driver's own refusal, inside its call bound.
    let plain = tokio::time::timeout(
        Duration::from_secs(90),
        service.call(&seat, "browser_click", json!({ "target": "#target" })),
    )
    .await
    .expect("the plain click answers inside the driver's bound");
    let refused = plain.expect_err("a drifting button refuses a plain click");
    assert!(
        refused.to_lowercase().contains("stable"),
        "the refusal is the actionability gate's, not a schema or unknown-tool error: {refused}",
    );
    let clicks = service
        .call(&seat, "browser_evaluate", json!({ "function": "() => window.__clicks" }))
        .await
        .expect("the page reports its clicks");
    assert_eq!(result_number(&text(&clicks)), 0, "and the refusal landed no click");

    // The forced click skips the gate and lands on the element the selector
    // names.
    let forced = service
        .call(&seat, "browser_click", json!({ "target": "#target", "force": true }))
        .await
        .expect("the forced click runs");
    assert!(text(&forced).contains("clicked"), "the snippet answers: {}", text(&forced));
    let clicks = service
        .call(&seat, "browser_evaluate", json!({ "function": "() => window.__clicks" }))
        .await
        .expect("the page reports its clicks");
    assert_eq!(result_number(&text(&clicks)), 1, "and the forced click landed");
}

/// **`expression` waits on a condition no text can state.**
#[ignore = "needs the built host bundle and the vendored driver stack; run with --run-ignored ignored-only"]
#[tokio::test(flavor = "multi_thread")]
async fn a_wait_on_an_expression_returns_when_the_page_gets_there() {
    let (_scratch, service) = service();
    let seat = seat();
    let page = "<!doctype html><meta charset=utf-8><div id=rows></div>\
        <script>setInterval(function(){var d=document.createElement('div');\
        d.className='row';d.textContent='r';document.body.appendChild(d);},100);</script>";
    service
        .call(
            &seat,
            "browser_navigate",
            json!({ "url": format!("data:text/html;charset=utf-8,{}", percent_encode(page)) }),
        )
        .await
        .expect("the page loads");

    service
        .call(
            &seat,
            "browser_wait_for",
            json!({ "expression": "document.querySelectorAll('.row').length > 3" }),
        )
        .await
        .expect("the condition is reached");
    // **The page is the evidence, not the snippet's own answer**: a snippet
    // that returned its literal without waiting would still read as success.
    let rows = service
        .call(
            &seat,
            "browser_evaluate",
            json!({ "function": "() => document.querySelectorAll('.row').length" }),
        )
        .await
        .expect("the page reports its rows");
    assert!(
        result_number(&text(&rows)) > 3,
        "the wait returned because the page got there: {}",
        text(&rows),
    );
}

/// **A snapshot's ref is a target, not a string.** These tools lead with the
/// ref a `browser_snapshot` hands the model, so that ref has to reach the
/// driver's own `aria-ref` engine mid-flow; every other live case here drives
/// a CSS selector, which would pass whether or not the engine resolves.
#[ignore = "needs the built host bundle and the vendored driver stack; run with --run-ignored ignored-only"]
#[tokio::test(flavor = "multi_thread")]
async fn a_snapshot_ref_lands_a_forced_click() {
    let (_scratch, service) = service();
    let seat = seat();
    let page = "<!doctype html><meta charset=utf-8>\
        <button id=refprobe>probe</button>\
        <script>window.__clicks=0;\
        document.getElementById('refprobe').addEventListener('mousedown',function(){\
        window.__clicks+=1;});</script>";
    service
        .call(
            &seat,
            "browser_navigate",
            json!({ "url": format!("data:text/html;charset=utf-8,{}", percent_encode(page)) }),
        )
        .await
        .expect("the page loads");

    let snapshot =
        service.call(&seat, "browser_snapshot", json!({})).await.expect("the page is captured");
    let reference = ref_for_button(&text(&snapshot), "probe");

    service
        .call(&seat, "browser_click", json!({ "target": reference, "force": true }))
        .await
        .expect("the ref clicks");
    let clicks = service
        .call(&seat, "browser_evaluate", json!({ "function": "() => window.__clicks" }))
        .await
        .expect("the page reports its clicks");
    assert_eq!(
        result_number(&text(&clicks)),
        1,
        "the forced click on the snapshot's ref ({reference}) landed",
    );
}
