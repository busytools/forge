//! A cross-site `window.open` must not take the opener with it.
//!
//! **This is the red the host opener bridge is built to turn green.** With a
//! browser-level CDP client attached (`Target.setAutoAttach` and
//! `waitForDebuggerOnStart`, the shape `connectOverCDP` and the passkey veto
//! both use), a page that opens a cross-site popup never gets a handle back
//! from `window.open`, and the opener's renderer crashes about 35 s later
//! (#2861). The popup's own document still loads; it is the opener that dies.
//!
//! **The control is the same page without a browser-level client**, where
//! `window.open` answers a handle. Without it, a red could be the harness
//! rather than the wedge.
//!
//! The test holds the opener's own page session across the popup, because
//! that is the session the wedge silences: a freshly created page connection
//! still answers while the old one does not.
//!
//! **These need the real bundle**: the harness stages the workspace's own host
//! the way the server ships it.

#![cfg(target_os = "macos")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod support;

use std::io::{Read as _, Write as _};
use std::time::{Duration, Instant};

use forge_browser::control::{HostEvent, parse};
use forge_browser::osr::Ring;
use forge_browser::window::{Backlog, Cdp};
use serde_json::{Value, json};

/// How long CEF is given to boot, paint and open a popup.
const BOUND: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(50);

/// The opener's page: a button that opens a popup **straight at an address**,
/// the shape a sign-in flow arrives in, and keeps the handle `window.open`
/// answered.
fn opener_page(popup: &str) -> String {
    format!(
        "<!doctype html><meta charset=utf-8>\
         <style>html,body{{margin:0;padding:0}}\
         button{{width:300px;height:120px;font-size:24px}}</style>\
         <button id=signin>Sign in</button>\
         <script>\
         window.__clicks=[];\
         addEventListener('mousedown',function(e){{\
           window.__clicks.push({{x:e.clientX,y:e.clientY,target:e.target&&e.target.id}});\
         }},true);\
         document.getElementById('signin').onclick=function(){{\
           if (window.__popup && !window.__popup.closed) return;\
           var w=window.open('{popup}','signin');\
           window.__popup=w;\
           document.title=w?'popup-open':'popup-blocked';\
         }};\
         document.title='ready';\
         </script>"
    )
}

/// Serve `body` on `host` for the length of the test, and answer how many
/// requests the page took.
///
/// **A real origin, not a `data:` document**: the opener is an ordinary web
/// page, and the popup must be a different site so the engine makes a
/// cross-site popup. `127.0.0.1` and `[::1]` are different sites without a
/// name lookup.
fn serve_on(host: &str, body: &str) -> (u16, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let listener = std::net::TcpListener::bind((host, 0)).expect("a loopback port");
    serve_listener(listener, body)
}

/// Serve `body` on a listener already bound, answering its port.
fn serve_listener(
    listener: std::net::TcpListener,
    body: &str,
) -> (u16, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
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
                let _ = stream.write_all(body.as_bytes());
                let _ = stream.flush();
            });
        }
    });
    (port, hits)
}

/// Open the popup from an opener page, with or without the browser-level
/// client the wedge needs, and answer the opener's title through a renderer-
/// domain call on the session held across the popup.
///
/// Panics with the wedge's own words when the call hangs or the handle never
/// came back.
async fn opener_title(auto_attach: bool) -> String {
    let scratch = tempfile::tempdir().expect("a temp dir");
    let exe = support::host_executable(scratch.path());
    let profile = scratch.path().join("profile");
    std::fs::create_dir_all(&profile).expect("the profile dir");
    let port = support::free_port();
    // **A short name**: a POSIX shared-memory name is bounded by the
    // platform (31 characters on macOS, leading slash included).
    let ring_name =
        format!("/forge-ob-{}-{}", if auto_attach { "red" } else { "ctl" }, std::process::id());
    let log = scratch.path().join("host.log");

    // **A different SITE is what makes the engine create a cross-site popup.**
    // `[::1]` and the opener's `127.0.0.1` are different sites, and neither
    // needs a name lookup or the network.
    let (provider_port, _provider) = serve_on(
        "::1",
        "<!doctype html><meta charset=utf-8><title>provider</title><h1>provider</h1>",
    );
    let provider_url = format!("http://[::1]:{provider_port}/");
    let (page_port, _page) = serve_on("127.0.0.1", &opener_page(&provider_url));

    let mut host =
        support::Host::start_with_events(&exe, &profile, port, &ring_name, "800x600", &[], &log);
    let opener = support::wait_for_page(port, &log, BOUND);

    // **The wedge's own shape**: a root session with auto-attach, exactly as
    // Playwright asks for it, held open across the click. The control omits
    // it and must answer.
    let (root, _root_events) = if auto_attach {
        let path = browser_path(port).expect("the host answers its browser endpoint");
        let (mut connected, events) = connect(port, &path).await.expect("the browser root accepts");
        connected
            .call(
                "Target.setAutoAttach",
                json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true }),
                None,
            )
            .await
            .expect("the root session auto-attaches");
        (Some(connected), Some(events))
    } else {
        (None, None)
    };

    // **The opener's own page session, held across the popup.** A fresh page
    // connection answers after the wedge; this one does not, which is the
    // whole defect.
    let (mut cdp, _events) = connect(port, &format!("/devtools/page/{opener}"))
        .await
        .expect("the page's devtools endpoint accepts");
    cdp.call("Page.navigate", json!({ "url": format!("http://127.0.0.1:{page_port}/") }), None)
        .await
        .expect("the page navigates");
    wait_for_title(&mut cdp, &["ready"], BOUND).await;

    // **An object id from before the popup.** The wedge silences
    // `Runtime.callFunctionOn` on the opener's page session, and that is the
    // command the driver's own utility-script evaluation uses; a plain
    // `Runtime.evaluate` can still answer once. The object keeps the target
    // alive across the popup.
    let target = cdp
        .call(
            "Runtime.evaluate",
            json!({ "expression": "document", "objectGroup": "forge-red" }),
            None,
        )
        .await
        .expect("the page hands its document");
    let object = target
        .pointer("/result/objectId")
        .and_then(Value::as_str)
        .expect("the document has an object id")
        .to_owned();

    let tab = click_until_popup(&mut host, BOUND).await;
    assert_ne!(tab, 1, "the popup is a tab of its own");

    // **The popup exists as a page target**, so a failure below is the
    // opener's and not a popup that never opened.
    let listed = wait_for_pages(port, 2, BOUND).await;
    assert!(
        listed.iter().any(|page| page != &opener),
        "the popup's page is listed beside the opener: {listed:?}"
    );

    // **The assertion the bridge exists for.** The title is the opener's own
    // word: `popup-open` means `window.open` answered a handle, which is the
    // sign-in contract. The call itself is renderer-domain, so a wedge that
    // silences it is a hang and the bound is the failure.
    let answered = tokio::time::timeout(
        Duration::from_secs(10),
        cdp.call(
            "Runtime.callFunctionOn",
            json!({
                "objectId": object,
                "functionDeclaration": "function () { return this.title; }",
                "returnByValue": true,
            }),
            None,
        ),
    )
    .await;
    let title = match answered {
        Ok(Ok(reply)) => {
            reply.pointer("/result/value").and_then(Value::as_str).unwrap_or_default().to_owned()
        }
        Ok(Err(why)) => panic!(
            "the opener's renderer refused a call after a cross-site window.open: {why} \
             (log: {})",
            host.log_tail(),
        ),
        Err(elapsed) => panic!(
            "the opener stopped answering Runtime.callFunctionOn after a cross-site \
             window.open: the #2861 wedge (bound {elapsed:?}, popup tab {tab}, events {:?}, \
             log: {})",
            host.events(),
            host.log_tail(),
        ),
    };

    assert!(host.terminate(Duration::from_secs(10)).success(), "the host ends on SIGTERM");
    Ring::unlink(&ring_name);
    drop(root);
    title
}

/// **The red.** With the browser-level client attached, a cross-site
/// `window.open` must still answer a handle and leave the opener answering.
#[tokio::test(flavor = "multi_thread")]
async fn a_cross_site_open_leaves_the_opener_answering_under_a_browser_level_auto_attach() {
    assert_eq!(
        opener_title(true).await,
        "popup-open",
        "a cross-site window.open under a browser-level auto-attach must answer a handle; \
         today it never returns and the opener is wedged (#2861)",
    );
}

/// **The control**: the same page with no browser-level client answers the
/// handle. A red on this side would be the harness, not the wedge.
#[tokio::test(flavor = "multi_thread")]
async fn a_cross_site_open_answers_a_handle_without_a_browser_level_client() {
    assert_eq!(opener_title(false).await, "popup-open", "the control's handle must come back");
}

/// **The relay carries `postMessage` both ways, and only where the sender
/// named.** The popup's page posts once at an origin that is not the app's
/// (dropped) and once at the app's (delivered); the opener posts to the
/// popup; each side reports what it received in its own title.
#[tokio::test(flavor = "multi_thread")]
async fn a_cross_site_popup_relays_post_message_both_ways() {
    let scratch = tempfile::tempdir().expect("a temp dir");
    let exe = support::host_executable(scratch.path());
    let profile = scratch.path().join("profile");
    std::fs::create_dir_all(&profile).expect("the profile dir");
    let port = support::free_port();
    let ring_name = format!("/forge-ob-msg-{}", std::process::id());
    let log = scratch.path().join("host.log");

    // **Both origins are chosen before either body is built**: the provider
    // names the app's origin and the app names the provider's.
    let provider_listener = std::net::TcpListener::bind(("::1", 0)).expect("a provider port");
    let provider_port = provider_listener.local_addr().expect("the provider port").port();
    let app_listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("an app port");
    let app_port = app_listener.local_addr().expect("the app port").port();
    let provider_url = format!("http://[::1]:{provider_port}/");
    let provider_origin = format!("http://[::1]:{provider_port}");
    let app_origin = format!("http://127.0.0.1:{app_port}");

    let provider = format!(
        "<!doctype html><meta charset=utf-8><title>provider-start</title>\
         <script>\
         addEventListener('message', function (e) {{\
           document.title = 'provider-got:' + (e.data && e.data.from) + ':' + e.origin;\
         }});\
         document.title = 'provider-ready';\
         setTimeout(function () {{\
           if (!window.opener) {{ document.title = 'provider-no-opener'; return; }}\
           window.opener.postMessage({{from:'provider-wrong'}}, 'http://127.0.0.1:1');\
           window.opener.postMessage({{from:'provider'}}, '{app_origin}');\
         }}, 300);\
         </script>"
    );
    let app = format!(
        "<!doctype html><meta charset=utf-8>\
         <style>html,body{{margin:0;padding:0}}\
         button{{width:300px;height:120px;font-size:24px}}</style>\
         <button id=signin>Sign in</button>\
         <script>\
         window.__messages=[];\
         addEventListener('message', function (e) {{\
           window.__messages.push({{from: e.data && e.data.from, origin: e.origin}});\
           document.title = 'got:' + (e.data && e.data.from) + ':' + e.origin + ':' + \
             window.__messages.length;\
           if (e.data && e.data.from === 'provider' && window.__popup) {{\
             window.__popup.postMessage({{from:'opener'}}, '{provider_origin}');\
           }}\
         }});\
         document.getElementById('signin').onclick=function() {{\
           if (window.__popup && !window.__popup.closed) return;\
           var w=window.open('{provider_url}','signin');\
           window.__popup=w;\
           document.title = w ? 'popup-open' : 'popup-blocked';\
         }};\
         document.title='ready';\
         </script>"
    );
    let _provider = serve_listener(provider_listener, &provider);
    let _app = serve_listener(app_listener, &app);

    let mut host =
        support::Host::start_with_events(&exe, &profile, port, &ring_name, "800x600", &[], &log);
    let opener = support::wait_for_page(port, &log, BOUND);

    let path = browser_path(port).expect("the host answers its browser endpoint");
    let (mut root, mut root_events) = connect(port, &path).await.expect("the browser root accepts");
    root.call(
        "Target.setAutoAttach",
        json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true }),
        None,
    )
    .await
    .expect("the root session auto-attaches");

    // **A real client resumes what it pauses.** The veto and the Playwright
    // driver both do; a rig that only pauses leaves the popup's renderer
    // waiting for a debugger that never comes, which is the test's bug, not
    // the bridge's.
    let resumer = tokio::spawn(async move {
        while let Some(event) = root_events.recv().await {
            if event["method"] != "Target.attachedToTarget" {
                continue;
            }
            let session = event["params"]["sessionId"].as_str().unwrap_or_default().to_owned();
            if session.is_empty() {
                continue;
            }
            let _ = tokio::time::timeout(
                Duration::from_secs(5),
                root.call("Runtime.runIfWaitingForDebugger", json!({}), Some(&session)),
            )
            .await;
        }
    });

    let (mut cdp, _events) = connect(port, &format!("/devtools/page/{opener}"))
        .await
        .expect("the page's devtools endpoint accepts");
    cdp.call("Page.navigate", json!({ "url": format!("http://127.0.0.1:{app_port}/") }), None)
        .await
        .expect("the app navigates");
    wait_for_title(&mut cdp, &["ready"], BOUND).await;

    click_until_popup(&mut host, BOUND).await;
    let listed = wait_for_pages(port, 2, BOUND).await;
    let popup = listed
        .into_iter()
        .find(|page| page != &opener)
        .expect("the popup's page is listed beside the opener");

    // **The app hears the provider, and only the matching post.** Two were
    // sent; the log holds one, with the provider's own origin.
    let wanted = format!("got:provider:{provider_origin}:1");
    let deadline = Instant::now() + BOUND;
    loop {
        let title = evaluate(&mut cdp, "document.title").await;
        if title.as_str() == Some(wanted.as_str()) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the app never heard the provider: {title:?} (wanted {wanted:?}, popup {:?}, log: {})",
            popup_probe(port, &popup).await,
            host.log_tail(),
        );
        tokio::time::sleep(POLL).await;
    }

    // **The popup heard the opener through its own `window.opener`.**
    let wanted = format!("provider-got:opener:{app_origin}");
    let deadline = Instant::now() + BOUND;
    loop {
        let title = popup_title(port, &popup).await;
        if title.as_deref() == Some(wanted.as_str()) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the provider never heard the opener: {title:?} (wanted {wanted:?}, log: {})",
            host.log_tail(),
        );
        tokio::time::sleep(POLL).await;
    }

    resumer.abort();
    assert!(host.terminate(Duration::from_secs(10)).success(), "the host ends on SIGTERM");
    Ring::unlink(&ring_name);
}

/// **A popup that closes itself ends its tab and flips the opener's
/// `closed`.** A host-created tab is not script-closable to Blink, so the
/// popup's `window.close()` goes through the shim; the opener's proxy reports
/// it, and the host reports the tab ended.
#[tokio::test(flavor = "multi_thread")]
async fn a_popup_that_closes_itself_flips_the_openers_closed() {
    let scratch = tempfile::tempdir().expect("a temp dir");
    let exe = support::host_executable(scratch.path());
    let profile = scratch.path().join("profile");
    std::fs::create_dir_all(&profile).expect("the profile dir");
    let port = support::free_port();
    let ring_name = format!("/forge-ob-close-{}", std::process::id());
    let log = scratch.path().join("host.log");

    let (provider_port, _provider) = serve_on(
        "::1",
        "<!doctype html><meta charset=utf-8><title>provider-ready</title>\
         <script>setTimeout(function () { window.close(); }, 500);</script>",
    );
    let provider_url = format!("http://[::1]:{provider_port}/");
    let app = format!(
        "<!doctype html><meta charset=utf-8>\
         <style>html,body{{margin:0;padding:0}}\
         button{{width:300px;height:120px;font-size:24px}}</style>\
         <button id=signin>Sign in</button>\
         <script>\
         setInterval(function () {{\
           if (window.__popup && window.__popup.closed) document.title = 'popup-closed';\
         }}, 200);\
         document.getElementById('signin').onclick=function() {{\
           if (window.__popup && !window.__popup.closed) return;\
           var w=window.open('{provider_url}','signin');\
           window.__popup=w;\
           document.title = w ? 'popup-open' : 'popup-blocked';\
         }};\
         document.title='ready';\
         </script>"
    );
    let (app_port, _app) = serve_on("127.0.0.1", &app);

    let mut host =
        support::Host::start_with_events(&exe, &profile, port, &ring_name, "800x600", &[], &log);
    let opener = support::wait_for_page(port, &log, BOUND);
    let (mut cdp, _events) = connect(port, &format!("/devtools/page/{opener}"))
        .await
        .expect("the page's devtools endpoint accepts");
    cdp.call("Page.navigate", json!({ "url": format!("http://127.0.0.1:{app_port}/") }), None)
        .await
        .expect("the app navigates");
    wait_for_title(&mut cdp, &["ready"], BOUND).await;

    let tab = click_until_popup(&mut host, BOUND).await;
    wait_for_pages(port, 2, BOUND).await;

    let deadline = Instant::now() + BOUND;
    loop {
        let title = evaluate(&mut cdp, "document.title").await;
        if title.as_str() == Some("popup-closed") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the opener never read the popup as closed: {title:?} (log: {})",
            host.log_tail(),
        );
        tokio::time::sleep(POLL).await;
    }
    assert!(
        host.events().iter().any(|line| matches!(
            parse(line),
            Some(HostEvent::TabClosed { tab: ended }) if ended == tab
        )),
        "the host reported the popup tab ended: {:?}",
        host.events(),
    );

    assert!(host.terminate(Duration::from_secs(10)).success(), "the host ends on SIGTERM");
    Ring::unlink(&ring_name);
}

/// **A popup that navigates mid-flow keeps its bridge.** The provider sends
/// the popup to a second origin; that document is a new renderer, and its
/// `window.opener` must still reach the app with a post.
#[tokio::test(flavor = "multi_thread")]
async fn a_popup_that_navigates_mid_flow_keeps_its_bridge() {
    let scratch = tempfile::tempdir().expect("a temp dir");
    let exe = support::host_executable(scratch.path());
    let profile = scratch.path().join("profile");
    std::fs::create_dir_all(&profile).expect("the profile dir");
    let port = support::free_port();
    let ring_name = format!("/forge-ob-move-{}", std::process::id());
    let log = scratch.path().join("host.log");

    // **All three listeners are bound before any body is built**: the
    // provider names the callback's address and the callback names the app's
    // origin.
    let provider_listener = std::net::TcpListener::bind(("::1", 0)).expect("a provider port");
    let provider_port = provider_listener.local_addr().expect("the provider port").port();
    let provider_url = format!("http://[::1]:{provider_port}/");
    let callback_listener = std::net::TcpListener::bind(("localhost", 0)).expect("a callback port");
    let callback_port = callback_listener.local_addr().expect("the callback port").port();
    let callback_url = format!("http://localhost:{callback_port}/");
    let app_listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("an app port");
    let app_port = app_listener.local_addr().expect("the app port").port();
    let app_origin = format!("http://127.0.0.1:{app_port}");

    let provider = format!(
        "<!doctype html><meta charset=utf-8><title>provider</title>\
         <script>setTimeout(function () {{ location.href = '{callback_url}'; }}, 300);</script>"
    );
    let callback = format!(
        "<!doctype html><meta charset=utf-8><title>callback</title>\
         <script>\
         document.title = window.opener ? 'callback-has-opener' : 'callback-no-opener';\
         if (window.opener) window.opener.postMessage({{from:'callback'}}, '{app_origin}');\
         </script>"
    );
    let app = format!(
        "<!doctype html><meta charset=utf-8>\
         <style>html,body{{margin:0;padding:0}}\
         button{{width:300px;height:120px;font-size:24px}}</style>\
         <button id=signin>Sign in</button>\
         <script>\
         addEventListener('message', function (e) {{\
           if (e.data && e.data.from === 'callback') document.title = 'got-callback:' + e.origin;\
         }});\
         document.getElementById('signin').onclick=function() {{\
           if (window.__popup && !window.__popup.closed) return;\
           var w=window.open('{provider_url}','signin');\
           window.__popup=w;\
           document.title = w ? 'popup-open' : 'popup-blocked';\
         }};\
         document.title='ready';\
         </script>"
    );
    let _provider = serve_listener(provider_listener, &provider);
    let _callback = serve_listener(callback_listener, &callback);
    let _app = serve_listener(app_listener, &app);

    let mut host =
        support::Host::start_with_events(&exe, &profile, port, &ring_name, "800x600", &[], &log);
    let opener = support::wait_for_page(port, &log, BOUND);
    let (mut cdp, _events) = connect(port, &format!("/devtools/page/{opener}"))
        .await
        .expect("the page's devtools endpoint accepts");
    cdp.call("Page.navigate", json!({ "url": format!("http://127.0.0.1:{app_port}/") }), None)
        .await
        .expect("the app navigates");
    wait_for_title(&mut cdp, &["ready"], BOUND).await;

    click_until_popup(&mut host, BOUND).await;
    let listed = wait_for_pages(port, 2, BOUND).await;
    let popup = listed
        .into_iter()
        .find(|page| page != &opener)
        .expect("the popup's page is listed beside the opener");

    // **The callback's document is a new renderer**, and its `window.opener`
    // still reaches the app with its own origin.
    let wanted = format!("got-callback:http://localhost:{callback_port}");
    let deadline = Instant::now() + BOUND;
    loop {
        let title = evaluate(&mut cdp, "document.title").await;
        if title.as_str() == Some(wanted.as_str()) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the callback's opener post never reached the app: {title:?} \
             (wanted {wanted:?}, popup {:?}, log: {})",
            popup_probe(port, &popup).await,
            host.log_tail(),
        );
        tokio::time::sleep(POLL).await;
    }

    assert!(host.terminate(Duration::from_secs(10)).success(), "the host ends on SIGTERM");
    Ring::unlink(&ring_name);
}

/// **The `about:blank`-then-navigate pattern keeps the opener answering.**
/// That open is same-origin, so the engine's own popup path creates it; the
/// popup's first document starts at once, and the later cross-site navigation
/// is what the wedge would have to catch. If this fails, the pattern is a
/// finding to report, not a silent pass.
#[tokio::test(flavor = "multi_thread")]
async fn an_about_blank_popup_navigated_cross_site_leaves_the_opener_answering() {
    let scratch = tempfile::tempdir().expect("a temp dir");
    let exe = support::host_executable(scratch.path());
    let profile = scratch.path().join("profile");
    std::fs::create_dir_all(&profile).expect("the profile dir");
    let port = support::free_port();
    let ring_name = format!("/forge-ob-blank-{}", std::process::id());
    let log = scratch.path().join("host.log");

    let (provider_port, _provider) = serve_on(
        "::1",
        "<!doctype html><meta charset=utf-8><title>provider</title><h1>provider</h1>",
    );
    let provider_url = format!("http://[::1]:{provider_port}/");
    let app = format!(
        "<!doctype html><meta charset=utf-8>\
         <style>html,body{{margin:0;padding:0}}\
         button{{width:300px;height:120px;font-size:24px}}</style>\
         <button id=signin>Sign in</button>\
         <script>\
         document.getElementById('signin').onclick=function() {{\
           if (window.__popup && !window.__popup.closed) return;\
           var w=window.open('about:blank','signin');\
           window.__popup=w;\
           if (w) w.location = '{provider_url}';\
           document.title = w ? 'popup-open' : 'popup-blocked';\
         }};\
         document.title='ready';\
         </script>"
    );
    let (app_port, _app) = serve_on("127.0.0.1", &app);

    let mut host =
        support::Host::start_with_events(&exe, &profile, port, &ring_name, "800x600", &[], &log);
    let opener = support::wait_for_page(port, &log, BOUND);

    let path = browser_path(port).expect("the host answers its browser endpoint");
    let (mut root, mut root_events) = connect(port, &path).await.expect("the browser root accepts");
    root.call(
        "Target.setAutoAttach",
        json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true }),
        None,
    )
    .await
    .expect("the root session auto-attaches");
    let resumer = tokio::spawn(async move {
        while let Some(event) = root_events.recv().await {
            if event["method"] != "Target.attachedToTarget" {
                continue;
            }
            let session = event["params"]["sessionId"].as_str().unwrap_or_default().to_owned();
            if session.is_empty() {
                continue;
            }
            let _ = tokio::time::timeout(
                Duration::from_secs(5),
                root.call("Runtime.runIfWaitingForDebugger", json!({}), Some(&session)),
            )
            .await;
        }
    });

    let (mut cdp, _events) = connect(port, &format!("/devtools/page/{opener}"))
        .await
        .expect("the page's devtools endpoint accepts");
    cdp.call("Page.navigate", json!({ "url": format!("http://127.0.0.1:{app_port}/") }), None)
        .await
        .expect("the app navigates");
    wait_for_title(&mut cdp, &["ready"], BOUND).await;

    click_until_popup(&mut host, BOUND).await;
    let listed = wait_for_pages(port, 2, BOUND).await;
    let popup = listed
        .into_iter()
        .find(|page| page != &opener)
        .expect("the popup's page is listed beside the opener");
    // **The popup's document is up** before the opener is asked anything.
    let deadline = Instant::now() + BOUND;
    loop {
        if popup_title(port, &popup).await.as_deref() == Some("provider") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the popup never loaded the cross-site address (log: {})",
            host.log_tail(),
        );
        tokio::time::sleep(POLL).await;
    }

    let answered = tokio::time::timeout(
        Duration::from_secs(10),
        cdp.call(
            "Runtime.evaluate",
            json!({ "expression": "document.title", "returnByValue": true }),
            None,
        ),
    )
    .await;
    match answered {
        Ok(Ok(reply)) => assert_eq!(
            reply.pointer("/result/value").cloned(),
            Some(json!("popup-open")),
            "the about:blank popup answered a handle (log: {})",
            host.log_tail(),
        ),
        Ok(Err(why)) => panic!(
            "the opener's renderer refused a call after an about:blank popup navigated \
             cross-site: {why} (log: {})",
            host.log_tail(),
        ),
        Err(elapsed) => panic!(
            "the opener stopped answering after an about:blank popup navigated cross-site: \
             the #2861 wedge reaches this pattern too (bound {elapsed:?}, log: {})",
            host.log_tail(),
        ),
    }

    resumer.abort();
    assert!(host.terminate(Duration::from_secs(10)).success(), "the host ends on SIGTERM");
    Ring::unlink(&ring_name);
}

/// One reading of a page's title over a connection of its own.
async fn popup_title(port: u16, page: &str) -> Option<String> {
    popup_value(port, page, "document.title").await?.as_str().map(str::to_owned)
}

/// The popup's own report of its bridge state, for a failure's message.
async fn popup_probe(port: u16, page: &str) -> Option<Value> {
    popup_value(
        port,
        page,
        "({ title: document.title, native: typeof window.__forgeBridgeNative, \
          popup: window.__forgeBridgePopup === true, \
          opener: window.opener === null ? 'null' : 'set', \
          close: window.close ? 'set' : 'missing' })",
    )
    .await
}

async fn popup_value(port: u16, page: &str, expression: &str) -> Option<Value> {
    let (mut cdp, _events) = connect(port, &format!("/devtools/page/{page}")).await?;
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

/// The browser endpoint's websocket path, from the host's own `/json/version`.
fn browser_path(port: u16) -> Option<String> {
    let answer = support::http_get(&format!("http://127.0.0.1:{port}/json/version"))?;
    let body = answer.split_once("\r\n\r\n").map(|(_, body)| body)?;
    let parsed: Value = serde_json::from_str(body).ok()?;
    let url = parsed.get("webSocketDebuggerUrl")?.as_str()?;
    let path = url.split(&format!(":{port}")).nth(1)?.to_owned();
    path.starts_with("/devtools/browser/").then_some(path)
}

/// A DevTools connection bounded like every other wait here, so a target that
/// accepts a socket and never upgrades names the failure instead of hanging.
async fn connect(port: u16, path: &str) -> Option<(Cdp, forge_browser::window::Events)> {
    match tokio::time::timeout(
        Duration::from_secs(5),
        Cdp::connect_watch(port, path, Backlog::EveryEvent),
    )
    .await
    {
        Ok(Ok(pair)) => Some(pair),
        _ => None,
    }
}

/// **Click until the host reports a popup.** A gesture sent the instant a
/// document finishes loading can be dropped by the fresh renderer, so the
/// click is retried rather than assumed; the first one that opens a popup
/// ends the loop.
async fn click_until_popup(host: &mut support::Host, bound: Duration) -> u16 {
    let deadline = Instant::now() + bound;
    loop {
        support::click(host, 60, 60);
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

/// The tab a popup was reported under, when the host has said so already.
fn created_tab(host: &support::Host, opener: u16) -> Option<u16> {
    host.events().iter().find_map(|line| match parse(line) {
        Some(HostEvent::TabCreated { tab, opener: Some(open), .. }) if open == opener => Some(tab),
        _ => None,
    })
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

async fn evaluate(cdp: &mut Cdp, expression: &str) -> Value {
    let reply = cdp
        .call("Runtime.evaluate", json!({ "expression": expression, "returnByValue": true }), None)
        .await
        .expect("the page evaluates");
    reply.pointer("/result/value").cloned().unwrap_or(Value::Null)
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
