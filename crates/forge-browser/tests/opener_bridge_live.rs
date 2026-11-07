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
    let auto = if auto_attach { Some(attach_and_resume(port).await) } else { None };

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
    let popup = listed
        .into_iter()
        .find(|page| page != &opener)
        .expect("the popup's page is listed beside the opener");

    // **The popup actually started.** The bridge does not avoid the driver's
    // attachment - it removes the shared renderer and the CEF popup target -
    // so this rig resumes what it pauses, and the popup's own document is
    // what proves it ran rather than merely appeared as a target.
    let deadline = Instant::now() + BOUND;
    loop {
        if popup_title(port, &popup).await.as_deref() == Some("provider") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the popup never started its document (log: {})",
            host.log_tail(),
        );
        tokio::time::sleep(POLL).await;
    }

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

    // **The client attached and every pause was resumed.** The bridge keeps
    // the driver's attachment; what it removes is the shared renderer and the
    // CEF popup target. A rig that only paused would leave the popup dead and
    // this test would be asserting against a quiet host.
    if let Some((resumer, attached, failures)) = auto {
        resumer.abort();
        assert_eq!(
            failures.load(std::sync::atomic::Ordering::Acquire),
            0,
            "every attached target was resumed",
        );
        let urls = attached.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        assert!(
            urls.iter().any(|(id, _)| id == &popup),
            "the driver attached to the popup's target too: {urls:?}",
        );
    }
    assert!(host.terminate(Duration::from_secs(10)).success(), "the host ends on SIGTERM");
    Ring::unlink(&ring_name);
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

    // **The provider posts inline, at document start.** That is the early
    // message the channel must already be bound for: the host runs a nested
    // CEF loop inside the create that starts this document.
    let provider = format!(
        "<!doctype html><meta charset=utf-8><title>provider-start</title>\
         <script>\
         addEventListener('message', function (e) {{\
           document.title = 'provider-got:' + (e.data && e.data.from) + ':' + e.origin;\
         }});\
         if (!window.opener) {{\
           document.title = 'provider-no-opener:' + window.name + ':' + \
             (window.__forgeBridgePopup === true);\
         }} else {{\
           window.opener.postMessage({{from:'provider-wrong'}}, 'http://127.0.0.1:1');\
           window.opener.postMessage({{from:'provider'}}, '{app_origin}');\
           document.title = 'provider-posted';\
         }}\
         </script>"
    );
    let app = format!(
        "<!doctype html><meta charset=utf-8>\
         <style>html,body{{margin:0;padding:0}}\
         button{{width:300px;height:120px;font-size:24px}}</style>\
         <button id=signin>Sign in</button>\
         <script>\
         window.__messages=[];\
         window.__syntax=[false,false];\
         addEventListener('message', function (e) {{\
           window.__messages.push({{from: e.data && e.data.from, origin: e.origin}});\
           document.title = 'got:' + (e.data && e.data.from) + ':' + e.origin;\
           if (e.data && e.data.from === 'provider' && window.__popup) {{\
             window.__popup.postMessage({{from:'opener'}}, '{provider_origin}');\
           }}\
         }});\
         document.getElementById('signin').onclick=function() {{\
           if (window.__popup && !window.__popup.closed) return;\
           var w=window.open('{provider_url}','signin');\
           window.__popup=w;\
           document.title = w ? 'popup-open' : 'popup-blocked';\
           if (w) {{\
             try {{ w.postMessage({{from:'bad'}}, 'not a url'); }} \
             catch (e) {{ window.__syntax[0] = e.name === 'SyntaxError'; }}\
             try {{ w.postMessage({{from:'bad'}}, '{provider_origin}/path'); }} \
             catch (e) {{ window.__syntax[1] = e.name === 'SyntaxError'; }}\
           }}\
         }};\
         document.title='ready';\
         </script>"
    );
    let _provider = serve_listener(provider_listener, &provider);
    let _app = serve_listener(app_listener, &app);

    let mut host =
        support::Host::start_with_events(&exe, &profile, port, &ring_name, "800x600", &[], &log);
    let opener = support::wait_for_page(port, &log, BOUND);

    let (resumer, attached, failures) = attach_and_resume(port).await;

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
    let wanted = format!("got:provider:{provider_origin}");
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
    // **The delivered list is exactly one entry with the provider's own
    // origin.** A wrong-origin post that was delivered instead would leave
    // `from: provider-wrong` here, and a second delivery would leave two
    // entries; the assertion names what actually arrived.
    let messages = evaluate(&mut cdp, "window.__messages").await;
    assert_eq!(
        messages,
        json!([{ "from": "provider", "origin": provider_origin }]),
        "the app heard the provider's matching post and nothing else (log: {})",
        host.log_tail(),
    );

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

    // **An invalid targetOrigin throws in the page**, as the platform's
    // postMessage does: an unparseable string and a path-bearing URL are both
    // SyntaxError, not a silent drop.
    let syntax = evaluate(&mut cdp, "JSON.stringify(window.__syntax)").await;
    assert_eq!(
        syntax.as_str(),
        Some("[true,true]"),
        "an invalid targetOrigin must throw SyntaxError (log: {})",
        host.log_tail(),
    );

    // **The driver attached to both and every pause was resumed.** The
    // bridge removes the shared renderer and the CEF popup target; it does
    // not avoid the attachment, and this is the evidence.
    resumer.abort();
    assert_eq!(
        failures.load(std::sync::atomic::Ordering::Acquire),
        0,
        "every attached target was resumed",
    );
    let urls = attached.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
    assert!(
        urls.iter().any(|(id, _)| id == &popup),
        "the driver attached to the popup's target too: {urls:?}",
    );

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

    let (resumer, attached, failures) = attach_and_resume(port).await;

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

    // **The driver attached to the popup's address and every pause was
    // resumed.** A resumer that swallowed both would read a popup the driver
    // paused and never resumed as a pass.
    resumer.abort();
    assert_eq!(
        failures.load(std::sync::atomic::Ordering::Acquire),
        0,
        "every attached target was resumed",
    );
    let urls = attached.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
    assert!(
        urls.iter().any(|(id, _)| id == &popup),
        "the driver attached to the popup's target too: {urls:?}",
    );
    assert!(host.terminate(Duration::from_secs(10)).success(), "the host ends on SIGTERM");
    Ring::unlink(&ring_name);
}

/// **A cross-origin iframe cannot reach the bridge or open a popup.** The
/// native object is the main frame's alone, and a web address opened in a
/// cross-origin frame is refused rather than becoming a CEF popup that shares
/// that frame's renderer.
#[tokio::test(flavor = "multi_thread")]
async fn a_cross_origin_iframe_cannot_reach_the_bridge_or_open_a_popup() {
    let scratch = tempfile::tempdir().expect("a temp dir");
    let exe = support::host_executable(scratch.path());
    let profile = scratch.path().join("profile");
    std::fs::create_dir_all(&profile).expect("the profile dir");
    let port = support::free_port();
    let ring_name = format!("/forge-ob-frame-{}", std::process::id());
    let log = scratch.path().join("host.log");

    let app_listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("an app port");
    let app_port = app_listener.local_addr().expect("the app port").port();
    let app_origin = format!("http://127.0.0.1:{app_port}");
    let frame_listener = std::net::TcpListener::bind(("::1", 0)).expect("a frame port");
    let frame_port = frame_listener.local_addr().expect("the frame port").port();
    let frame_url = format!("http://[::1]:{frame_port}/");
    let frame_origin = format!("http://[::1]:{frame_port}");

    let frame = format!(
        "<!doctype html><meta charset=utf-8><title>frame</title>\
         <style>html,body{{margin:0;padding:0}}\
         button{{width:300px;height:150px;font-size:24px}}</style>\
         <button id=open>Open</button>\
         <script>\
         var native = typeof window.__forgeBridgeNative;\
         document.getElementById('open').onclick = function () {{\
           if (window.__opened) return;\
           window.__opened = true;\
           var opened = window.open(location.href, 'frame-popup');\
           parent.postMessage({{native: native, \
             opened: opened === null ? 'null' : 'object'}}, '{app_origin}');\
         }};\
         </script>"
    );
    let app = format!(
        "<!doctype html><meta charset=utf-8>\
         <iframe src=\"{frame_url}\"></iframe>\
         <script>\
         addEventListener('message', function (e) {{\
           if (e.data && 'native' in e.data) {{\
             document.title = 'frame:' + e.data.native + ':' + e.data.opened + ':' + e.origin;\
           }}\
         }});\
         document.title = 'ready';\
         </script>"
    );
    let _frame = serve_listener(frame_listener, &frame);
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

    // **The open is a real click inside the frame.** An ungestured open is
    // answered null by the engine's popup blocker even with the shim gone, so
    // only a gestured one proves the shim refused: without it, this click
    // would create a page.
    let wanted = format!("frame:undefined:null:{frame_origin}");
    let deadline = Instant::now() + BOUND;
    loop {
        support::click(&mut host, 60, 60);
        let title = evaluate(&mut cdp, "document.title").await;
        if title.as_str() == Some(wanted.as_str()) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the cross-origin frame got more than a refusal: {title:?} \
             (wanted {wanted:?}, log: {})",
            host.log_tail(),
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // **No page was created**: the iframe's refused open did not fall back to
    // CEF's popup.
    assert_eq!(pages(port).len(), 1, "the iframe's open created no page");

    assert!(host.terminate(Duration::from_secs(10)).success(), "the host ends on SIGTERM");
    Ring::unlink(&ring_name);
}

/// **A `noopener` popup whose renderer dies is reaped.** It has no channel,
/// so the tab itself is the record that the host made it; a dead renderer
/// must not leave a row pointing at nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_noopener_popup_whose_renderer_dies_is_reaped() {
    let scratch = tempfile::tempdir().expect("a temp dir");
    let exe = support::host_executable(scratch.path());
    let profile = scratch.path().join("profile");
    std::fs::create_dir_all(&profile).expect("the profile dir");
    let port = support::free_port();
    let ring_name = format!("/forge-ob-noop-{}", std::process::id());
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
           if (window.__opened) return;\
           window.__popup = window.open('{provider_url}', 'signin', 'noopener');\
           window.__opened = true;\
           document.title = window.__popup === null ? 'noopener-null' : 'noopener-object';\
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
    let listed = wait_for_pages(port, 2, BOUND).await;
    let popup = listed
        .into_iter()
        .find(|page| page != &opener)
        .expect("the popup's page is listed beside the opener");
    let (mut popup_cdp, _popup_events) = connect(port, &format!("/devtools/page/{popup}"))
        .await
        .expect("the popup's devtools endpoint accepts");
    // **The noopener popup starts at its address**, with no blank first
    // document: it has no opener role to wait for.
    wait_for_title(&mut popup_cdp, &["provider"], BOUND).await;
    // **Crash its renderer.** The call itself may not answer once the renderer
    // is gone, so it is bounded and its failure is expected.
    let _ =
        tokio::time::timeout(Duration::from_secs(5), popup_cdp.call("Page.crash", json!({}), None))
            .await;

    let deadline = Instant::now() + BOUND;
    loop {
        let closed = host.events().iter().any(|line| {
            matches!(parse(line), Some(HostEvent::TabClosed { tab: closed }) if closed == tab)
        });
        if closed {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "a noopener popup whose renderer died was not reaped: {:?} (log: {})",
            host.events(),
            host.log_tail(),
        );
        tokio::time::sleep(POLL).await;
    }

    assert!(host.terminate(Duration::from_secs(10)).success(), "the host ends on SIGTERM");
    Ring::unlink(&ring_name);
}

/// **A `noopener` popup closes itself.** Its `window.close()` ends the tab
/// and the host emits `TabClosed`; a page target that leaves while the tab
/// stays behind is the dead row this test would catch.
#[tokio::test(flavor = "multi_thread")]
async fn a_noopener_popup_closes_itself() {
    let scratch = tempfile::tempdir().expect("a temp dir");
    let exe = support::host_executable(scratch.path());
    let profile = scratch.path().join("profile");
    std::fs::create_dir_all(&profile).expect("the profile dir");
    let port = support::free_port();
    let ring_name = format!("/forge-ob-noop-close-{}", std::process::id());
    let log = scratch.path().join("host.log");

    let (provider_port, _provider) = serve_on(
        "::1",
        "<!doctype html><meta charset=utf-8><title>provider</title>\
         <script>\
         setTimeout(function () {{\
           document.title = 'state:' + (typeof window.__forgeBridgePopup) + ':' + \
             (window.opener === null);\
         }}, 300);\
         setTimeout(function () {{ window.close(); }}, 3000);\
         </script>",
    );
    let provider_url = format!("http://[::1]:{provider_port}/");
    let app = format!(
        "<!doctype html><meta charset=utf-8>\
         <style>html,body{{margin:0;padding:0}}\
         button{{width:300px;height:120px;font-size:24px}}</style>\
         <button id=signin>Sign in</button>\
         <script>\
         document.getElementById('signin').onclick=function() {{\
           if (window.__opened) return;\
           window.__opened = true;\
           var w = window.open('{provider_url}', 'signin', 'noopener');\
           document.title = w === null ? 'noopener-null' : 'noopener-object';\
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
    let listed = wait_for_pages(port, 2, BOUND).await;
    let popup = listed
        .into_iter()
        .find(|page| page != &opener)
        .expect("the popup's page is listed beside the opener");
    let (mut popup_cdp, _popup_events) = connect(port, &format!("/devtools/page/{popup}"))
        .await
        .expect("the popup's devtools endpoint accepts");
    // **The popup carries no bridge role**: `__forgeBridgePopup` is undefined
    // and its opener is null. A regression that gave it the role would fail
    // here rather than hiding behind the same end state.
    wait_for_title(&mut popup_cdp, &["state:undefined:true"], BOUND).await;

    // **The popup closes itself and its tab ends**: a page target that leaves
    // while the tab stays behind is the dead row this test exists to catch.
    let deadline = Instant::now() + BOUND;
    loop {
        let closed = host.events().iter().any(|line| {
            matches!(parse(line), Some(HostEvent::TabClosed { tab: closed }) if closed == tab)
        });
        if closed && pages(port).len() == 1 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the noopener popup never closed itself (tab {tab}, log: {})",
            host.log_tail(),
        );
        tokio::time::sleep(POLL).await;
    }

    assert!(host.terminate(Duration::from_secs(10)).success(), "the host ends on SIGTERM");
    Ring::unlink(&ring_name);
}

/// **A noopener popup that takes the shim's role through `window.name` can
/// still close itself.** Its second document sees the marker, installs the
/// close routing, and the host ends the tab from its own record because a
/// noopener popup has no channel.
#[tokio::test(flavor = "multi_thread")]
async fn a_noopener_popup_that_takes_the_name_marker_closes_itself() {
    let scratch = tempfile::tempdir().expect("a temp dir");
    let exe = support::host_executable(scratch.path());
    let profile = scratch.path().join("profile");
    std::fs::create_dir_all(&profile).expect("the profile dir");
    let port = support::free_port();
    let ring_name = format!("/forge-ob-noop-mark-{}", std::process::id());
    let log = scratch.path().join("host.log");

    let (second_port, _second) = serve_on(
        "::1",
        "<!doctype html><meta charset=utf-8><title>second</title>\
         <script>setTimeout(function () { window.close(); }, 1200);</script>",
    );
    let second_url = format!("http://[::1]:{second_port}/");
    let (provider_port, _provider) = serve_on(
        "::1",
        &format!(
            "<!doctype html><meta charset=utf-8><title>provider</title>\
             <script>\
             window.name = 'forge-opener-bridge';\
             location.href = '{second_url}';\
             </script>"
        ),
    );
    let provider_url = format!("http://[::1]:{provider_port}/");
    let app = format!(
        "<!doctype html><meta charset=utf-8>\
         <style>html,body{{margin:0;padding:0}}\
         button{{width:300px;height:120px;font-size:24px}}</style>\
         <button id=signin>Sign in</button>\
         <script>\
         document.getElementById('signin').onclick=function() {{\
           if (window.__opened) return;\
           window.__opened = true;\
           var w = window.open('{provider_url}', 'signin', 'noopener');\
           document.title = w === null ? 'noopener-null' : 'noopener-object';\
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

    // **The tab ends**: the marker routes the close to the host, and the
    // host's record of the noopener tab is what finishes it.
    let deadline = Instant::now() + BOUND;
    loop {
        let closed = host.events().iter().any(|line| {
            matches!(parse(line), Some(HostEvent::TabClosed { tab: closed }) if closed == tab)
        });
        if closed && pages(port).len() == 1 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the name-marker popup never closed itself (tab {tab}, log: {})",
            host.log_tail(),
        );
        tokio::time::sleep(POLL).await;
    }

    assert!(host.terminate(Duration::from_secs(10)).success(), "the host ends on SIGTERM");
    Ring::unlink(&ring_name);
}

/// **The opener closes first and the popup survives it.** The popup's
/// `window.opener.closed` flips, its posts are dropped, and its tab stays; the
/// same fact `TabRegistry` keeps as `is_popup` so an attach timeout still
/// knows a popup is open.
#[tokio::test(flavor = "multi_thread")]
async fn a_popup_whose_opener_closes_first_sees_it_closed_and_survives() {
    let scratch = tempfile::tempdir().expect("a temp dir");
    let exe = support::host_executable(scratch.path());
    let profile = scratch.path().join("profile");
    std::fs::create_dir_all(&profile).expect("the profile dir");
    let port = support::free_port();
    let ring_name = format!("/forge-ob-orphan-{}", std::process::id());
    let log = scratch.path().join("host.log");

    let (provider_port, _provider) = serve_on(
        "::1",
        "<!doctype html><meta charset=utf-8><title>provider-ready</title>\
         <script>\
         setInterval(function () {\
           if (!window.opener) { document.title = 'no-opener'; return; }\
           document.title = window.opener.closed ? 'opener-closed' : 'opener-open';\
           if (window.opener.closed) window.opener.postMessage({from:'after'}, '*');\
         }, 200);\
         </script>",
    );
    let provider_url = format!("http://[::1]:{provider_port}/");
    let app = format!(
        "<!doctype html><meta charset=utf-8>\
         <style>html,body{{margin:0;padding:0}}\
         button{{width:300px;height:120px;font-size:24px}}</style>\
         <button id=signin>Sign in</button>\
         <script>\
         document.getElementById('signin').onclick=function() {{\
           if (window.__popup) return;\
           window.__popup = window.open('{provider_url}', 'signin');\
           document.title = window.__popup ? 'popup-open' : 'popup-blocked';\
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
    let listed = wait_for_pages(port, 2, BOUND).await;
    let popup = listed
        .into_iter()
        .find(|page| page != &opener)
        .expect("the popup's page is listed beside the opener");

    // **The popup's document is up with its proxy before the opener goes.**
    // Closing first would race the push against a context that has not
    // loaded; the host re-sends on a later document, but the test should
    // exercise the ordinary path.
    let deadline = Instant::now() + BOUND;
    loop {
        let title = popup_title(port, &popup).await;
        if title.as_deref() == Some("opener-open") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the popup never installed its opener proxy: {title:?} (log: {})",
            host.log_tail(),
        );
        tokio::time::sleep(POLL).await;
    }

    // **The opener tab ends the way a call or the sweep ends it.** The boot
    // tab takes number 1, and the popup is the other page.
    host.input(&forge_browser::input::host_line(&forge_browser::input::HostInput::TabClose {
        tab: 1,
    }));

    let deadline = Instant::now() + BOUND;
    loop {
        let title = popup_title(port, &popup).await;
        if title.as_deref() == Some("opener-closed") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the popup never read its opener as closed: {title:?} (log: {})",
            host.log_tail(),
        );
        tokio::time::sleep(POLL).await;
    }
    assert!(
        !host.events().iter().any(|line| matches!(
            parse(line),
            Some(HostEvent::TabClosed { tab: closed }) if closed == tab
        )),
        "the popup survives its opener: {:?}",
        host.events(),
    );
    let _ = host.terminate(Duration::from_secs(10));
    Ring::unlink(&ring_name);
}

/// A root client that does what the driver does: auto-attach to every target,
/// pause it, and resume it. Answers the attachments it made and how many
/// resumes the host refused. **The bridge does not avoid this attachment**:
/// it removes the shared renderer and CEF's popup target, so the popup is a
/// page target like any tab.
async fn attach_and_resume(
    port: u16,
) -> (
    tokio::task::JoinHandle<()>,
    std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    let path = browser_path(port).expect("the host answers its browser endpoint");
    let (mut root, mut events) = connect(port, &path).await.expect("the browser root accepts");
    root.call(
        "Target.setAutoAttach",
        json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true }),
        None,
    )
    .await
    .expect("the root session auto-attaches");
    let attached = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let failures = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let record = std::sync::Arc::clone(&attached);
    let failed = std::sync::Arc::clone(&failures);
    let resumer = tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            if event["method"] != "Target.attachedToTarget" {
                continue;
            }
            let session = event["params"]["sessionId"].as_str().unwrap_or_default().to_owned();
            let info = &event["params"]["targetInfo"];
            let id = info["targetId"].as_str().unwrap_or_default().to_owned();
            // **The URL at attach is the one the target was created at**, which
            // for a bridged popup is `about:blank`; the test names the popup by
            // the target id the browser lists.
            let url = info["url"].as_str().unwrap_or_default().to_owned();
            if session.is_empty() {
                continue;
            }
            record.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push((id, url));
            let resumed = tokio::time::timeout(
                Duration::from_secs(5),
                root.call("Runtime.runIfWaitingForDebugger", json!({}), Some(&session)),
            )
            .await;
            if !matches!(resumed, Ok(Ok(_))) {
                failed.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            }
        }
    });
    (resumer, attached, failures)
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
        "({ title: document.title, name: window.name, \
          native: typeof window.__forgeBridgeNative, \
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
        Cdp::connect_watch(port, path, Backlog::EveryEvent, None, ""),
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
