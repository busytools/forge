//! The raise path, end to end, with no window anywhere.
//!
//! **The whole flow runs verbatim; only the browser is a stand-in.** The
//! process the host spawns is a stub that writes the `DevToolsActivePort`
//! file a real launch writes and then sleeps, and this test serves the two
//! endpoints the host talks to over that port - the devtools HTTP answers
//! (`/json/version`, `/json/list`) and the CDP websocket that carries the
//! window moves. So what is pinned here is the host's own decisions and its
//! exact calls: the launch's shape, the window born hidden and moved off
//! every screen BEFORE anything can drive it, a raise that moves a window
//! and relaunches nothing, and a hide that leaves the browser running.
//!
//! What this file cannot cover is what needs a real engine - that a headed
//! browser honours the moves, that the page stays drivable out of sight, that
//! the fill survives the raise. Those are the live suite's, deliberately.
//!
//! The one thing these cases do not take from the shell is the budget a
//! launch's wait is given: the shell gives it 15 s, and these give it one of
//! their own (see `TEST_LAUNCH_BUDGET`), because that wait is spent on the
//! machine starting the stub, which a loaded machine can hold back past the
//! shell's own.
//!
//! One arm is deliberately absent here: a show over a launch from BEFORE the
//! window model closes that launch, and a close kills by the port's listener
//! (which, with this test serving the port itself, would be this very test
//! process). The live suite carries that arm instead, as
//! a_raise_over_an_older_headless_launch_brings_up_the_new_shape.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use forge_client::browser::chromium;
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

/// The identity the fake browser answers with: the port file's second line
/// and `/json/version` must agree on it, which is what makes the host take
/// the endpoint for this profile's own launch.
const IDENTITY: &str = "/devtools/browser/STUBID";

/// Where the stub rests: the window server's own corner for a hidden window
/// (read off a real one, 2026-10-12), which is what a raise moves away from.
const HIDDEN_AT: (i64, i64) = (2008, 1022);
/// Where a raise puts the window, as `chromium::RAISE_AT` does.
const RAISED_AT: (i64, i64) = (40, 60);

/// The budget the tests give a launch: **not the shell's own.**
///
/// A launch's wait is spent on the OS starting the stub and the stub
/// answering, and a machine running a gate beside the suite can hold that
/// back far past the shell's 15 s - measured 2026-10-14, a launch that takes
/// half a second idle took 13 s under load, and three of these five tests
/// went red at the deadline with every poll reading no port file at all. The
/// stall is the machine's, not the launch's, and no case here pins how long
/// a launch may take, so they give the wait a budget load cannot eat.
const TEST_LAUNCH_BUDGET: Duration = Duration::from_secs(60);

/// What the fake devtools endpoint has seen: every CDP call, in order, and
/// the window's current bounds - which the moves change and the assertions
/// read back.
struct Endpoint {
    calls: Mutex<Vec<(String, Value)>>,
    bounds: Mutex<Option<Value>>,
    /// The window's page URLs, in the order the endpoint lists them. Empty is
    /// the shape where nothing is left to raise - a window whose last tab
    /// went, or a launch the person's X left without one.
    pages: Mutex<Vec<String>>,
}

impl Default for Endpoint {
    fn default() -> Self {
        Self {
            calls: Mutex::default(),
            bounds: Mutex::default(),
            pages: Mutex::new(vec!["https://example.com/prepared".to_owned()]),
        }
    }
}

impl Endpoint {
    /// The pages as they are now: what `/json/list` and `Target.getTargets`
    /// will answer, and what the test set them to.
    fn pages_now(&self) -> Vec<String> {
        self.pages.lock().expect("the pages").clone()
    }

    /// Put the window in the state `pages` describes.
    fn set_pages(&self, pages: Vec<String>) {
        *self.pages.lock().expect("the pages") = pages;
    }

    /// The window object is gone while the pages stay: `Browser.getWindowForTarget`
    /// refuses and nothing can be raised - the state a page target outliving
    /// its window leaves.
    fn windowless(&self) {
        *self.bounds.lock().expect("the bounds") = None;
    }

    /// The methods called, in order.
    fn methods(&self) -> Vec<String> {
        self.calls.lock().expect("the calls").iter().map(|(method, _)| method.clone()).collect()
    }

    /// The params of every call to `method`.
    fn params_of(&self, method: &str) -> Vec<Value> {
        self.calls
            .lock()
            .expect("the calls")
            .iter()
            .filter(|(called, _)| called == method)
            .map(|(_, params)| params.clone())
            .collect()
    }

    /// Put the endpoint in the state a launch leaves: a window exists, hidden
    /// in the bottom corner.
    fn born_hidden(&self) {
        *self.bounds.lock().expect("the bounds") = Some(json!({
            "left": HIDDEN_AT.0, "top": HIDDEN_AT.1,
            "width": 1200, "height": 988, "windowState": "normal",
        }));
    }

    /// The window's bounds now, when there is one.
    fn bounds(&self) -> Option<Value> {
        self.bounds.lock().expect("the bounds").clone()
    }

    /// Every `setWindowBounds` that asked for `left`, in order.
    fn moved_to(&self, left: i64) -> usize {
        self.params_of("Browser.setWindowBounds")
            .iter()
            .filter(|params| params["bounds"]["left"] == json!(left))
            .count()
    }

    fn brought_forward(&self) -> usize {
        self.methods().iter().filter(|method| *method == "Page.bringToFront").count()
    }
}

/// Start the endpoint: one listener answering devtools HTTP and upgrading the
/// CDP websocket, told apart by the request head (peeked, never consumed, so
/// a websocket handshake reaches tungstenite whole).
async fn serve(endpoint: Arc<Endpoint>) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let port = listener.local_addr().expect("the bound port").port();
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else { continue };
            let endpoint = Arc::clone(&endpoint);
            tokio::spawn(async move {
                let mut head = [0_u8; 2048];
                // **The whole head, however the segments fall**: one peek can
                // catch half a handshake on a loaded machine (measured
                // 2026-10-17), which routes a websocket open down the HTTP arm.
                let mut peeked;
                let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                loop {
                    peeked = tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        socket.peek(&mut head),
                    )
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .unwrap_or(0);
                    let request = String::from_utf8_lossy(&head[..peeked]);
                    if request.contains("\r\n\r\n") || tokio::time::Instant::now() >= deadline {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                let request = String::from_utf8_lossy(&head[..peeked]).to_ascii_lowercase();
                if request.contains("upgrade: websocket") {
                    serve_cdp(socket, endpoint).await;
                } else {
                    serve_http(socket, endpoint, port, &request).await;
                }
            });
        }
    });
    port
}

/// The plain devtools answers: `/json/version` with this endpoint's identity,
/// `/json/list` with the window's pages as the test set them.
async fn serve_http(
    mut socket: tokio::net::TcpStream,
    endpoint: Arc<Endpoint>,
    port: u16,
    request: &str,
) {
    use tokio::io::AsyncWriteExt as _;

    let body = if request.starts_with("get /json/version") {
        json!({ "webSocketDebuggerUrl": format!("ws://127.0.0.1:{port}{IDENTITY}") }).to_string()
    } else {
        json!(page_rows(&endpoint.pages_now())).to_string()
    };
    let answer = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len(),
    );
    let _ = socket.write_all(answer.as_bytes()).await;
    let _ = socket.shutdown().await;
}

/// The page list as a devtools endpoint answers it under `/json/list`: ids
/// `PAGE1..` in the order given, which is what a raise reads back.
fn page_rows(pages: &[String]) -> Vec<Value> {
    pages
        .iter()
        .enumerate()
        .map(|(at, url)| json!({ "id": format!("PAGE{}", at + 1), "type": "page", "url": url }))
        .collect()
}

/// The CDP side: numbered calls, the few methods the window path uses, each
/// recorded so the tests can read what the host asked for.
async fn serve_cdp(socket: tokio::net::TcpStream, endpoint: Arc<Endpoint>) {
    let Ok(mut ws) = tokio_tungstenite::accept_async(socket).await else { return };
    while let Some(Ok(frame)) = ws.next().await {
        let Message::Text(text) = frame else { continue };
        let Ok(call) = serde_json::from_str::<Value>(&text) else { continue };
        let id = call.get("id").cloned().unwrap_or(Value::Null);
        let method = call.get("method").and_then(Value::as_str).unwrap_or_default().to_owned();
        let params = call.get("params").cloned().unwrap_or(Value::Null);
        endpoint.calls.lock().expect("the calls").push((method.clone(), params.clone()));
        let result = match method.as_str() {
            "Target.getTargets" => json!({
                "targetInfos": page_rows(&endpoint.pages_now())
                    .into_iter()
                    .map(|row| json!({ "targetId": row["id"], "type": "page", "url": row["url"] }))
                    .collect::<Vec<Value>>(),
            }),
            "Target.createTarget" => {
                // A window born for its first page REPLACES what the browser
                // holds; a tab is added to it. This is what a real browser
                // answers after either, and what the marker is read from.
                let url =
                    params.get("url").and_then(Value::as_str).unwrap_or("about:blank").to_owned();
                let new_window = params.get("newWindow").and_then(Value::as_bool).unwrap_or(false);
                let target = {
                    let mut pages = endpoint.pages.lock().expect("the pages");
                    if new_window {
                        pages.clear();
                        endpoint.born_hidden();
                    }
                    pages.push(url);
                    format!("PAGE{}", pages.len())
                };
                json!({ "targetId": target })
            }
            "Target.attachToTarget" => json!({ "sessionId": "SESSION1" }),
            "Browser.getWindowForTarget" => match endpoint.bounds() {
                Some(bounds) => json!({ "windowId": 7, "bounds": bounds }),
                None => {
                    return_answer(&mut ws, id, Err("Browser window not found")).await;
                    continue;
                }
            },
            "Browser.setWindowBounds" => {
                let asked = params.get("bounds").cloned().unwrap_or(Value::Null);
                let mut bounds = endpoint.bounds.lock().expect("the bounds");
                if let Some(current) = bounds.as_mut()
                    && let (Some(current), Some(asked)) =
                        (current.as_object_mut(), asked.as_object())
                {
                    for (name, value) in asked {
                        current.insert(name.clone(), value.clone());
                    }
                }
                Value::Null
            }
            _ => Value::Null,
        };
        return_answer(&mut ws, id, Ok(result)).await;
    }
}

async fn return_answer(
    ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    id: Value,
    answer: Result<Value, &str>,
) {
    let message = match answer {
        Ok(result) => json!({ "id": id, "result": result }),
        Err(why) => json!({ "id": id, "error": { "message": why } }),
    };
    let _ = ws.send(Message::Text(message.to_string().into())).await;
}

/// The stub browser, written beside the profile: it reads `stub-config` (this
/// endpoint's port, identity and how long to hold its port file back) from its
/// own directory, writes the port file a real launch writes, records the
/// command line it was spawned with, and sleeps until the test's guard reaps
/// it.
fn stub_browser(dir: &Path) -> PathBuf {
    let stub = dir.join("stub-browser");
    std::fs::write(
        &stub,
        r#"#!/usr/bin/env python3
import os, sys, time

argv = sys.argv[1:]
udd = None
for a in argv:
    if a.startswith("--user-data-dir="):
        udd = a.split("=", 1)[1]
if udd is None:
    sys.exit(3)
with open(os.path.join(udd, "fake-launch.log"), "a") as f:
    f.write(" ".join(argv) + "\n")
config = open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "stub-config")).read().split()
delay = float(config[2]) if len(config) > 2 else 0.0
if delay:
    time.sleep(delay)
with open(os.path.join(udd, "DevToolsActivePort"), "w") as f:
    f.write(config[0] + "\n" + config[1] + "\n")
time.sleep(600)
"#,
    )
    .expect("the stub is written");
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("+x");
    stub
}

/// One test's world: a temp dir, the endpoint, the stub, and the profile.
struct World {
    _dir: tempfile::TempDir,
    endpoint: Arc<Endpoint>,
    stub: PathBuf,
    profile: PathBuf,
}

async fn world() -> World {
    let dir = tempfile::tempdir().expect("a temp dir");
    let profile = dir.path().join("profile");
    std::fs::create_dir_all(&profile).expect("the profile");
    let endpoint = Arc::new(Endpoint::default());
    let port = serve(Arc::clone(&endpoint)).await;
    let stub = stub_browser(dir.path());
    std::fs::write(dir.path().join("stub-config"), format!("{port}\n{IDENTITY}\n"))
        .expect("the stub config");
    World { _dir: dir, endpoint, stub, profile }
}

impl World {
    /// The profile as a launch of this build leaves it: the port file, the
    /// window's birth marker, and the endpoint answering as that launch - but
    /// no process of the test's own (the launch already happened, in the
    /// fiction).
    fn launched(&self) {
        std::fs::write(
            self.profile.join("DevToolsActivePort"),
            format!("{}\n{}\n", self.port(), IDENTITY),
        )
        .expect("the port file");
        std::fs::write(self.profile.join("has-window"), b"").expect("the birth marker");
        self.endpoint.born_hidden();
    }

    fn port(&self) -> u16 {
        // The port the endpoint bound: the stub config carries it.
        let config =
            std::fs::read_to_string(self.stub.parent().expect("the dir").join("stub-config"))
                .expect("the config reads");
        config.lines().next().expect("the port line").parse().expect("a port")
    }

    /// The command lines the stub was spawned with, oldest first.
    fn launches(&self) -> Vec<String> {
        std::fs::read_to_string(self.profile.join("fake-launch.log"))
            .map(|text| text.lines().map(str::to_owned).collect())
            .unwrap_or_default()
    }

    /// Wait, bounded, for the stub to have been spawned `n` times.
    async fn launches_at_least(&self, n: usize) -> Vec<String> {
        for _ in 0..100 {
            let seen = self.launches();
            if seen.len() >= n {
                return seen;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        self.launches()
    }

    /// A holder that kills the stub a launch spawned, at the test's end.
    ///
    /// **By pid only - never by the port.** The port this fake browser
    /// answers on is the TEST's own listener, so a kill-by-port here would
    /// take this very process down, and the fixture would be killing the
    /// reader instead of the browser.
    fn reap_after(&self, pid: Option<u32>) -> Reap {
        Reap { pid }
    }

    /// Hold the stub's port file back by `delay` past the point a launch
    /// normally writes it: a child the machine has stalled, on demand.
    fn stub_delay(&self, delay: Duration) {
        std::fs::write(
            self.stub.parent().expect("the dir").join("stub-config"),
            format!("{}\n{IDENTITY}\n{}\n", self.port(), delay.as_secs_f64()),
        )
        .expect("the stub config");
    }
}

/// Kill the stub a test's launch left, by the pid the launch answered with.
struct Reap {
    pid: Option<u32>,
}

impl Drop for Reap {
    fn drop(&mut self) {
        if let Some(pid) = self.pid {
            let _ = std::process::Command::new("kill").arg(pid.to_string()).status();
        }
    }
}

/// **A launch shows nothing.** It comes up headed (`--headless` absent), with
/// no startup window; the host creates the profile's window born `background`
/// and moves it clear of every screen BEFORE returning; nothing is brought
/// forward, and the strip's own read says no window is up.
#[tokio::test]
async fn a_launch_brings_up_a_hidden_window_and_shows_nothing() {
    let world = world().await;

    let launched = chromium::launch_within(&world.stub, &world.profile, &[], TEST_LAUNCH_BUDGET)
        .await
        .unwrap_or_else(|why| panic!("the launch must come up: {why}"));
    let _reap = world.reap_after(launched.pid);

    let launched_with = world.launches_at_least(1).await;
    let line = launched_with.first().expect("the stub was spawned").clone();
    assert!(!line.contains("--headless"), "the launch is the person's own browser: {line}");
    assert!(line.contains("--no-startup-window"), "and it starts with no window: {line}");

    let created = world.endpoint.params_of("Target.createTarget");
    assert_eq!(created.len(), 1, "one window was created: {created:?}");
    assert_eq!(created[0]["newWindow"], json!(true), "{created:?}");
    assert_eq!(
        created[0]["background"],
        json!(true),
        "born hidden - a foreground window would surface at the person: {created:?}",
    );
    assert_eq!(
        world.endpoint.moved_to(100_000),
        1,
        "the window was moved clear of every screen before anything could drive it: {:?}",
        world.endpoint.params_of("Browser.setWindowBounds"),
    );
    assert_eq!(world.endpoint.brought_forward(), 0, "the launch raised nothing");
    assert!(
        !chromium::windowed(&world.profile).await,
        "and no window of its is up: the launch is invisible to the person",
    );
    assert!(world.profile.join("has-window").is_file(), "the birth marker is written");
}

/// **The person's show with nothing running**: a browser comes up - hidden,
/// as any launch does - and its window is then brought forward. This is the
/// strip's show, and the edge the hand-off loop passes through after every
/// answer.
#[tokio::test]
async fn a_show_with_nothing_running_brings_a_window_up() {
    let world = world().await;
    assert_eq!(chromium::read_active_port(&world.profile), None, "nothing is up");

    let shown = chromium::show_within(&world.stub, &world.profile, TEST_LAUNCH_BUDGET)
        .await
        .unwrap_or_else(|why| panic!("the person's show must bring a browser up: {why}"));
    let _reap = world.reap_after(shown.pid);

    let launched_with = world.launches_at_least(1).await;
    let line = launched_with.first().expect("the stub was spawned").clone();
    assert!(!line.contains("--headless"), "the show's launch is headed: {line}");
    assert_eq!(
        world.endpoint.moved_to(100_000),
        1,
        "the window was born hidden before the raise: {:?}",
        world.endpoint.params_of("Browser.setWindowBounds"),
    );
    let raised = world.endpoint.params_of("Browser.setWindowBounds");
    assert!(
        raised.iter().any(|params| {
            params["bounds"]["windowState"] == json!("normal")
                && params["bounds"]["left"] == json!(RAISED_AT.0)
                && params["bounds"]["top"] == json!(RAISED_AT.1)
        }),
        "the window was put into view: {raised:?}",
    );
    assert!(world.endpoint.brought_forward() >= 1, "and brought to the front");
    assert!(chromium::windowed(&world.profile).await, "the window is up");
}

/// **A window already up is moved forward, and NOTHING is relaunched.** This
/// is the #2107 property at the call level: the raise of a live launch spawns
/// no process, on the same port the sessions have been driving.
#[tokio::test]
async fn a_raise_with_a_window_up_spawns_nothing() {
    let world = world().await;
    world.launched();
    let before = chromium::read_active_port(&world.profile).expect("a live launch");

    let shown = chromium::show(&world.stub, &world.profile)
        .await
        .unwrap_or_else(|why| panic!("the raise must answer: {why}"));
    assert_eq!(shown.port, before.port, "the launch that was there, not a new one");
    assert!(
        world.launches().is_empty(),
        "nothing was spawned - the up window is the one raised: {:?}",
        world.launches(),
    );
    let raised = world.endpoint.params_of("Browser.setWindowBounds");
    assert!(
        raised.iter().any(|params| params["bounds"]["left"] == json!(RAISED_AT.0)),
        "the window was put into view: {raised:?}",
    );
    assert!(world.endpoint.brought_forward() >= 1, "and brought to the front");
    assert!(
        world.endpoint.params_of("Target.createTarget").is_empty(),
        "and nothing was created over the window that is up: {:?}",
        world.endpoint.params_of("Target.createTarget"),
    );
    assert!(chromium::windowed(&world.profile).await, "the window is up");
}

/// **A raise over a live launch with no page births the window again.** This
/// is the shape a session leaves by closing the window's last tab - the
/// browser still answering, the window standing with nothing in it - and the
/// shape the person's X has left behind (2026-10-07, the process surviving
/// with nothing to raise). The window is born on every page the last raise
/// carried, the raised one first and left the active one, and the raise then
/// lands on it rather than failing on a browser with nothing to show.
#[tokio::test]
async fn a_raise_over_a_launch_with_no_page_births_the_window_again() {
    let world = world().await;
    world.launched();
    world.endpoint.set_pages(Vec::new());
    std::fs::write(world.profile.join("windowed"), b"https://one.example\nhttps://two.example")
        .expect("the raise marker");

    let shown = chromium::show(&world.stub, &world.profile)
        .await
        .unwrap_or_else(|why| panic!("the raise must birth the window again: {why}"));
    assert_eq!(shown.port, world.port(), "raised over the launch that was there");
    assert!(world.launches().is_empty(), "nothing was spawned - the browser kept running");

    let created = world.endpoint.params_of("Target.createTarget");
    assert_eq!(created.len(), 2, "the window, then its second page: {created:?}");
    assert_eq!(created[0]["url"], json!("https://one.example"), "{created:?}");
    assert_eq!(
        created[0]["newWindow"],
        json!(true),
        "the window is born for the page the raise showed first: {created:?}",
    );
    assert_eq!(created[0]["background"], json!(true), "born hidden, as any launch's is");
    assert_eq!(created[1]["url"], json!("https://two.example"), "{created:?}");
    assert_eq!(
        created[1]["newWindow"],
        json!(false),
        "the window's other pages come back as its tabs: {created:?}",
    );
    assert_eq!(
        world.endpoint.params_of("Target.activateTarget").len(),
        1,
        "the page the raise showed is left the active one: {:?}",
        world.endpoint.params_of("Target.activateTarget"),
    );
    assert_eq!(
        world.endpoint.moved_to(100_000),
        1,
        "and the reborn window is moved clear of every screen before anything can drive it: {:?}",
        world.endpoint.params_of("Browser.setWindowBounds"),
    );
    assert!(world.endpoint.brought_forward() >= 1, "then brought forward to the person");
    assert!(chromium::windowed(&world.profile).await, "the window is up again");
    let marker = std::fs::read_to_string(world.profile.join("windowed")).expect("the marker");
    assert_eq!(
        marker, "https://one.example\nhttps://two.example",
        "and the marker still carries the window's pages",
    );
}

/// **A raise carries every page of the window into its marker**: what a later
/// launch - or a rebirth - opens the window's tabs on. The rest follow the
/// browser's own list order, which nothing trusts (it varied across runs,
/// measured 2026-10-17): the raised page leads by its target id, pinned in
/// the unit tests.
#[tokio::test]
async fn a_raise_carries_every_page_of_the_window_into_its_marker() {
    let world = world().await;
    world.launched();
    world.endpoint.set_pages(vec![
        "https://two.example".to_owned(),
        "https://one.example".to_owned(),
        "https://three.example".to_owned(),
    ]);

    chromium::show(&world.stub, &world.profile)
        .await
        .unwrap_or_else(|why| panic!("the raise must answer: {why}"));

    let marker = std::fs::read_to_string(world.profile.join("windowed")).expect("the marker");
    assert_eq!(
        marker, "https://two.example\nhttps://one.example\nhttps://three.example",
        "every page of the window rides the marker, in the browser's own order",
    );
}

/// **A launch opens the pages it is carried**: the window born for the page
/// the raise showed, the rest of the window as its tabs, so a relaunch after
/// the window went restores the window rather than the last-served page
/// alone.
#[tokio::test]
async fn a_launch_opens_the_pages_it_is_carried() {
    let world = world().await;
    let pages = vec!["https://one.example".to_owned(), "https://two.example".to_owned()];

    let launched = chromium::launch_within(&world.stub, &world.profile, &pages, TEST_LAUNCH_BUDGET)
        .await
        .unwrap_or_else(|why| panic!("the launch must come up: {why}"));
    let _reap = world.reap_after(launched.pid);

    let created = world.endpoint.params_of("Target.createTarget");
    assert_eq!(created.len(), 2, "the window and its one other page: {created:?}");
    assert_eq!(created[0]["url"], json!("https://one.example"), "{created:?}");
    assert_eq!(
        created[0]["newWindow"],
        json!(true),
        "the carried page the raise showed first is born into the window: {created:?}",
    );
    assert_eq!(created[1]["url"], json!("https://two.example"), "{created:?}");
    assert_eq!(created[1]["newWindow"], json!(false), "the rest are tabs of that window");
    assert_eq!(
        world.endpoint.params_of("Target.activateTarget").len(),
        1,
        "and the carried page is left the active one",
    );
}

/// **A raise over a page whose window is gone births the window again.** A
/// page target can outlive the window it lived in (reachable in production or
/// not, the class is closed here): `cdp.page()` answers, nothing can be
/// raised in it, and the raise must clear that page and land on the carried
/// pages instead of failing on a browser that answers and has nothing to
/// show.
#[tokio::test]
async fn a_raise_over_a_page_with_no_window_births_the_window_again() {
    let world = world().await;
    world.launched();
    world.endpoint.windowless();
    std::fs::write(world.profile.join("windowed"), b"https://one.example\nhttps://two.example")
        .expect("the raise marker");

    let shown = chromium::show(&world.stub, &world.profile)
        .await
        .unwrap_or_else(|why| panic!("the raise must birth the window again: {why}"));
    assert_eq!(shown.port, world.port(), "raised over the launch that was there");
    assert!(world.launches().is_empty(), "nothing was spawned - the browser kept running");
    assert_eq!(
        world.endpoint.params_of("Target.closeTarget").len(),
        1,
        "the windowless page was cleared first: {:?}",
        world.endpoint.params_of("Target.closeTarget"),
    );
    let created = world.endpoint.params_of("Target.createTarget");
    assert_eq!(created.len(), 2, "the window, then its second page: {created:?}");
    assert_eq!(created[0]["url"], json!("https://one.example"), "{created:?}");
    assert_eq!(created[0]["newWindow"], json!(true), "{created:?}");
    assert_eq!(created[1]["url"], json!("https://two.example"), "{created:?}");
    assert_eq!(created[1]["newWindow"], json!(false), "the rest are tabs of it: {created:?}");
    assert!(world.endpoint.brought_forward() >= 1, "and the reborn window is brought forward");
    assert!(chromium::windowed(&world.profile).await, "the window is up again");
}

/// **A window is the marker AND a live launch AND a window answering for a
/// page** - the same predicate `show` keeps. The marker alone lied once (a
/// person's X left the keep-alive process with the marker set and no page);
/// a page alone is not a window either - a page target can outlive the window
/// it lived in, and anything that SPEAKS for a window - the strip's mark and
/// its hide label - must not trust either alone.
#[tokio::test]
async fn a_window_is_the_marker_a_live_launch_and_a_window() {
    let world = world().await;
    assert!(!chromium::windowed(&world.profile).await, "no marker, no window");

    std::fs::write(world.profile.join("windowed"), b"").expect("the marker");
    assert!(
        !chromium::windowed(&world.profile).await,
        "a marker with nothing answering is no window",
    );

    // The launch answering, the marker set, no page at all.
    world.launched();
    world.endpoint.set_pages(Vec::new());
    assert!(!chromium::windowed(&world.profile).await, "no page is not a window");

    // A page left, but no window under it: still not a window.
    world.endpoint.set_pages(vec!["https://example.com/prepared".to_owned()]);
    world.endpoint.windowless();
    assert!(
        !chromium::windowed(&world.profile).await,
        "a page whose window is gone is not a window - nothing can be raised in it",
    );

    world.endpoint.born_hidden();
    assert!(
        chromium::windowed(&world.profile).await,
        "the marker, a launch answering as it, and a window for a page make a window",
    );
}

/// **A tab closed after the raise stops being carried.** The marker is not a
/// freeze-frame of the raise: a call that changes the window rewrites it, so
/// a tab a session closed itself does not resurrect on the next launch.
#[tokio::test]
async fn a_tab_closed_after_the_raise_stops_being_carried() {
    let world = world().await;
    world.launched();
    std::fs::write(world.profile.join("windowed"), b"https://one.example\nhttps://two.example")
        .expect("the raise marker");
    world.endpoint.set_pages(vec!["https://one.example".to_owned()]);

    chromium::note_pages(&world.profile, None).await;

    let marker = std::fs::read_to_string(world.profile.join("windowed")).expect("the marker");
    assert_eq!(marker, "https://one.example", "the closed tab is gone from the carry");
}

/// **A page a call named leads the carry**: a navigation's own URL is the
/// page the session was working on, wherever the browser's list happens to
/// put it.
#[tokio::test]
async fn a_named_page_leads_the_carry() {
    let world = world().await;
    world.launched();
    std::fs::write(world.profile.join("windowed"), b"https://one.example\nhttps://two.example")
        .expect("the raise marker");
    world
        .endpoint
        .set_pages(vec!["https://one.example".to_owned(), "https://two.example".to_owned()]);

    chromium::note_pages(&world.profile, Some("https://two.example")).await;

    let marker = std::fs::read_to_string(world.profile.join("windowed")).expect("the marker");
    assert_eq!(
        marker, "https://two.example\nhttps://one.example",
        "the named page leads, the window's other pages follow",
    );
}

/// **Blank pages carry nothing**: `about:blank` is a fresh tab, not a page
/// anyone was looking at.
#[tokio::test]
async fn a_blank_page_is_not_carried() {
    let world = world().await;
    world.launched();
    std::fs::write(world.profile.join("windowed"), b"about:blank").expect("a marker");
    world.endpoint.set_pages(vec!["about:blank".to_owned(), "https://one.example".to_owned()]);

    chromium::note_pages(&world.profile, None).await;

    let marker = std::fs::read_to_string(world.profile.join("windowed")).expect("the marker");
    assert_eq!(marker, "https://one.example", "the blank is not carried");
}

/// **No window up, no rewrite**: the marker means "the window is up", and a
/// refresh cannot raise one - a call on a browser whose window is not up
/// leaves the carry exactly as it was.
#[tokio::test]
async fn a_refresh_without_a_window_writes_nothing() {
    let world = world().await;
    world.launched();
    world.endpoint.set_pages(vec!["https://one.example".to_owned()]);
    assert!(!world.profile.join("windowed").exists(), "no raise, no marker");

    chromium::note_pages(&world.profile, Some("https://one.example")).await;

    assert!(!world.profile.join("windowed").exists(), "a refresh with no window up writes nothing",);
}

/// **A hide lowers the window and leaves the browser running.** The heart of
/// #2107: Done or Not now takes the window out of sight - the process and its
/// page all stay - and the marker the strip reads comes down with it.
#[tokio::test]
async fn a_hide_lowers_the_window_and_leaves_the_browser_running() {
    let world = world().await;
    world.launched();
    // Raised, as it is when the person is acting on it.
    std::fs::write(world.profile.join("windowed"), b"https://example.com/prepared")
        .expect("the raise marker");

    chromium::hide(&world.profile).await;
    assert!(
        chromium::probe(world.port()).await,
        "the browser is still answering - the hide closed nothing",
    );
    assert_eq!(
        world.endpoint.moved_to(100_000),
        1,
        "the window went back out of the person's view: {:?}",
        world.endpoint.params_of("Browser.setWindowBounds"),
    );
    assert!(!chromium::windowed(&world.profile).await, "and it is not up any more");
    assert!(
        !world.profile.join("windowed").exists(),
        "the raise marker came down - the strip's button says show again",
    );
}

/// **A stale raise marker does not stand a show down.** Nothing answers on
/// the port, whatever a marker says: the person's show brings a browser up.
#[tokio::test]
async fn a_show_over_a_stale_marker_still_brings_a_window_up() {
    let world = world().await;
    // A marker from a raise, and a port file whose port nothing answers on.
    std::fs::write(world.profile.join("windowed"), b"https://example.com/prepared")
        .expect("the raise marker");
    let dead = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
    let dead_port = dead.local_addr().expect("the address").port();
    drop(dead);
    std::fs::write(
        world.profile.join("DevToolsActivePort"),
        format!("{dead_port}\n/devtools/browser/gone\n"),
    )
    .expect("the stale port file");

    let shown = chromium::show_within(&world.stub, &world.profile, TEST_LAUNCH_BUDGET)
        .await
        .unwrap_or_else(|why| panic!("the show must bring a browser up: {why}"));
    let _reap = world.reap_after(shown.pid);
    assert!(!world.launches().is_empty(), "a fresh launch came up under the stale state",);
    assert!(chromium::windowed(&world.profile).await, "and its window is up");
}

/// **The launch's wait is the budget it is given.** A stub that holds its
/// port file back past a small budget is refused by name, and the same stub
/// is waited out by the budget these tests use - which is the seam the five
/// cases above rest on, the wait belonging to the machine rather than to
/// the launch.
#[tokio::test]
async fn a_launch_is_bounded_by_the_budget_it_is_given() {
    let refused_world = world().await;
    refused_world.stub_delay(Duration::from_secs(2));
    let refused = chromium::launch_within(
        &refused_world.stub,
        &refused_world.profile,
        &[],
        Duration::from_secs(1),
    )
    .await;
    let Err(why) = refused else {
        panic!(
            "a launch whose budget runs out before the child answers is refused, not waited out"
        );
    };
    assert!(why.contains("within 1 s"), "and the refusal names the budget it ran out of: {why}");

    let waited_world = world().await;
    waited_world.stub_delay(Duration::from_secs(2));
    let launched =
        chromium::launch_within(&waited_world.stub, &waited_world.profile, &[], TEST_LAUNCH_BUDGET)
            .await
            .unwrap_or_else(|why| panic!("the tests' budget must wait that stub out: {why}"));
    let _reap = waited_world.reap_after(launched.pid);
    assert!(
        waited_world.profile.join("has-window").is_file(),
        "and what it waited out is a whole launch",
    );
    assert!(
        TEST_LAUNCH_BUDGET >= Duration::from_secs(30),
        "the tests' budget must stay far past the shell's own 15 s: a loaded machine held a \
         launch's port file back for 13 s (measured 2026-10-14), which is the stall this seam \
         exists to keep out of the suite",
    );
}
