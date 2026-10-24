//! The host's pointer input, live: the gestures the server writes on the
//! host's stdin reach the page, and a select's popup is clickable through
//! them.
//!
//! **Both halves of the routing are pinned here.** A page click must behave
//! exactly as it did over DevTools - the same coordinates, the same click -
//! and a click on a row of a visible popup must set the select's value. The
//! bug being pinned is a menu that rendered into the frames but dismissed
//! when clicked, because the click went to the page target instead of the
//! host's own mouse call, the only path CEF routes into a popup.
//!
//! **These need the real bundle**: they stage the workspace's own host the
//! way the server ships it, then drive it as the server does.

#![cfg(target_os = "macos")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
#![allow(unsafe_code, reason = "a test ends its own child with a signal")]

mod support;

use std::time::{Duration, Instant};

use forge_browser::input::{HostInput, host_line};
use forge_browser::osr::Ring;
use forge_browser::window::Cdp;
use forge_primitives::browser::{InputModifiers, MouseAction};
use serde_json::{Value, json};

/// How long CEF is given to boot, paint and answer a gesture.
const BOUND: Duration = Duration::from_secs(45);
const POLL_EVERY: Duration = Duration::from_millis(50);

/// A staged host, its page's DevTools connection, and the scratch they live in.
struct Fixture {
    _scratch: tempfile::TempDir,
    host: support::Host,
    cdp: Cdp,
    _events: tokio::sync::mpsc::UnboundedReceiver<Value>,
    ring: String,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        Ring::unlink(&self.ring);
    }
}

async fn fixture(name: &str) -> Fixture {
    let scratch = tempfile::tempdir().expect("a scratch dir");
    let exe = support::host_executable(scratch.path());
    let profile = scratch.path().join("profile");
    let port = support::free_port();
    // POSIX shared-memory names are short; the name and pid keep two tests
    // in one process apart.
    let ring = format!("/forge-in-{}-{name}", std::process::id());
    let log = scratch.path().join("host.log");
    let host = support::Host::start(&exe, &profile, port, &ring, "800x600", &[], &log);
    let id = support::wait_for_page(port, &log, BOUND);
    let (cdp, events) = Cdp::connect_watch(port, &format!("/devtools/page/{id}"))
        .await
        .expect("the page's devtools endpoint accepts");
    Fixture { _scratch: scratch, host, cdp, _events: events, ring }
}

/// One expression's value, as the page evaluates it.
async fn evaluate(fixture: &mut Fixture, expression: &str) -> Value {
    let reply = fixture
        .cdp
        .call("Runtime.evaluate", json!({ "expression": expression, "returnByValue": true }), None)
        .await
        .expect("the page evaluates");
    reply.pointer("/result/value").cloned().unwrap_or(Value::Null)
}

/// Wait until the page answers `ok`, failing with what it said last.
async fn wait_for(fixture: &mut Fixture, expression: &str, ok: impl Fn(&Value) -> bool) {
    let deadline = Instant::now() + BOUND;
    loop {
        let value = evaluate(fixture, expression).await;
        if ok(&value) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the page never answered {expression} as expected; it says {value}"
        );
        tokio::time::sleep(POLL_EVERY).await;
    }
}

/// One full click on the page, written the way the server writes it.
fn click(fixture: &mut Fixture, x: i32, y: i32) {
    for action in [MouseAction::Press, MouseAction::Release] {
        fixture.host.input(&host_line(&HostInput::Mouse {
            action,
            x,
            y,
            button: 0,
            clicks: 1,
            modifiers: InputModifiers::default(),
        }));
    }
}

/// **A normal page click still lands.** The pointer path moved from the page
/// target to the host's own mouse calls; the coordinates and the click are
/// the same, so a button at (100,100)-(300,180) is hit by a click at its
/// middle and not by one away from it.
#[tokio::test(flavor = "multi_thread")]
async fn a_click_on_the_page_still_lands() {
    let mut at = fixture("click").await;
    evaluate(
        &mut at,
        "document.documentElement.style.margin='0'; document.body.style.margin='0'; \
         document.body.innerHTML = '<button id=\"b\" style=\"position:absolute;left:100px;\
         top:100px;width:200px;height:80px\" onclick=\"window.__clicks=\
         (window.__clicks||0)+1\">go</button>'; window.__clicks=0; 0",
    )
    .await;

    // The control: a click on the blank page is not the button's. The wait
    // gives a wrongly-routed click the time to land before it is asserted.
    click(&mut at, 600, 500);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(evaluate(&mut at, "window.__clicks").await, json!(0), "a click off the button");

    click(&mut at, 150, 130);
    wait_for(&mut at, "window.__clicks", |value| *value == json!(1)).await;
}

/// **A click on a visible popup row sets the select's value.** This is the
/// bug the host mouse path exists for: the menu was drawn into the frames
/// and every click on it closed it without choosing.
#[tokio::test(flavor = "multi_thread")]
async fn a_click_on_a_popup_row_sets_the_select() {
    let mut at = fixture("popup").await;
    evaluate(
        &mut at,
        "document.documentElement.style.margin='0'; document.body.style.margin='0'; \
         document.body.innerHTML = '<select id=\"s\" style=\"position:absolute;left:20px;\
         top:20px;width:200px;height:24px\"><option>one</option><option>two</option>\
         <option>three</option></select>'; 0",
    )
    .await;

    // **The window's first click focuses it rather than opening a menu**, so
    // the fixture takes that click on the blank page before the select's.
    // Activation can swallow more than one under load, so the priming click
    // is repeated until the page has actually seen one.
    evaluate(
        &mut at,
        "window.__downs = 0; document.addEventListener('mousedown', () => \
         window.__downs = (window.__downs || 0) + 1, true); 0",
    )
    .await;
    let deadline = Instant::now() + BOUND;
    loop {
        click(&mut at, 400, 400);
        tokio::time::sleep(Duration::from_millis(250)).await;
        let downs = evaluate(&mut at, "window.__downs || 0").await;
        if downs.as_u64().is_some_and(|downs| downs > 0) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no priming click reached the page (log: {})",
            at.host.log(),
        );
    }

    // The select sits at (20,20) 200x24: the menu opens under it at y=44,
    // with CEF's own 20-pixel rows. The second row's middle is (60,74).
    click(&mut at, 30, 30);
    wait_for(&mut at, "document.querySelector('select').matches(':open')", |value| {
        *value == json!(true)
    })
    .await;
    click(&mut at, 60, 74);
    wait_for(&mut at, "document.querySelector('select').selectedIndex", |value| {
        value.as_u64().is_some_and(|index| index >= 1)
    })
    .await;
    let value = evaluate(&mut at, "document.querySelector('select').value").await;
    assert_ne!(value, json!("one"), "the row click chose an option, not closed the menu");
}
