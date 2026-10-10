//! The parked page, measured live: a real session's browser call answered by
//! the bridge with no page anywhere, while frames are kept with nothing
//! drawing them.
//!
//! **This is the 2026-10-10 fingerprint, reproduced headlessly.** The wedge
//! was a parked webview starving the socket: twelve asks died at the 200s
//! bound with the calls already at the host. There is no webview here at all -
//! the bridge is the whole client - so the two claims are measured directly:
//!
//! 1. **The socket is consumed while the page is away.** The test stops
//!    heartbeating past the bridge's own freshness window, drives a real
//!    turn, and then reads the catch-up: the record's conversation carries
//!    the whole turn, which only an unbroken read of the socket can hold.
//! 2. **A browser ask is answered while the page is away.** The session's
//!    real `browser_navigate` call is answered by the real vendored host,
//!    visible in the strip's own ring (one ask in flight, then none) and in
//!    the turn's text carrying the page's own title.
//!
//! Ignored by default: it needs a scratch forge serving on `FORGE_WEDGE_URL`
//! (the dev-stack recipe's default is 8791) and the vendored browser stack,
//! with `FORGE_WEDGE_SLOT` naming the stack's own seat. Run:
//!
//! ```sh
//! FORGE_WEDGE_SLOT=Busytools/forge/lead \
//!   cargo nextest run --manifest-path client/src-tauri/Cargo.toml \
//!     --run-ignored ignored-only --test bridge_live --no-capture
//! ```

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use forge_client::bridge::{Bridge, ClientEvent};
use forge_client::browser::{BrowserHost, StackPaths};
use serde_json::{Value, json};
use support::Launched;
use tokio::sync::mpsc;

/// The scratch stack's socket. The dev-stack default is 8791; a stack that
/// had to take the next port pair names its own with `FORGE_WEDGE_URL`.
fn url() -> String {
    std::env::var("FORGE_WEDGE_URL").unwrap_or_else(|_| "ws://127.0.0.1:8791/socket".to_owned())
}

/// The bridge's own freshness window (`bridge.rs`): a page that has not
/// heartbeated for this long reads as away. The test waits past it before
/// driving anything, so the whole measurement happens page-away.
const FRESH: Duration = Duration::from_secs(15);

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a scratch forge on 8791 (the dev-stack recipe) and the vendored stack"]
async fn a_seats_browser_ask_is_answered_while_the_page_is_away() {
    let named = std::env::var("FORGE_WEDGE_SLOT").expect("FORGE_WEDGE_SLOT=org/project/label");
    let (org, rest) = named.split_once('/').expect("org/project/label");
    let (project, label) = rest.split_once('/').expect("org/project/label");
    let slot = json!({ "org": org, "project": project, "label": label });

    // The real host over the vendored stack: this is the client's own
    // browser, answering as the app's would.
    let stack = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("browser-stack");
    assert!(
        stack.join("node/bin/node").is_file(),
        "the vendored stack is not there - run `just vendor-browser-stack`",
    );
    let dir = tempfile::tempdir().expect("a temp dir");
    let paths = StackPaths {
        stack,
        user_data: dir.path().join("user-data"),
        output: dir.path().join("output"),
        profiles: dir.path().join("profiles"),
    };
    let host = Arc::new(BrowserHost::new(paths.clone()));
    let launched = host.start().await.unwrap_or_else(|why| panic!("the browser comes up: {why}"));
    let browser = Launched::new(launched.pid, launched.port, paths.user_data.clone());

    let (tx, mut rx) = mpsc::unbounded_channel();
    let bridge = Bridge::new(Some(Arc::clone(&host)), tx);
    bridge.connect(url());
    wait_status(&bridge, "open").await;

    // The seat's own subscription, declaring what the app declares:
    // answering, and hosting the browser. The subscribe's answer is the
    // seat's record, and the proof the slot is real.
    bridge.subscribe(json!({ "session": slot.clone() }), true, true);
    let opening = wait_inbound(&mut rx, "snapshot", Duration::from_secs(30)).await;
    assert_eq!(opening["subject"], json!({ "session": slot.clone() }), "the seat answers itself");

    // **The page goes away**: nothing heartbeats from here, so the freshness
    // window lapses and every frame below arrives with nothing drawing it.
    tokio::time::sleep(FRESH + Duration::from_secs(2)).await;
    let away_began = std::time::Instant::now();

    // A real turn, asking for the browser: the ask that follows is the one
    // the wedge killed. **The page is served with a secret that exists
    // nowhere else** - not in the prompt, not in any earlier run - so the
    // answer can only come from a browser that really fetched it.
    let secret = format!("wedge-{}-{}", std::process::id(), now_millis());
    let port = serve_secret(secret.clone());
    let page = format!("http://127.0.0.1:{port}/");
    bridge
        .dispatch(
            json!({
                "prompt_under": {
                    "key": slot,
                    "text": format!(
                        "Open {page} with the browser_navigate tool, then read the page \
                         (browser_snapshot) and tell me the exact text inside its h1 element, \
                         and nothing else.",
                    ),
                    "attachments": [],
                    // Deliberately NOT the secret: the uuid rides the
                    // prompt echo, and a secret that echoes proves nothing.
                    "uuid": "wedge-live-1",
                    "source": "you",
                }
            }),
            false,
        )
        .await
        .unwrap_or_else(|why| panic!("the prompt went: {why}"));

    // **The ask is answered here, page-away.** The ring is the first proof:
    // one ask in flight, then none - the session's browser call reached this
    // process and was answered while no page existed to answer it.
    //
    // The turn is still being written, so the test then lives a page's life:
    // it sleeps past the bridge's freshness window, wakes with one
    // heartbeat, and reads whatever the catch-up hands over - the record as
    // a snapshot, its conversation as the newest page - until the page's own
    // title rides in on the turn.
    let mut saw: Vec<usize> = Vec::new();
    let mut collected = String::new();
    let mut reconciled = 0_usize;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(360);
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(FRESH + Duration::from_secs(5)).await;
        bridge.heartbeat();
        while let Ok(event) = rx.try_recv() {
            match event {
                ClientEvent::Inbound(message) => {
                    if message["kind"] == json!("snapshot")
                        && message["subject"] == json!({ "session": slot.clone() })
                        && message["data"]["conversation"].is_object()
                    {
                        reconciled += 1;
                    }
                    collected.push_str(&message.to_string());
                }
                ClientEvent::Asks { inflight } => saw.push(inflight),
                _ => {}
            }
        }
        if collected.contains(&secret) {
            break;
        }
    }
    let elapsed = away_began.elapsed();

    eprintln!(
        "page-away for {:.1}s: {} catch-up snapshots, ring saw {:?}, turn text collected: {} bytes",
        elapsed.as_secs_f64(),
        reconciled,
        saw,
        collected.len(),
    );

    assert!(
        saw.contains(&1) && saw.contains(&0),
        "an ask ran and was answered while the page was away: {saw:?}",
    );
    assert!(reconciled > 0, "the catch-up handed the record over after the away window");
    assert!(
        collected.contains("browser_navigate"),
        "the turn's frames survived the away window: {}",
        &collected[collected.len().saturating_sub(2_000)..],
    );
    assert!(
        collected.contains(&secret),
        "the ask was answered by the real browser (the served page's secret is in the turn): {}",
        &collected[collected.len().saturating_sub(2_000)..],
    );

    bridge.close();
    browser.reap();
}

/// Now, in milliseconds, as a run-unique stamp.
fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|held| held.as_millis())
        .unwrap_or(0)
}

/// A page only a real browser can read: one h1 carrying `secret`, served on
/// a port of its own. The secret is in the BODY and never in the prompt, so
/// an answer carrying it proves the browser fetched the page.
fn serve_secret(secret: String) -> u16 {
    use std::io::Write as _;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
    let port = listener.local_addr().expect("the address").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let body = format!("<!doctype html><html><body><h1>{secret}</h1></body></html>");
            let answer = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len(),
            );
            let _ = stream.write_all(answer.as_bytes());
        }
    });
    port
}

async fn wait_status(bridge: &Arc<Bridge>, want: &str) {
    for _ in 0..600 {
        if bridge.state()["status"] == json!(want) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the connection never reached {want}");
}

/// The next `Inbound` message of a kind, skipping the rest.
async fn wait_inbound(
    rx: &mut mpsc::UnboundedReceiver<ClientEvent>,
    kind: &str,
    within: Duration,
) -> Value {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Some(ClientEvent::Inbound(message))) if message["kind"] == json!(kind) => {
                return message;
            }
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => panic!("nothing of kind {kind} within {within:?}"),
        }
    }
}
