//! The server: it binds what the config says, serves the home, and binds
//! nothing at all when it is turned off.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::path::Path;
use std::sync::Arc;

use forge_primitives::WebConfig;
use forge_sessions::surface::ViewSurface;
use forge_sessions::testing::Fleet;
use forge_web::WebState;

/// A port to hand the server: bind one, read it, let it go. Something
/// else can take it in the gap before the server binds, which is why
/// `start_on_a_free_port` retries rather than trusting this.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let port = listener.local_addr().expect("the ephemeral address").port();
    drop(listener);
    port
}

/// A fleet on disk: two orgs, one live project with a worker and a task,
/// one live project with nothing under it, and a dormant project.
fn fleet(dir: &Path) -> Fleet {
    let fleet =
        Fleet::in_dir(dir, &[("Busytools", &["forge", "busymail"]), ("Personal", &["dotfiles"])])
            .expect("the fleet builds");
    fleet.start("Busytools", "forge").expect("forge is declared");
    fleet.add_worker("Busytools", "forge", "em-dash-sweep").expect("forge is declared");
    fleet
        .add_task("Busytools", "forge", "Sweep the corpus for banned dashes", "lead")
        .expect("forge is declared");
    fleet.start("Busytools", "busymail").expect("busymail is declared");
    fleet
}

async fn start(bind: IpAddr, surface: Arc<ViewSurface>) -> (SocketAddr, WebConfig) {
    start_on_a_free_port_with(bind, surface, std::convert::identity).await
}

/// [`start`] with the config adjusted before it is handed over, so a test
/// can set a key without giving up the retry.
async fn start_on_a_free_port_with(
    bind: IpAddr,
    surface: Arc<ViewSurface>,
    adjust: impl Fn(WebConfig) -> WebConfig,
) -> (SocketAddr, WebConfig) {
    for _ in 0..8 {
        let config = adjust(WebConfig { port: free_port(), bind, ..WebConfig::default() });
        let state = WebState::new(
            Arc::clone(&surface),
            Arc::new(forge_web::WorkCache::new()),
            config.clone(),
        );
        match forge_web::start(state).await {
            Ok(Some(bound)) => return (bound, config),
            Ok(None) => panic!("an enabled config must not come back disabled"),
            // A stolen probe port: take another and try again.
            Err(_) => {}
        }
    }
    panic!("no free port after eight tries");
}

/// Open the page's stream, which stays open.
async fn open_stream(config: &WebConfig) -> reqwest::Response {
    let url = format!("http://127.0.0.1:{}/events", config.port);
    let response = tokio::time::timeout(std::time::Duration::from_secs(5), reqwest::get(url))
        .await
        .expect("the stream opens within five seconds")
        .expect("the stream is served");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let content_type = response.headers()["content-type"].to_str().expect("a readable type");
    assert!(content_type.starts_with("text/event-stream"), "got: {content_type}");
    response
}

/// Stream chunks, read until they carry `needle`: an SSE endpoint that
/// never writes it is the bug this catches.
async fn event_carrying(response: reqwest::Response, needle: &str) -> String {
    let what = format!("a chunk carrying {needle:?}");
    read_until(response, &what, &|seen| seen.contains(needle)).await
}

/// Stream chunks, read until the stream has carried two `fleet` events.
async fn two_fleet_events(response: reqwest::Response) -> String {
    read_until(response, "a second fleet event", &|seen| seen.matches("event: fleet").count() >= 2)
        .await
}

/// Read until `enough` is satisfied, or fail naming what was awaited and
/// which stream it never arrived on. A stall and a wrong needle read the
/// same without both.
async fn read_until(
    response: reqwest::Response,
    what: &str,
    enough: &dyn Fn(&str) -> bool,
) -> String {
    use futures_util::StreamExt;
    let url = response.url().clone();
    let mut stream = response.bytes_stream();
    let mut seen = String::new();
    while !enough(&seen) {
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .unwrap_or_else(|_| panic!("waited five seconds for {what} from {url}, saw: {seen}"))
            .expect("the stream yields")
            .expect("the chunk reads");
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }
    seen
}

async fn get(config: &WebConfig, path: &str) -> (reqwest::StatusCode, String, String) {
    let response =
        reqwest::get(format!("http://127.0.0.1:{}{path}", config.port)).await.expect("served");
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .map_or(String::new(), |value| value.to_str().expect("a readable content type").to_owned());
    (status, content_type, response.text().await.expect("the body reads"))
}

/// The Klin path, verbatim from the sheet the marks were picked from. A
/// literal rather than a call into the crate, so a redrawn or mistyped
/// path fails here instead of agreeing with itself.
const KLIN_PATH: &str = "M5 3h14a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2Zm2.5 \
                         19v-8.5a4.5 4.5 0 0 1 9 0V22h-9Z";

const LANES_BARS: &str = "x=\"10\" y=\"3\" width=\"4\" height=\"18\"";

/// Every interface rather than loopback, so the address has to come
/// from the config: loopback is what a hardcoded one would look like.
#[tokio::test]
async fn serves_on_the_configured_address() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (bound, config) = start(IpAddr::V4(Ipv4Addr::UNSPECIFIED), fleet.surface()).await;
    assert_eq!(
        bound,
        SocketAddr::new(config.bind, config.port),
        "the listener bound the configured address, not a default",
    );

    let (status, _content_type, body) = get(&config, "/").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(
        body.contains(&format!("this page is served on {bound}")),
        "the page reports the address it bound, got: {body}",
    );
}

/// The page a browser opens: one header per org, every project under it,
/// and the fleet count in the header line.
#[tokio::test]
async fn the_home_lists_every_project_under_its_org() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (status, content_type, page) = get(&config, "/").await;

    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(content_type.starts_with("text/html"), "a browser renders it as a page");
    for org in ["Busytools", "Personal"] {
        assert!(page.contains(&format!(">{org}<")), "one header per org, missing {org}: {page}");
    }
    // Not "forge": the wordmark and the title carry that word, so a row
    // that went missing would still pass. These two appear nowhere else.
    for project in ["busymail", "dotfiles"] {
        assert!(page.contains(project), "every project is listed, missing {project}: {page}");
    }
    assert!(page.contains("3 projects"), "the header carries the project count: {page}");
    assert!(page.contains("em-dash-sweep"), "a project's workers hang under it: {page}");
    assert!(
        page.contains("Sweep the corpus for banned dashes"),
        "and its task is what the row says it is doing: {page}",
    );
}

/// A quiet morning: projects configured, none of them started. Every row
/// is a dormant one and the fleet count is zero, and the page is still a
/// page.
#[tokio::test]
async fn a_fleet_with_nothing_started_still_draws_every_row() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Personal", &["dotfiles", "fitness"])])
        .expect("the fleet builds");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (status, _content_type, page) = get(&config, "/").await;

    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(page.contains(">Personal<"), "the org is still there: {page}");
    for project in ["dotfiles", "fitness"] {
        assert!(page.contains(project), "and so is {project}: {page}");
    }
    assert!(
        page.contains("<span class=\"n\">0</span> agents"),
        "a fleet with nothing started counts zero: {page}",
    );
    assert!(page.contains("2 asleep"), "and the org says so: {page}");
}

/// A second tab watches beside the first: both subscribers see an update
/// emitted after both attached, rather than splitting the stream between
/// them. Catches a stream that hands every connection the same receiver.
///
/// Every connection opens with the region it should be showing, so the
/// tell is a *second* `fleet` event on each: the opening one, and the one
/// the emit caused. No tick can account for it inside the read window.
#[tokio::test]
async fn two_subscribers_both_see_an_update() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    // Both attached: the response arrives once the handler has taken its
    // own receiver, so nothing emitted after this reaches one of them only.
    let first = open_stream(&config).await;
    let second = open_stream(&config).await;

    fleet.emit(forge_sessions::SessionUpdate::CatalogLoaded);

    let (a, b) = tokio::join!(two_fleet_events(first), two_fleet_events(second));

    assert_eq!(a.matches("event: fleet").count(), 2, "the first tab is sent it once: {a}");
    assert_eq!(b.matches("event: fleet").count(), 2, "and so is the second: {b}");
}

/// The view folds the stream whether or not a tab is attached, which is
/// the case the diamond exists for: a turn that completes while the page
/// is closed is exactly "finished and you have not looked at it". Catches
/// a fold that only happens inside a connection's own task.
#[tokio::test]
async fn a_completion_while_no_tab_is_open_earns_its_diamond() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    // No stream is opened at all: the page has never been loaded.
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    fleet.emit(forge_sessions::SessionUpdate::ChatAppended {
        key: forge_primitives::SessionSlot::lead("Busytools", "forge"),
        msg: serde_json::from_value(serde_json::json!({
            "type": "result",
            "subtype": "success",
            "duration_ms": 1,
            "duration_api_ms": 1,
            "is_error": false,
            "num_turns": 1,
            "session_id": "s",
        }))
        .expect("a result message"),
    });
    // The fold runs in a task of its own. Two yields rather than one: the
    // first lets the fold task wake from `recv`, the second lets it run
    // to the apply and back to its await, which is the state the request
    // below reads. A third would change nothing - the fold has no other
    // await between those two points.
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    let (_status, _content_type, page) = get(&config, "/").await;

    assert!(
        page.contains("class=\"row unseen\""),
        "a turn that finished while the page was closed is the diamond: {page}",
    );
}

/// The stream says what the region should be the moment it attaches. The
/// page is rendered before its stream opens, and a sleeping laptop or a
/// backgrounded tab reconnects much later, so an update landing in either
/// gap would otherwise leave the page stale until the next one.
#[tokio::test]
async fn the_stream_opens_with_the_region() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let opening = event_carrying(open_stream(&config).await, "this page is served on").await;

    assert!(opening.contains("event: fleet"), "the first event is the region: {opening}");
    assert!(
        opening.contains("class=\"wrap\" id=\"home\""),
        "which is what the page swaps in: {opening}",
    );
    assert!(
        !opening.contains("<html"),
        "and not the document, which would nest a second wrap: {opening}",
    );
}

/// A project that has run and stopped is asleep, not never-started. The
/// mock draws the two differently, and a forge restart leaves every
/// project with no live session, so reading the lifecycle alone makes the
/// whole fleet look like it has never run.
#[tokio::test]
async fn a_project_that_has_run_and_stopped_reads_asleep() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet =
        Fleet::in_dir(dir.path(), &[("Personal", &["dotfiles", "fitness"])]).expect("the fleet");
    fleet.record_session("dotfiles", "s-1").expect("dotfiles is declared");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (status, _content_type, page) = get(&config, "/").await;

    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(
        page.contains("class=\"row asleep\""),
        "a project that ran and stopped is asleep: {page}",
    );
    assert!(page.contains("class=\"row never\""), "and one that has never run is not: {page}");
}

/// The page's own scripts, served from the process: a browser that cannot
/// load them has a page that never updates, and nothing on the page itself
/// would say so. The markers are strings each library defines.
#[tokio::test]
async fn the_vendored_scripts_are_served() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    for (path, marker) in [
        ("/vendor/htmx.js", "htmx"),
        ("/vendor/htmx-sse.js", "sse"),
        ("/vendor/idiomorph.js", "Idiomorph"),
    ] {
        let response =
            reqwest::get(format!("http://127.0.0.1:{}{path}", config.port)).await.expect("served");
        assert_eq!(response.status(), reqwest::StatusCode::OK, "{path} is served");
        assert_eq!(
            response.headers()["cache-control"],
            "no-cache",
            "{path} must not be held stale by a browser",
        );
        let content_type = response.headers()["content-type"].to_str().expect("readable");
        assert!(content_type.starts_with("text/javascript"), "{path} is a script");
        let body = response.text().await.expect("the body reads");
        assert!(body.contains(marker), "{path} carries {marker}, got {} bytes", body.len());
    }

    let (status, _content_type, _body) = get(&config, "/vendor/absent.js").await;
    assert_eq!(
        status,
        reqwest::StatusCode::NOT_FOUND,
        "a name that is not vendored says so rather than serving an empty script",
    );
}

/// The stylesheet is served beside the page, as a stylesheet.
#[tokio::test]
async fn the_home_serves_its_stylesheet() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (status, content_type, body) = get(&config, "/home.css").await;

    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(content_type.starts_with("text/css"), "a browser reads it as a stylesheet");
    assert!(body.contains(".row"), "the stylesheet carries the row: {body}");
}

/// The mark a browser tab carries comes from `[web] mark`, so a mark
/// chosen in `forge.toml` is the one on the tab. Catches a route that
/// hardcodes the built-in, and one that serves the mark without a type a
/// browser will draw.
#[tokio::test]
async fn the_favicon_serves_the_configured_mark_in_the_palette() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start_on_a_free_port_with(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        fleet.surface(),
        |mut config| {
            config.mark = Some("lanes".to_owned());
            config
        },
    )
    .await;

    let (status, content_type, body) = get(&config, "/favicon.svg").await;

    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(content_type, "image/svg+xml", "a browser draws it as an image, not as text");
    assert!(
        body.contains(LANES_BARS),
        "the favicon carries the mark the config picked, got: {body}",
    );
    assert!(
        body.contains("color=\"#f47600\""),
        "and draws it in the palette's accent, got: {body}",
    );
}

/// Unset keys are the built-ins: Klin on the tab, and the dark palette
/// as the page's own root variables rather than a stylesheet's copy of
/// it.
#[tokio::test]
async fn unset_names_fall_back_to_the_built_in_mark_and_palette() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (status, _content_type, favicon) = get(&config, "/favicon.svg").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(favicon.contains(KLIN_PATH), "the default mark is Klin, got: {favicon}");

    let (status, _content_type, page) = get(&config, "/").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(
        page.contains("--accent:#f47600"),
        "the page carries the palette as its root variables, got: {page}",
    );
}

/// A port something else holds is an error rather than a silent no-op:
/// the caller is what tells the user the view is not serving, and an
/// on-by-default listener that loses a port fight has to say so.
#[tokio::test]
async fn a_taken_port_is_an_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let holder = TcpListener::bind("127.0.0.1:0").expect("hold a port");
    let port = holder.local_addr().expect("the held address").port();
    let config = WebConfig { port, bind: IpAddr::V4(Ipv4Addr::LOCALHOST), ..WebConfig::default() };
    let state = WebState::new(fleet.surface(), Arc::new(forge_web::WorkCache::new()), config);

    let error = forge_web::start(state).await.expect_err("a taken port must not pass as bound");

    assert!(
        error.to_string().contains(&format!("127.0.0.1:{port}")),
        "the error names the address it could not bind, got: {error}",
    );
}

/// The port is held for the whole test, so a view that tried to bind it
/// would come back with an error: `Ok(None)` is what says it never tried.
/// No window for another process to take the port, and no listening check
/// to be right about for the wrong reason.
#[tokio::test]
async fn disabled_binds_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let holder = TcpListener::bind("127.0.0.1:0").expect("hold a port");
    let port = holder.local_addr().expect("the held address").port();
    let state = WebState::new(
        fleet.surface(),
        Arc::new(forge_web::WorkCache::new()),
        WebConfig {
            enabled: false,
            port,
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            ..WebConfig::default()
        },
    );

    let bound = forge_web::start(state).await.expect("turning it off is not an error");

    assert!(bound.is_none(), "a disabled server binds nothing");
}
