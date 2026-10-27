//! A page's `window.open` gets a real handle and a tab of its own.
//!
//! **The handle is the defect this pins.** A host that cancels CEF's popup and
//! opens a tab in its place leaves `window.open` answering null, and every
//! sign-in library reports that as "the pop-up was blocked" - a tab opened
//! beside it never helps, because the page has already failed. So the page's
//! own check of the handle is the assertion that matters here.
//!
//! The rest of the test holds the tab half down: the popup paints its own
//! stream, it is reported under the tab that opened it, and closing it takes
//! neither the opener nor the server's view of it.
//!
//! **These need the real bundle**: they stage the workspace's own host the
//! way the server ships it, then drive it as the server does.

#![cfg(target_os = "macos")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
#![allow(unsafe_code, reason = "a test reads the host's ring header")]

mod support;

use std::io::{Read as _, Write as _};
use std::time::{Duration, Instant};

use forge_browser::channel;
use forge_browser::control::{HostEvent, parse};
use forge_browser::frame::Frame;
use forge_browser::input::{HostInput, host_line};
use forge_browser::osr::Ring;
use forge_browser::window::Cdp;
use forge_primitives::browser::{InputModifiers, MouseAction};
use serde_json::{Value, json};

/// How long CEF is given to boot, paint and open a popup.
const BOUND: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(50);

/// **The page keeps the handle `window.open` answered**, the way a sign-in
/// library does, and writes its verdict into the title a test can read.
const PAGE: &str = "<!doctype html><meta charset=utf-8>\
<style>html,body{margin:0;padding:0}\
button{width:300px;height:120px;font-size:24px}</style>\
<button id=signin>Sign in with Google</button>\
<script>\
window.__clicks=[];\
addEventListener('mousedown',function(e){window.__clicks.push({x:e.clientX,y:e.clientY,target:e.target&&e.target.id});},true);\
document.getElementById('signin').onclick=function(){\
var w=window.open('about:blank','signin');\
window.__popup=w;\
document.title=w?'popup-open':'popup-blocked';\
};\
document.title='ready';\
</script>";

/// Serve `body` on a loopback port for the length of the test, and answer how
/// many requests the page took.
///
/// **A real origin, not a `data:` document**: the popup starts blank and the
/// page that opened it is an ordinary web page, which is the shape a sign-in
/// flow arrives in. `delay` is the page's own latency before its headers, so
/// a navigation can be watched while it is in flight.
fn serve_page(
    body: &str,
    delay: Duration,
) -> (u16, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("a loopback port");
    let port = listener.local_addr().expect("the bound address").port();
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = std::sync::Arc::clone(&hits);
    let body = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            // **One connection at a time stalls a preconnect.** A browser
            // opens a spare socket with no request on it, and a server that
            // reads every accepted stream in turn would wait on that socket
            // while the real request sat unread; each connection is served
            // on its own thread, and only a request line counts.
            let counted = std::sync::Arc::clone(&counted);
            let body = body.clone();
            std::thread::spawn(move || {
                let mut request = [0_u8; 4096];
                let Ok(read) = stream.read(&mut request) else { return };
                // **The document, not the favicon**: every document load also
                // asks for one, and counting it would halve the evidence.
                if read == 0 || !request.starts_with(b"GET / HTTP") {
                    return;
                }
                counted.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                std::thread::sleep(delay);
                let _ = stream.write_all(body.as_bytes());
                let _ = stream.flush();
            });
        }
    });
    (port, hits)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pages_popup_gets_a_handle_and_becomes_a_tab_of_its_own() {
    let scratch = tempfile::tempdir().expect("a temp dir");
    let exe = support::host_executable(scratch.path());
    let profile = scratch.path().join("profile");
    std::fs::create_dir_all(&profile).expect("the profile dir");
    let port = support::free_port();
    let ring_name = format!("/forge-popup-test-{}", std::process::id());
    let log = scratch.path().join("host.log");

    let (frames, painted) = std::sync::mpsc::channel();
    let socket = channel::socket_path(&profile);
    let _reader = channel::serve(&socket, move |message| {
        if let channel::ChannelMessage::Frame(frame) = message {
            let _ = frames.send(frame);
        }
    })
    .expect("the frame socket is served");

    let (page_port, _pages) = serve_page(PAGE, Duration::ZERO);
    let mut host =
        support::Host::start_with_events(&exe, &profile, port, &ring_name, "800x600", &[], &log);
    let opener_page = support::wait_for_page(port, &log, BOUND);
    let (mut cdp, _events) = connect(port, &format!("/devtools/page/{opener_page}"))
        .await
        .expect("the page's devtools endpoint accepts");

    // **A host with no viewer is paused.** The service unpauses the ring when
    // a viewer attaches, and a popup's frame exists only if this does the
    // same.
    let ring = wait_for_ring(&ring_name, &host);
    ring.pause(false);
    ring.key();

    cdp.call("Page.navigate", json!({ "url": format!("http://127.0.0.1:{page_port}/") }), None)
        .await
        .expect("the page navigates");
    wait_for_title(&mut cdp, &["ready"], BOUND).await;

    // **The gesture goes to a page that has painted.** A click sent the
    // instant the document's title reads back is dropped by the fresh
    // renderer (measured); the first frame after the navigation is the proof
    // the page is live, and it is what the server's own click follows.
    while painted.try_recv().is_ok() {}
    wait_for_frame(&painted, 1, BOUND).await;

    // **The gesture is the server's own**: a press and release written on the
    // host's input pipe, which is the path a person's click takes.
    for action in [MouseAction::Press, MouseAction::Release] {
        host.input(&host_line(&HostInput::Mouse {
            tab: 1,
            action,
            x: 60,
            y: 60,
            button: 0,
            clicks: 1,
            modifiers: InputModifiers::default(),
        }));
    }

    let deadline = Instant::now() + BOUND;
    let title = loop {
        let title = evaluate(&mut cdp, "document.title").await;
        let title = title.as_str().unwrap_or_default().to_owned();
        if title == "popup-open" || title == "popup-blocked" {
            break title;
        }
        if Instant::now() >= deadline {
            println!(
                "the click reached {:?}, at (60,60) is {:?}",
                evaluate(&mut cdp, "JSON.stringify(window.__clicks)").await,
                evaluate(&mut cdp, "document.elementFromPoint(60,60)?.id ?? 'nothing'").await,
            );
            println!("log: {}", host.log());
            panic!("the page never reacted to the click; it says {title:?}");
        }
        tokio::time::sleep(POLL).await;
    };
    println!("the page reports {title}, events {:?}", host.events());
    assert_eq!(
        title,
        "popup-open",
        "window.open must answer a real handle, or every sign-in flow reports a blocked pop-up \
         (log: {})",
        host.log()
    );

    let tab = wait_for_created(&host, 1, &[], BOUND).await;
    println!("the popup is tab {tab}");
    let blank = wait_for_frame(&painted, tab, BOUND).await;
    assert_eq!(
        (blank.width, blank.height),
        (800, 600),
        "the popup paints the profile's view, not the size window.open asked for",
    );

    // **A popup takes input as the tab it is.** The page that opened it draws
    // a button into it (same origin, so this is the opener's own write) and
    // the gesture goes to the popup's tab number over the host's input pipe -
    // a host that routed it to the opener would leave the button unpressed.
    let drawn = evaluate(
        &mut cdp,
        "window.__popup.document.body.style.margin='0'; \
         window.__popup.document.body.innerHTML = \
           '<button id=p style=\"position:absolute;left:0;top:0;width:300px;height:120px\">pop</button>'; \
         window.__popup.__pops = 0; \
         window.__popup.document.getElementById('p').onclick = \
           function(){ window.__popup.__pops = 1; }; \
         window.__popup.document.elementFromPoint(60, 60).id",
    )
    .await;
    assert_eq!(drawn, json!("p"), "the popup has a button at the point the click names");

    // **The popup's own paint is what proves it renders.** Its first frame is
    // a blank `about:blank`; the frame that differs from it is the composited
    // DOM, so a view drawn but never composited fails here rather than
    // passing on the blank one.
    let painted_popup = wait_for_repaint(&painted, tab, &blank, BOUND).await;
    assert_eq!((painted_popup.width, painted_popup.height), (800, 600), "the repaint is the view");

    for action in [MouseAction::Press, MouseAction::Release] {
        host.input(&host_line(&HostInput::Mouse {
            tab,
            action,
            x: 60,
            y: 60,
            button: 0,
            clicks: 1,
            modifiers: InputModifiers::default(),
        }));
    }
    let pressed = Instant::now() + BOUND;
    while evaluate(&mut cdp, "window.__popup.__pops").await != json!(1) {
        assert!(
            Instant::now() < pressed,
            "the popup's own button was never pressed: {:?}",
            host.log()
        );
        tokio::time::sleep(POLL).await;
    }

    let listed = wait_for_pages(port, 2, BOUND).await;
    assert!(
        listed.iter().any(|page| page == &opener_page),
        "the opener is still a page: {listed:?}"
    );

    // **A popup closes itself when the sign-in is done.** Closing it must
    // leave the page that opened it standing, and the server must hear the
    // tab end.
    cdp.call("Runtime.evaluate", json!({ "expression": "window.__popup.close()" }), None)
        .await
        .expect("the popup closes");
    wait_for_closed(&host, tab, BOUND).await;
    assert_eq!(
        evaluate(&mut cdp, "document.title").await,
        Value::String("popup-open".to_owned()),
        "the opener survives its popup"
    );
    let listed = wait_for_pages(port, 1, BOUND).await;
    assert_eq!(listed, vec![opener_page], "the popup's page is gone and the opener's is not");

    // **The other order, on the same host**: the opener closes under the popup
    // it opened, and the popup is the page left standing.
    for action in [MouseAction::Press, MouseAction::Release] {
        host.input(&host_line(&HostInput::Mouse {
            tab: 1,
            action,
            x: 60,
            y: 60,
            button: 0,
            clicks: 1,
            modifiers: InputModifiers::default(),
        }));
    }
    let second = wait_for_created(&host, 1, &[tab], BOUND).await;
    println!("the second popup is tab {second}");
    wait_for_pages(port, 2, BOUND).await;
    let _ = cdp.call("Page.close", json!({}), None).await;
    wait_for_closed(&host, 1, BOUND).await;
    let left = wait_for_pages(port, 1, BOUND).await;
    assert_eq!(left.len(), 1, "the popup is the page left standing: {left:?}");
    let (mut popup_cdp, _popup_events) =
        Cdp::connect_watch(port, &format!("/devtools/page/{}", left[0]))
            .await
            .expect("the popup still answers its devtools endpoint");
    assert_eq!(
        evaluate(&mut popup_cdp, "location.href").await,
        Value::String("about:blank".to_owned()),
        "the popup is a live page after its opener closed",
    );

    assert!(host.terminate(Duration::from_secs(10)).success(), "the host ends on SIGTERM");
    Ring::unlink(&ring_name);
}

/// **A popup the opener moves is left to finish.** `window.open('about:blank')`
/// then `w.location = authUrl` is how half the OAuth libraries start, and the
/// popup has no committed URL while that navigation is in flight: a host that
/// judged by the committed URL would load `about:blank` over the sign-in leg.
/// The gate is CEF's own load start, and the proof is the page that answered -
/// once, with its own URL committed.
#[tokio::test(flavor = "multi_thread")]
async fn a_popup_navigated_by_its_opener_is_left_to_finish() {
    const SLOW: &str = "<!doctype html><meta charset=utf-8><title>slow-done</title><h1>slow</h1>";
    let (slow_port, hits) = serve_page(SLOW, Duration::from_millis(600));
    let opener = format!(
        "<!doctype html><meta charset=utf-8>\
         <style>html,body{{margin:0;padding:0}}\
         button{{width:300px;height:120px;font-size:24px}}</style>\
         <button id=signin>Sign in</button>\
         <script>\
         document.getElementById('signin').onclick=function(){{\
         window.__opens=(window.__opens||0)+1;\
         if (window.__popup && !window.__popup.closed) {{ return; }}\
         window.__popup=window.open('http://127.0.0.1:{slow_port}/','signin');\
         document.title='opened';\
         }};\
         </script>"
    );
    let (page_port, _) = serve_page(&opener, Duration::ZERO);

    let scratch = tempfile::tempdir().expect("a temp dir");
    let exe = support::host_executable(scratch.path());
    let profile = scratch.path().join("profile");
    std::fs::create_dir_all(&profile).expect("the profile dir");
    let port = support::free_port();
    let ring_name = format!("/forge-popup-slow-{}", std::process::id());
    let log = scratch.path().join("host.log");
    let mut host =
        support::Host::start_with_events(&exe, &profile, port, &ring_name, "800x600", &[], &log);
    let opener_page = support::wait_for_page(port, &log, BOUND);
    let (mut cdp, _events) = connect(port, &format!("/devtools/page/{opener_page}"))
        .await
        .expect("the page's devtools endpoint accepts");
    cdp.call("Page.navigate", json!({ "url": format!("http://127.0.0.1:{page_port}/") }), None)
        .await
        .expect("the page navigates");
    let deadline = Instant::now() + BOUND;
    while evaluate(&mut cdp, "document.readyState").await != json!("complete") {
        assert!(Instant::now() < deadline, "the opener never finished loading");
        tokio::time::sleep(POLL).await;
    }
    wait_for_paint(&ring_name, &host).await;
    let tab = click_until_popup(&mut host, BOUND).await;
    wait_for_pages(port, 2, BOUND).await;
    let slow_url = Value::String(format!("http://127.0.0.1:{slow_port}/"));
    let deadline = Instant::now() + BOUND;
    loop {
        let seen = popup_value(port, &opener_page, "location.href").await;
        if seen.as_ref() == Some(&slow_url) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the popup never committed the page its opener named; it says {seen:?} \
             (tab {tab}, log: {})",
            host.log()
        );
        tokio::time::sleep(POLL).await;
    }
    assert_eq!(
        popup_value(port, &opener_page, "document.title").await,
        Some(Value::String("slow-done".to_owned())),
        "the popup is the page its opener named",
    );
    let requests = hits.load(std::sync::atomic::Ordering::Acquire);
    assert_eq!(
        requests,
        1,
        "the popup's own navigation is the only one the page took; opens {:?}, events {:?}, log {}",
        evaluate(&mut cdp, "window.__opens").await,
        host.events(),
        host.log()
    );

    assert!(host.terminate(Duration::from_secs(10)).success(), "the host ends on SIGTERM");
    Ring::unlink(&ring_name);
}

/// **A popup still starts with a browser-level client auto-attached.** CEF
/// does not start a popup's first document while an auto-attach session holds
/// its target - measured, and neither a pause flag nor a release changes it -
/// so the host's own navigation is what starts it. This rig is the
/// deterministic form of that wedge: no Playwright, one root session with the
/// same `setAutoAttach` the driver sends.
#[tokio::test(flavor = "multi_thread")]
async fn a_popup_starts_under_a_browser_level_auto_attach() {
    let scratch = tempfile::tempdir().expect("a temp dir");
    let exe = support::host_executable(scratch.path());
    let profile = scratch.path().join("profile");
    std::fs::create_dir_all(&profile).expect("the profile dir");
    let port = support::free_port();
    let ring_name = format!("/forge-popup-wedge-{}", std::process::id());
    let log = scratch.path().join("host.log");
    let (page_port, _) = serve_page(PAGE, Duration::ZERO);
    let mut host =
        support::Host::start_with_events(&exe, &profile, port, &ring_name, "800x600", &[], &log);
    let opener_page = support::wait_for_page(port, &log, BOUND);

    // **The wedge's own shape**: a root session with auto-attach, exactly as
    // Playwright asks for it, held open across the click.
    let path = browser_path(port).expect("the host answers its browser endpoint");
    let (mut root, _root_events) = connect(port, &path).await.expect("the browser root accepts");
    root.call(
        "Target.setAutoAttach",
        json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true }),
        None,
    )
    .await
    .expect("the root session auto-attaches");

    let (mut cdp, _events) = connect(port, &format!("/devtools/page/{opener_page}"))
        .await
        .expect("the page's devtools endpoint accepts");
    cdp.call("Page.navigate", json!({ "url": format!("http://127.0.0.1:{page_port}/") }), None)
        .await
        .expect("the page navigates");
    let deadline = Instant::now() + BOUND;
    while evaluate(&mut cdp, "document.readyState").await != json!("complete") {
        assert!(Instant::now() < deadline, "the opener never finished loading");
        tokio::time::sleep(POLL).await;
    }
    wait_for_paint(&ring_name, &host).await;
    let tab = click_until_popup(&mut host, BOUND).await;
    wait_for_pages(port, 2, BOUND).await;
    // **The document is the assertion.** A popup the engine never started has
    // no URL and answers no page-domain command.
    let deadline = Instant::now() + BOUND;
    let mut seen = None;
    while seen.as_ref().and_then(Value::as_str).unwrap_or_default().is_empty() {
        seen = popup_value(port, &opener_page, "location.href").await;
        assert!(
            Instant::now() < deadline,
            "the popup never committed a document under a browser-level auto-attach \
             (tab {tab}, log: {})",
            host.log()
        );
        tokio::time::sleep(POLL).await;
    }
    assert_eq!(
        popup_value(port, &opener_page, "document.readyState").await,
        Some(json!("complete")),
        "the started popup answers a page-domain command",
    );
    assert!(host.terminate(Duration::from_secs(10)).success(), "the host ends on SIGTERM");
    Ring::unlink(&ring_name);
}

/// The tab a popup was reported under, when the host has said so already.
fn created_tab(host: &support::Host, opener: u16) -> Option<u16> {
    host.events().iter().find_map(|line| match parse(line) {
        Some(HostEvent::TabCreated { tab, opener: Some(open), .. }) if open == opener => Some(tab),
        _ => None,
    })
}

/// **Click until the host reports a popup.** A gesture sent the instant a
/// document finishes loading can be dropped by the fresh renderer, so the
/// click is retried rather than assumed; the first one that opens a popup
/// ends the loop.
async fn click_until_popup(host: &mut support::Host, bound: Duration) -> u16 {
    let deadline = Instant::now() + bound;
    loop {
        for action in [MouseAction::Press, MouseAction::Release] {
            host.input(&host_line(&HostInput::Mouse {
                tab: 1,
                action,
                x: 60,
                y: 60,
                button: 0,
                clicks: 1,
                modifiers: InputModifiers::default(),
            }));
        }
        let settle = Instant::now() + Duration::from_millis(800);
        loop {
            if let Some(tab) = created_tab(host, 1) {
                return tab;
            }
            if Instant::now() >= settle {
                break;
            }
            tokio::time::sleep(POLL).await;
        }
        assert!(Instant::now() < deadline, "the page's popup never opened (log: {})", host.log());
    }
}

/// **A gesture only lands on a page that has painted.** Wait for the host's
/// first frame, read off its ring, before a test clicks.
async fn wait_for_paint(name: &str, host: &support::Host) {
    let deadline = Instant::now() + BOUND;
    loop {
        if let Ok(ring) = Ring::open(name) {
            // SAFETY: the header is mapped for this process's whole life.
            let header = unsafe { &*ring.header() };
            if header.width.load(std::sync::atomic::Ordering::Relaxed) > 0
                && header.height.load(std::sync::atomic::Ordering::Relaxed) > 0
            {
                return;
            }
        }
        assert!(Instant::now() < deadline, "the opener never painted (log: {})", host.log());
        tokio::time::sleep(POLL).await;
    }
}

/// The browser endpoint's websocket path, from the host's own `/json/version`.
fn browser_path(port: u16) -> Option<String> {
    let answer = support::http_get(&format!("http://127.0.0.1:{port}/json/version"))?;
    let body = answer.split_once("\r\n\r\n").map(|(_, body)| body)?;
    let parsed: Value = serde_json::from_str(body).ok()?;
    let url = parsed.get("webSocketDebuggerUrl")?.as_str()?;
    let path = url.split(&format!(":{port}")).nth(1)?.to_owned();
    path.starts_with("/devtools/browser/").then_some(path)
}

/// The host's ring, once the host has mapped it.
fn wait_for_ring(name: &str, host: &support::Host) -> Ring {
    let deadline = Instant::now() + BOUND;
    loop {
        match Ring::open(name) {
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

/// A DevTools connection bounded like every other wait here, so a target that
/// accepts a socket and never upgrades names the failure instead of hanging.
async fn connect(
    port: u16,
    path: &str,
) -> Option<(Cdp, tokio::sync::mpsc::UnboundedReceiver<Value>)> {
    match tokio::time::timeout(Duration::from_secs(5), Cdp::connect_watch(port, path)).await {
        Ok(Ok(pair)) => Some(pair),
        _ => None,
    }
}

/// One reading of the popup's committed URL, on a connection of its own.
///
/// **A navigation swaps the page's DevTools target**, so a connection held
/// across one answers `Inspected target navigated or closed`; each reading
/// asks the host's list for the page that is not the opener and connects
/// fresh.
async fn popup_value(port: u16, opener_page: &str, expression: &str) -> Option<Value> {
    let popup = pages(port).into_iter().find(|page| page != opener_page)?;
    let (mut cdp, _events) = connect(port, &format!("/devtools/page/{popup}")).await?;
    let reply = tokio::time::timeout(
        Duration::from_secs(5),
        cdp.call(
            "Runtime.evaluate",
            json!({ "expression": expression, "returnByValue": true }),
            None,
        ),
    )
    .await
    .ok()?
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

/// The page's title, once it is one of `want`.
async fn wait_for_title(cdp: &mut Cdp, want: &[&str], bound: Duration) -> String {
    let deadline = Instant::now() + bound;
    loop {
        let title = evaluate(cdp, "document.title").await;
        if let Some(title) = title.as_str().filter(|title| want.contains(title)) {
            return title.to_owned();
        }
        assert!(Instant::now() < deadline, "the page never read {want:?}; it says {title:?}");
        tokio::time::sleep(POLL).await;
    }
}

/// The tab number of the next popup reported under `opener`, ignoring any
/// number already in `seen`.
async fn wait_for_created(host: &support::Host, opener: u16, seen: &[u16], bound: Duration) -> u16 {
    let deadline = Instant::now() + bound;
    loop {
        for line in host.events() {
            if let Some(HostEvent::TabCreated { tab, opener: Some(open), .. }) = parse(&line)
                && open == opener
                && !seen.contains(&tab)
            {
                return tab;
            }
        }
        assert!(
            Instant::now() < deadline,
            "no popup tab was reported under tab {opener}: {:?}",
            host.events()
        );
        tokio::time::sleep(POLL).await;
    }
}

async fn wait_for_closed(host: &support::Host, tab: u16, bound: Duration) {
    let deadline = Instant::now() + bound;
    loop {
        let closed = host.events().iter().any(|line| {
            matches!(parse(line), Some(HostEvent::TabClosed { tab: closed }) if closed == tab)
        });
        if closed {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "tab {tab} was never reported closed: {:?}",
            host.events()
        );
        tokio::time::sleep(POLL).await;
    }
}

/// One tab's first encoded frame.
async fn wait_for_frame(
    frames: &std::sync::mpsc::Receiver<Frame>,
    tab: u16,
    bound: Duration,
) -> Frame {
    let deadline = Instant::now() + bound;
    loop {
        while let Ok(frame) = frames.try_recv() {
            if frame.tab == tab {
                return frame;
            }
        }
        assert!(Instant::now() < deadline, "tab {tab} never painted");
        tokio::time::sleep(POLL).await;
    }
}

/// One tab's next frame whose bytes differ from `blank`: the repaint a DOM
/// change caused.
async fn wait_for_repaint(
    frames: &std::sync::mpsc::Receiver<Frame>,
    tab: u16,
    blank: &Frame,
    bound: Duration,
) -> Frame {
    let deadline = Instant::now() + bound;
    loop {
        while let Ok(frame) = frames.try_recv() {
            if frame.tab == tab && frame.bytes != blank.bytes {
                return frame;
            }
        }
        assert!(Instant::now() < deadline, "tab {tab} never repainted after its DOM changed");
        tokio::time::sleep(POLL).await;
    }
}

/// The page targets the host's DevTools endpoint lists.
fn pages(port: u16) -> Vec<String> {
    let answer =
        support::http_get(&format!("http://127.0.0.1:{port}/json/list")).unwrap_or_default();
    let body = answer.split_once("\r\n\r\n").map(|(_, body)| body).unwrap_or_default();
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|parsed| parsed.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter(|entry| entry.get("type").and_then(Value::as_str) == Some("page"))
        .filter_map(|entry| entry.get("id")?.as_str().map(str::to_owned))
        .collect()
}

async fn wait_for_pages(port: u16, want: usize, bound: Duration) -> Vec<String> {
    let deadline = Instant::now() + bound;
    loop {
        let listed = pages(port);
        if listed.len() == want {
            return listed;
        }
        assert!(Instant::now() < deadline, "the browser lists {listed:?}, not {want} pages");
        tokio::time::sleep(POLL).await;
    }
}
