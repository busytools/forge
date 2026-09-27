//! The server: it binds what the config says, serves the home, and binds
//! nothing at all when it is turned off.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::path::Path;
use std::sync::Arc;

use forge_primitives::SessionSlot;
use forge_primitives::WebConfig;
use forge_sessions::surface::{PendingKind, ViewSurface};
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
    open_stream_at(config, "/events").await
}

/// [`open_stream`] for a stream that is not the fleet's: a session page has
/// one of its own.
async fn open_stream_at(config: &WebConfig, path: &str) -> reqwest::Response {
    let url = format!("http://127.0.0.1:{}{path}", config.port);
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
        body.contains(&bound.to_string()),
        "the band reports the address it bound, got: {body}",
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

/// The header's claude version is the core's answer rather than anything
/// the request carried, so the page the server renders states it.
#[tokio::test]
async fn the_home_names_the_claude_version_the_core_holds() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    fleet.set_cli_version(Some("2.1.156"), Some("2.1.201"));
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (status, _content_type, page) = get(&config, "/").await;

    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(page.contains("claude 2.1.156"), "the header names the installed version: {page}");
    assert!(
        page.contains("\u{2191} v2.1.201 available"),
        "and the available one, which is what this page is for: {page}",
    );
}

/// The orgs read alphabetically rather than in the order `forge.toml`
/// happens to declare them. The fixture names Personal first, so a page
/// that kept declaration order fails this.
#[tokio::test]
async fn the_orgs_read_alphabetically() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet =
        Fleet::in_dir(dir.path(), &[("Personal", &["dotfiles"]), ("Busytools", &["forge"])])
            .expect("the fleet builds");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_, _, page) = get(&config, "/").await;

    let busytools = page.find(">Busytools<").expect("Busytools is on the page");
    let personal = page.find(">Personal<").expect("Personal is on the page");
    assert!(
        busytools < personal,
        "declared Personal first, so this fails if declaration order survives: {page}",
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

    let opening = event_carrying(open_stream(&config).await, "id=\"home\"").await;

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

/// The stylesheet is served beside the page, as a stylesheet, and it is
/// the one sheet both pages link: a second copy of a row or a mark is the
/// defect rule 21 names.
#[tokio::test]
async fn the_pages_serve_the_one_stylesheet() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (status, content_type, body) = get(&config, "/web.css").await;

    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(content_type.starts_with("text/css"), "a browser reads it as a stylesheet");
    assert!(body.contains(".row"), "the sheet carries the home's row: {body}");
    assert!(body.contains(".rail"), "and the session page's rail: {body}");
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

/// The route serves a real page for a real slot, with all three columns.
#[tokio::test]
async fn the_session_page_serves_all_three_columns() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (status, content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(content_type.starts_with("text/html"), "a browser renders it as a page");
    assert!(page.contains("projects"), "the rail renders: {page}");
    assert!(page.contains("inspector"), "the inspector renders: {page}");
    assert!(page.contains("/web.css"), "and the page links the one stylesheet: {page}");
}

/// An unknown slot is a 404, not an empty shell that looks like a session.
/// A shell would read as a session with nothing in it, which is a wrong
/// answer rather than a missing one.
#[tokio::test]
async fn an_unknown_slot_is_not_found() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (status, _content_type, _page) = get(&config, "/session/Nobody/nothing/lead").await;
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND, "an unknown slot is a 404");

    // The project is declared and the seat it names is not one of its own:
    // a project's roster names its lead and its workers, and nobody else.
    let (status, _content_type, _page) = get(&config, "/session/Busytools/forge/cli-version").await;
    assert_eq!(
        status,
        reqwest::StatusCode::NOT_FOUND,
        "a label the project's roster does not hold is a 404 like any other unknown seat",
    );
}

/// A slot in the roster whose lead is not running opens its page. It is NOT
/// a 404: the seat exists, its occupant does not, and the page says which of
/// the two it is rather than claiming a connection nothing is making.
#[tokio::test]
async fn a_sleeping_slot_opens_rather_than_404ing() {
    let dir = tempfile::tempdir().expect("tempdir");
    // dotfiles is declared and nothing has ever run in it: the sleeping
    // project the home draws a Start row for.
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (status, _content_type, page) = get(&config, "/session/Personal/dotfiles/lead").await;

    assert_eq!(status, reqwest::StatusCode::OK, "a sleeping seat renders, it does not 404");
    assert!(page.contains("not running"), "and says what the seat is: {page}");
    assert!(!page.contains("class=\"kind\""), "and draws no chat skeleton under it: {page}");
    assert!(
        !page.contains("connecting"),
        "without claiming a connection nothing is making: {page}"
    );
}

/// A seat whose spawn failed carries the reason the core recorded, which is
/// the same diagnostic the home renders: a failure mark over a page that
/// says nothing about the failure is a page that lost it.
#[tokio::test]
async fn a_failed_seat_carries_the_reason_the_core_recorded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Busytools", &["forge"])]).expect("the fleet builds");
    fleet.start("Busytools", "forge").expect("forge is declared");
    fleet.fail_spawn("Busytools", "forge", "lead", "the subprocess exited");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert_eq!(status, reqwest::StatusCode::OK, "a failed seat is still its own page");
    assert!(page.contains("the subprocess exited"), "carrying the reason: {page}");
}

/// The home's project rows link here. Without this the page has no entry
/// point, and a page nothing points at is one nobody opens.
#[tokio::test]
async fn the_home_links_to_the_session_page() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, page) = get(&config, "/").await;

    assert!(
        page.contains("/session/Busytools/forge/lead"),
        "a project's lead row links into the page: {page}",
    );
    assert!(
        page.contains("/session/Busytools/forge/em-dash-sweep"),
        "and so does a worker's row, under its own label: {page}",
    );
    assert!(
        page.contains("/session/Personal/dotfiles/lead"),
        "including the one a project nobody has started carries: {page}",
    );
}

/// The three groups render in order, and a needs-you row says what it is
/// waiting on: the reason is what makes the row actionable without opening
/// it, and the mark alone only says that something is wrong.
#[tokio::test]
async fn the_rail_groups_by_state_and_names_what_is_pending() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let lead = SessionSlot::lead("Busytools", "forge");
    fleet.seed_test_pending_interaction(&lead, PendingKind::Question);
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    let needs = page.find("needs you").expect("the needs-you group renders");
    let working = page.find("working").expect("the working group renders");
    let asleep = page.find("asleep").expect("the asleep group renders");
    assert!(needs < working, "needs-you precedes working: {page}");
    assert!(working < asleep, "and working precedes asleep: {page}");
    assert!(page.contains("asked you a question"), "and the row names what it waits on: {page}");
}

/// A worker row and a lead row are the same object, so both carry a close
/// chip: a chip the sheet styles for one and not the other is the defect
/// this pins.
#[tokio::test]
async fn a_worker_row_carries_the_same_close_chip_as_a_lead() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Busytools", &["forge"])]).expect("the fleet builds");
    fleet.start("Busytools", "forge").expect("forge is declared");
    fleet.add_worker("Busytools", "forge", "cli-version").expect("forge is declared");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert!(page.contains("cli-version"), "the worker's row renders: {page}");
    assert_eq!(
        page.matches("class=\"x\"").count(),
        2,
        "the lead and its worker each carry one close chip: {page}",
    );
}

/// The rail marks the session the page is showing, so a reader can see
/// where in the fleet they are.
#[tokio::test]
async fn the_rail_marks_the_current_project() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, busymail) = get(&config, "/session/Busytools/busymail/lead").await;
    let (_status, _content_type, forge) = get(&config, "/session/Busytools/forge/lead").await;

    assert!(
        busymail.contains("class=\"pj cur\""),
        "the project the page shows carries the current mark: {busymail}",
    );
    assert!(
        forge.contains("class=\"pj cur\""),
        "and so does another's when it is the one being shown: {forge}",
    );
    assert_eq!(
        busymail.matches("class=\"pj cur\"").count(),
        1,
        "exactly one project is the current one: {busymail}",
    );
}

/// A section renders its summary from real data, and the summary is a
/// fact rather than a label: a git section with eight changed files says
/// eight.
#[tokio::test]
async fn the_inspector_summarises_from_the_core() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Busytools", &["forge"])]).expect("the fleet builds");
    fleet.start("Busytools", "forge").expect("forge is declared");
    repo_with(dir.path().join("forge").as_path(), 8);
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert!(
        page.contains("<span class=\"c2\">main \u{b7} 8 files</span>"),
        "the section renders with its summary: {page}",
    );
    assert!(
        page.contains("<details class=\"sec\" open>"),
        "and opens on the changes it holds: {page}",
    );
    assert!(page.contains("file-0.txt"), "and the body lists the files behind that count: {page}");
}

/// A section with nothing behind it renders without inventing content. A
/// fixture with no connectors configured must not report one connected.
#[tokio::test]
async fn an_empty_section_does_not_invent_content() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Busytools", &["forge"])]).expect("the fleet builds");
    fleet.start("Busytools", "forge").expect("forge is declared");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert!(!page.contains("workspaces"), "no slack content without slack: {page}");
    assert!(!page.contains("connected"), "and nothing claims a connection: {page}");
}

/// The tasks and schedules sections read the project's own store rows, so
/// what the core holds is what the page lists, under the section's own
/// counting summary.
#[tokio::test]
async fn the_inspector_lists_the_projects_tasks_and_schedules() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    fleet.add_cron("forge", "stand-up").expect("forge is declared");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert!(
        page.contains("Sweep the corpus for banned dashes"),
        "the task the store holds is listed: {page}",
    );
    assert!(page.contains("of 1"), "under its own done-of-total summary: {page}");
    assert!(page.contains("stand-up"), "and so is the project's cron: {page}");
    assert!(page.contains("recurring"), "named for what kind it is: {page}");
}

/// The files are grouped by directory even though the scan returns them
/// ordered by how much each changed: a directory's heading is drawn once,
/// wherever in that order its files land.
#[tokio::test]
async fn the_git_section_groups_a_directory_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Busytools", &["forge"])]).expect("the fleet builds");
    fleet.start("Busytools", "forge").expect("forge is declared");
    // `a/` holds two files of different sizes with `b/`'s between them, so
    // the scan's size order is a/wide, b/mid, a/narrow.
    repo_of_sizes(
        dir.path().join("forge").as_path(),
        &[("a/wide.txt", 9), ("b/mid.txt", 5), ("a/narrow.txt", 1)],
    );
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert_eq!(
        page.matches("<div class=\"dir\">a/</div>").count(),
        1,
        "a directory's heading is drawn once: {page}",
    );
    for file in ["wide.txt", "mid.txt", "narrow.txt"] {
        assert!(page.contains(file), "{file} is listed: {page}");
    }
}

/// The section's summary counts what its own body lists, so a file the
/// body does not carry cannot inflate the count into a truncation note
/// that is not true.
#[tokio::test]
async fn the_git_section_summary_counts_what_it_lists() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Busytools", &["forge"])]).expect("the fleet builds");
    fleet.start("Busytools", "forge").expect("forge is declared");
    let repo = dir.path().join("forge");
    repo_with(repo.as_path(), 3);
    // Untracked, so the working tree's own count includes it and the diff
    // the section lists cannot.
    std::fs::write(repo.join("untracked.txt"), "new\n").expect("write");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert!(page.contains("3 files"), "the summary counts the three it lists: {page}");
    assert!(!page.contains("4 files"), "and not the untracked one it cannot: {page}");
    assert_eq!(page.matches("<div class=\"file\">").count(), 3, "three files are listed: {page}");
}

/// A clean tree with no pull request has nothing to open on, so the section
/// leads the inspector closed rather than open and empty.
#[tokio::test]
async fn a_git_section_with_nothing_behind_it_starts_closed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Busytools", &["forge"])]).expect("the fleet builds");
    fleet.start("Busytools", "forge").expect("forge is declared");
    let repo = dir.path().join("forge");
    repo_with(repo.as_path(), 3);
    git(repo.as_path(), &["checkout", "--", "."]);
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert!(
        page.contains("href=\"#i-git\""),
        "the section is still there, with its own icon: {page}",
    );
    assert!(
        !page.contains("<details class=\"sec\" open>"),
        "and carries nothing to open on: {page}",
    );
}

/// The mockup, read here rather than by a human: it is the specification the
/// page is built from, and a pin against it is the mechanical form of "the
/// mockup wins".
const MOCK: &str = include_str!("../../../docs/mockups/web-session.html");

/// The `<symbol>` elements of a document, by id, carrying their attribute
/// text. Enough to catch a hand-copied sprite drifting: a path or an
/// attribute that differs fails, and the id names which one.
fn symbols(html: &str) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    for chunk in html.split("<symbol ").skip(1) {
        let Some((attrs, _)) = chunk.split_once("</symbol>") else {
            continue;
        };
        let Some(id) = attrs.split("id=\"").nth(1).and_then(|rest| rest.split('"').next()) else {
            continue;
        };
        out.insert(id.to_owned(), attrs.trim_end().to_owned());
    }
    out
}

/// The `--fs-*` tokens a stylesheet declares, by name.
fn scale_tokens(css: &str) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    let without_comments = css
        .split("/*")
        .enumerate()
        .map(|(nth, chunk)| {
            if nth == 0 {
                chunk.to_owned()
            } else {
                chunk.split_once("*/").map_or(String::new(), |(_, rest)| rest.to_owned())
            }
        })
        .collect::<String>();
    for declaration in without_comments.split(';') {
        let Some((key, value)) = declaration.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if key.starts_with("--fs-") {
            out.insert(key.to_owned(), value.trim().to_owned());
        }
    }
    out
}

/// Every symbol the mockup defines is the symbol the page draws, attribute
/// for attribute. The sprite is a hand-copied block with nothing else
/// comparing it, which is how a stroke width drifted once already.
#[tokio::test]
async fn the_sprite_is_the_mocks_sprite() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;
    let (_status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    let want = symbols(MOCK);
    let got = symbols(&page);

    assert!(!want.is_empty(), "the mockup's sprite is what this pins the page against");
    assert_eq!(got.len(), want.len(), "the page draws every symbol the mockup defines");
    for (id, attrs) in &want {
        assert_eq!(got.get(id).map(String::as_str), Some(attrs.as_str()), "the page's {id}");
    }
}

/// The sheet's type scale is the mockup's, token for token: the scale is
/// what a page's text says a thing is, and the mockup is where it was
/// settled.
#[tokio::test]
async fn the_type_scale_is_the_mocks() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;
    let (_status, _content_type, sheet) = get(&config, "/web.css").await;

    let want = scale_tokens(MOCK);
    let got = scale_tokens(&sheet);

    assert!(!want.is_empty(), "the mockup declares the scale this pins the sheet against");
    for (token, value) in &want {
        assert_eq!(got.get(token).map(String::as_str), Some(value.as_str()), "the sheet's {token}");
    }
}

/// The declarations a stylesheet makes for one selector, whitespace
/// collapsed so the mockup's formatting and the sheet's compare.
fn declarations_for(css: &str, selector: &str) -> String {
    let at =
        css.find(&format!("{selector} {{")).unwrap_or_else(|| panic!("no rule for {selector}"));
    let rest = &css[at..];
    let open = rest.find('{').expect("the block opens");
    let close = rest[open..].find('}').expect("the block closes");
    rest[open + 1..open + close].split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The turn body's grid is the mockup's, declaration for declaration. It
/// rendered as one run-on line with every label fused to its value until the
/// sheet was given these, and nothing in the suite noticed: the fields were
/// all present and every one of them was asserted.
#[tokio::test]
async fn the_turn_body_grid_is_the_mocks() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;
    let (_status, _content_type, sheet) = get(&config, "/web.css").await;

    for selector in [".tibody", ".tibody .l, .tibody .n", ".tibody .wide", ".tibody b"] {
        assert_eq!(
            declarations_for(&sheet, selector),
            declarations_for(MOCK, selector),
            "the sheet's {selector}",
        );
    }
}

/// A conversation with a run of tool calls renders the run's count and the
/// families it met, with each call's own target under its family and the
/// assistant's prose above it. This is the model the mockup was built on.
///
/// Three calls across two families, so the header's count is the calls
/// rather than the families: one call per family would read the same either
/// way.
#[tokio::test]
async fn the_conversation_renders_the_run_and_its_families() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Busytools", &["forge"])]).expect("the fleet builds");
    fleet.start("Busytools", "forge").expect("forge is declared");
    fleet.install_agent("Busytools", "forge", "lead");
    fleet
        .seed_transcript(
            "Busytools",
            "forge",
            "lead",
            &[
                r#"{"type":"user","message":{"role":"user","content":"make the call tree the default"}}"#,
                r#"{"type":"assistant","message":{"id":"msg_1","role":"assistant","model":"claude-opus-5","content":[{"type":"text","text":"Reading the grouping code first."}]}}"#,
                r#"{"type":"assistant","message":{"id":"msg_2","role":"assistant","model":"claude-opus-5","content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"/tmp/family.rs"}}]}}"#,
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"pub enum ToolFamily {}"}]}}"#,
                r#"{"type":"assistant","message":{"id":"msg_3","role":"assistant","model":"claude-opus-5","content":[{"type":"tool_use","id":"toolu_2","name":"Grep","input":{"pattern":"KindRow"}}]}}"#,
                r#"{"type":"assistant","message":{"id":"msg_4","role":"assistant","model":"claude-opus-5","content":[{"type":"tool_use","id":"toolu_3","name":"Read","input":{"file_path":"/tmp/grouping.rs"}}]}}"#,
            ],
        )
        .expect("the transcript is written");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(page.contains("3 tool calls"), "every call in the run, not one per family: {page}");
    assert!(page.contains(">read</span>"), "and the families it met: {page}");
    assert!(page.contains(">search</span>"), "the run's second: {page}");
    let read = page.find(">read</span>").expect("the read family");
    let search = page.find(">search</span>").expect("the search family");
    assert!(read < search, "drawn in the order it met them: {page}");
    assert!(page.contains("family.rs"), "with each call's own target: {page}");
    assert!(page.contains("grouping.rs"), "including the second of a family: {page}");
    assert!(
        page.contains("Reading the grouping code first."),
        "and the prose the assistant wrote above it: {page}",
    );
    assert!(
        page.contains("make the call tree the default"),
        "and the turn the user wrote, on its own: {page}",
    );
}

/// A conversation with nothing in it draws no skeleton. A chat frame with no
/// rows reads as a session that has nothing to say, which is a different
/// page from one whose session has not started.
#[tokio::test]
async fn an_empty_conversation_draws_no_skeleton() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Busytools", &["forge"])]).expect("the fleet builds");
    fleet.start("Busytools", "forge").expect("forge is declared");
    fleet.install_agent("Busytools", "forge", "lead");
    fleet.seed_transcript("Busytools", "forge", "lead", &[]).expect("an empty transcript");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(
        page.contains("class=\"chat\""),
        "the page is the session's own, so an absence below is an empty conversation \
         rather than a page that never loaded: {page}",
    );
    assert!(!page.contains("class=\"kind\""), "no group is drawn for nothing: {page}");
    assert!(!page.contains("class=\"mine\""), "and no turn either: {page}");
    assert!(!page.contains("not running"), "the seat is running, so it does not say otherwise");
}

/// A turn the wire carries, with the id the transcript row shares.
fn user_frame(uuid: &str, text: &str) -> forge_primitives::Message {
    serde_json::from_value(serde_json::json!({
        "type": "user",
        "uuid": uuid,
        "message": { "role": "user", "content": text },
        "session_id": "s",
    }))
    .expect("a user frame")
}

/// The live half: the read is the baseline and the stream is applied on top
/// of it, so a message that arrives in the overlap is in both halves and the
/// page drops the stream's copy because the read already carries its id.
#[tokio::test]
async fn a_message_the_read_carried_is_not_appended_twice() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Busytools", &["forge"])]).expect("the fleet builds");
    fleet.start("Busytools", "forge").expect("forge is declared");
    fleet.install_agent("Busytools", "forge", "lead");
    fleet
        .seed_transcript(
            "Busytools",
            "forge",
            "lead",
            &[
                r#"{"type":"user","uuid":"u-1","message":{"role":"user","content":"make the call tree the default"}}"#,
            ],
        )
        .expect("the transcript is written");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;
    let (_status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;
    assert!(page.contains("make the call tree the default"), "precondition: the read drew it");

    let stream = open_stream_at(&config, "/session/Busytools/forge/lead/events").await;
    // The opening event is the read, and the same row then arrives on the
    // stream: that overlap is what the ordering makes possible. A second
    // message follows it, so the payload below is the region as the page
    // holds it once both have been through.
    fleet.emit(forge_sessions::SessionUpdate::ChatAppended {
        key: SessionSlot::lead("Busytools", "forge"),
        msg: user_frame("u-1", "make the call tree the default"),
    });
    fleet.emit(forge_sessions::SessionUpdate::ChatAppended {
        key: SessionSlot::lead("Busytools", "forge"),
        msg: user_frame("u-3", "and the one after it"),
    });
    let second = next_session_event(stream).await.expect("the second message redraws the region");

    assert!(second.contains("and the one after it"), "the new message is drawn: {second}");
    assert_eq!(
        second.matches("make the call tree the default").count(),
        1,
        "and the read's copy is the only copy of it the region carries: {second}",
    );
}

/// A message that arrived entirely after the read is appended, which is the
/// other half of the same rule.
#[tokio::test]
async fn a_message_the_read_did_not_carry_is_appended() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Busytools", &["forge"])]).expect("the fleet builds");
    fleet.start("Busytools", "forge").expect("forge is declared");
    fleet.install_agent("Busytools", "forge", "lead");
    fleet.seed_transcript("Busytools", "forge", "lead", &[]).expect("an empty transcript");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let stream = open_stream_at(&config, "/session/Busytools/forge/lead/events").await;
    fleet.emit(forge_sessions::SessionUpdate::ChatAppended {
        key: SessionSlot::lead("Busytools", "forge"),
        msg: user_frame("u-2", "a turn nobody had read"),
    });
    let region = next_session_event(stream).await.expect("the message redraws the region");

    assert!(
        region.contains("a turn nobody had read"),
        "the stream's own message is drawn: {region}",
    );
}

/// The API clock the wire sends counts up across the session, so a row draws
/// the delta from the result before it. Two captured results where the
/// second's cumulative 3375 ms is 1278 ms of its own against a 1284 ms wall
/// clock, and the first has nothing before it to subtract, so its own figure
/// is unknown rather than the session's whole clock.
#[tokio::test]
async fn a_turn_row_draws_its_own_api_time_rather_than_the_sessions() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Busytools", &["forge"])]).expect("the fleet builds");
    fleet.start("Busytools", "forge").expect("forge is declared");
    fleet.install_agent("Busytools", "forge", "lead");
    fleet.seed_transcript("Busytools", "forge", "lead", &[]).expect("an empty transcript");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let stream = open_stream_at(&config, "/session/Busytools/forge/lead/events").await;
    for result in captured_results("multi_turn") {
        fleet.emit(forge_sessions::SessionUpdate::ChatAppended {
            key: SessionSlot::lead("Busytools", "forge"),
            msg: result,
        });
    }
    let region = nth_session_event(stream, 3).await.expect("both results redraw the region");

    assert!(region.contains("<b>api</b>1.2s"), "the turn's own api time: {region}");
    assert!(!region.contains("<b>api</b>3.4s"), "not the clock the session had reached: {region}");
    assert!(
        !region.contains("<b>api</b>2.0s"),
        "and the first turn has nothing to subtract, so it reports none of the clock \
         rather than all of it: {region}",
    );
    assert!(region.contains("<b>elapsed</b>2.1s"), "its own wall clock is still there: {region}");
    assert!(
        region.contains("<b>local</b>0.0s tools + hooks"),
        "and the split that follows from it: {region}",
    );
}

/// A finished turn draws its row and the body behind it: the wall clock the
/// CLI recorded for that turn, the tokens it reported, and the session cost
/// it had reached, with what the row has no room for behind the fold.
///
/// The result frame arrives on the stream rather than in the transcript the
/// scan reads: the scan keeps conversation rows, and a result is not one. It
/// is the capture's own frame, so every figure below is one the CLI wrote.
#[tokio::test]
async fn a_settled_turn_draws_its_row_and_its_body() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Busytools", &["forge"])]).expect("the fleet builds");
    fleet.start("Busytools", "forge").expect("forge is declared");
    fleet.install_agent("Busytools", "forge", "lead");
    fleet.seed_transcript("Busytools", "forge", "lead", &[]).expect("an empty transcript");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;
    let settled = captured_results("monitor_persistent_stream").pop().expect("the captured turn");

    let stream = open_stream_at(&config, "/session/Busytools/forge/lead/events").await;
    fleet.emit(forge_sessions::SessionUpdate::ChatAppended {
        key: SessionSlot::lead("Busytools", "forge"),
        msg: settled,
    });
    let region = next_session_event(stream).await.expect("the settled turn redraws the region");

    assert!(region.contains("class=\"turninfo\""), "the settled row: {region}");
    assert!(region.contains("41.0s"), "the turn's own wall clock: {region}");
    assert!(region.contains("14\u{2191} 1.7k\u{2193}"), "and what it used: {region}");
    assert!(region.contains("93% cached"), "how much of the input the cache served: {region}");
    assert!(region.contains("7.0k written"), "including what it wrote to the cache: {region}");
    assert!(
        region.contains("$0.16 cumulative"),
        "and the session cost, named as the running total it is: {region}",
    );
    // One result and nothing before it: the API clock on the wire is the
    // session's, so this turn's share of it is unknown, and the local figure
    // is the split that needs it.
    assert!(region.contains("<b>api</b>-"), "no anchor, so no turn api time: {region}");
    assert!(region.contains("<b>local</b>-"), "and nothing to split out of the clock: {region}");
    assert!(region.contains("<b>out</b>1,711"), "the body's counts are the exact ones: {region}");
    assert!(region.contains("<b>cache</b>102,194 read"), "both sides of the cache: {region}");
    assert!(region.contains("<span class=\"tog\"></span>"), "the chip that opens it: {region}");
    assert!(
        region.contains("93% of input served from cache"),
        "and the share spelled out: {region}",
    );
}

/// The pane handles are two halves that have to meet: the boxes outside the
/// region the stream swaps, and rules that cross that region to the app they
/// size. Both were wrong at once, so every handle did nothing and the rail
/// and the inspector were unreachable below the widths that hide them.
#[tokio::test]
async fn the_pane_state_crosses_the_swapped_region() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;
    let (_status, _content_type, sheet) = get(&config, "/web.css").await;

    let wrapper = page.find("id=\"live\"").expect("the wrapper the payload never replaces");
    let region = page.find("id=\"session-body\"").expect("the region the payload replaces");
    let boxes = page.find("id=\"l\"").expect("the projects box");
    assert!(
        wrapper < boxes && boxes < region,
        "the boxes hold their state across a swap only by sitting outside the region: {page}",
    );

    assert!(
        page.contains("htmx:beforeSwap"),
        "and the swap carries what a reader opened or closed across itself: {page}",
    );

    let mut reads = sheet.match_indices(":checked").peekable();
    assert!(reads.peek().is_some(), "the sheet reads the boxes");
    for (at, _) in reads {
        assert!(
            sheet[at..].starts_with(":checked ~ #session-body"),
            "every rule that reads a box crosses the region to reach the app: {}",
            &sheet[at..(at + 60).min(sheet.len())],
        );
    }
}

/// A resume or a `/new` puts another occupant in the slot, and the history
/// that arrives with it is the conversation the reader is owed. The page's
/// own copy belongs to the seat that just left, so the region is drawn from
/// the update when it carries one.
#[tokio::test]
async fn a_replacement_draws_the_history_it_carries() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Busytools", &["forge"])]).expect("the fleet builds");
    fleet.start("Busytools", "forge").expect("forge is declared");
    fleet.install_agent("Busytools", "forge", "lead");
    fleet
        .seed_transcript(
            "Busytools",
            "forge",
            "lead",
            &[
                r#"{"type":"user","uuid":"u-out","message":{"role":"user","content":"the seat before it left"}}"#,
            ],
        )
        .expect("the transcript is written");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let stream = open_stream_at(&config, "/session/Busytools/forge/lead/events").await;
    fleet.emit(forge_sessions::SessionUpdate::SessionReplaced {
        key: SessionSlot::lead("Busytools", "forge"),
        session_id: forge_primitives::SessionId::new("session-2"),
        cwd: String::new(),
        current_model: forge_primitives::CurrentModel::new("claude-opus-5", "opus", "Opus 5"),
        available_models: Vec::new(),
        mode: None,
        history: vec![user_frame("u-in", "the turn the new occupant resumes")],
        compaction_count: 0,
    });
    let region = next_session_event(stream).await.expect("the replacement redraws the region");

    assert!(
        region.contains("the turn the new occupant resumes"),
        "the history it carries: {region}"
    );
    assert!(
        !region.contains("the seat before it left"),
        "and not the conversation that belonged to the seat it replaced: {region}",
    );
}

/// A turn's thinking estimate, as the CLI reports it frame by frame.
fn thinking_frame(tokens: u64) -> forge_primitives::Message {
    serde_json::from_value(serde_json::json!({
        "type": "system",
        "subtype": "thinking_tokens",
        "estimated_tokens": tokens,
        "estimated_tokens_delta": 100,
        "uuid": "think-1",
        "session_id": "s",
    }))
    .expect("a thinking frame")
}

/// The state frame that says a turn began, which is what arms the row's clock.
fn running_frame() -> forge_primitives::Message {
    serde_json::from_value(serde_json::json!({
        "type": "system",
        "subtype": "session_state_changed",
        "state": "running",
        "uuid": "state-1",
        "session_id": "s",
    }))
    .expect("a state frame")
}

/// The live row counts the estimate while the turn runs.
#[tokio::test]
async fn the_live_row_counts_the_thinking_estimate() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Busytools", &["forge"])]).expect("the fleet builds");
    fleet.start("Busytools", "forge").expect("forge is declared");
    fleet.install_agent("Busytools", "forge", "lead");
    fleet.seed_transcript("Busytools", "forge", "lead", &[]).expect("an empty transcript");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let stream = open_stream_at(&config, "/session/Busytools/forge/lead/events").await;
    for msg in [running_frame(), thinking_frame(434)] {
        fleet.emit(forge_sessions::SessionUpdate::ChatAppended {
            key: SessionSlot::lead("Busytools", "forge"),
            msg,
        });
    }
    let region = nth_session_event(stream, 3).await.expect("the frames redraw the region");

    assert!(region.contains("class=\"ring\""), "the row is the live one: {region}");
    assert!(region.contains("thinking 434"), "and it carries the estimate: {region}");
}

/// Once the result lands the estimate leaves the row and stays in the body:
/// the row gives its width to the billed counts, and the body holds what the
/// turn thought.
#[tokio::test]
async fn the_settled_row_keeps_the_estimate_in_its_body() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Busytools", &["forge"])]).expect("the fleet builds");
    fleet.start("Busytools", "forge").expect("forge is declared");
    fleet.install_agent("Busytools", "forge", "lead");
    fleet.seed_transcript("Busytools", "forge", "lead", &[]).expect("an empty transcript");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;
    let settled = captured_results("multi_turn").pop().expect("the captured turn");

    let stream = open_stream_at(&config, "/session/Busytools/forge/lead/events").await;
    for msg in [thinking_frame(434), settled] {
        fleet.emit(forge_sessions::SessionUpdate::ChatAppended {
            key: SessionSlot::lead("Busytools", "forge"),
            msg,
        });
    }
    let region = nth_session_event(stream, 3).await.expect("the frames redraw the region");

    assert!(region.contains("<b>thinking</b>434 est"), "the body keeps it: {region}");
    assert!(
        !region.contains("thinking 434"),
        "and the row does not, now that the billed counts are there: {region}",
    );
}

/// A zero inside a block that does carry counters is a measurement rather
/// than an absence. The captured frame spent nothing on cache reads and
/// 13,939 on cache writes, so the read cell says zero while the row around it
/// stands.
#[tokio::test]
async fn a_zero_inside_a_real_usage_block_is_drawn_as_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Busytools", &["forge"])]).expect("the fleet builds");
    fleet.start("Busytools", "forge").expect("forge is declared");
    fleet.install_agent("Busytools", "forge", "lead");
    fleet.seed_transcript("Busytools", "forge", "lead", &[]).expect("an empty transcript");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;
    let frame = captured_results("set_model").pop().expect("the captured turn");

    let stream = open_stream_at(&config, "/session/Busytools/forge/lead/events").await;
    fleet.emit(forge_sessions::SessionUpdate::ChatAppended {
        key: SessionSlot::lead("Busytools", "forge"),
        msg: frame,
    });
    let region = next_session_event(stream).await.expect("the turn redraws the region");

    assert!(region.contains("<b>cache</b>0 read"), "the zero it did spend: {region}");
    assert!(region.contains("<b>wrote</b>13,939"), "beside the count it did spend: {region}");
    assert!(region.contains("13k written"), "and the row's own chip keeps it: {region}");
}

/// A cron fire is drawn as the turn it is. The session's model is handed the
/// prompt on its own stdin and the CLI does not echo it back, so the wire
/// carries nothing a page could draw; the workspace announces the delivery as
/// a typed update instead, and the turn is forged from that.
#[tokio::test]
async fn a_cron_delivery_is_drawn_as_the_turn_it_is() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Busytools", &["forge"])]).expect("the fleet builds");
    fleet.start("Busytools", "forge").expect("forge is declared");
    fleet.install_agent("Busytools", "forge", "lead");
    fleet.seed_transcript("Busytools", "forge", "lead", &[]).expect("an empty transcript");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let stream = open_stream_at(&config, "/session/Busytools/forge/lead/events").await;
    fleet.emit(forge_sessions::SessionUpdate::CronPromptAppended {
        key: SessionSlot::lead("Busytools", "forge"),
        text: "the nightly sweep is due".to_owned(),
    });
    let region = next_session_event(stream).await.expect("the delivery redraws the region");

    assert!(region.contains("the nightly sweep is due"), "the fired prompt is drawn: {region}");
    assert!(
        region.contains("class=\"notice"),
        "as the block a delivery arrives in, not as a turn somebody wrote: {region}",
    );
}

/// The slot guard reads all three parts of a slot. The same org and project
/// with another seat is another seat: a worker's message is not its lead's
/// news, and the stream carries every session in the fleet.
#[tokio::test]
async fn a_workers_message_does_not_redraw_the_leads_page() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let stream = open_stream_at(&config, "/session/Busytools/forge/lead/events").await;
    fleet.emit(forge_sessions::SessionUpdate::ChatAppended {
        key: SessionSlot::worker("Busytools", "forge", "em-dash-sweep"),
        msg: user_frame("u-10", "a message for the seat beside it"),
    });

    assert!(
        nth_session_event_within(stream, 2, std::time::Duration::from_millis(300)).await.is_none(),
        "the worker's seat is not this page",
    );
}

/// An update for another slot does not redraw this page: the same stream
/// carries every session, and a page that swapped on all of them would
/// re-render a conversation nobody changed.
#[tokio::test]
async fn an_update_for_another_slot_does_not_swap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let stream = open_stream_at(&config, "/session/Busytools/forge/lead/events").await;
    fleet.emit(forge_sessions::SessionUpdate::ChatAppended {
        key: SessionSlot::lead("Busytools", "busymail"),
        msg: user_frame("u-9", "a message for another session"),
    });

    assert!(
        nth_session_event_within(stream, 2, std::time::Duration::from_millis(300)).await.is_none(),
        "another slot's message is not this page's news",
    );
}

/// The region that follows the connection's opening one, whenever it comes.
async fn next_session_event(response: reqwest::Response) -> Option<String> {
    nth_session_event(response, 2).await
}

/// The region of the `nth` event the stream sends, counting the opening one
/// as the first, or `None` when it does not arrive in time: a stream that
/// should stay quiet is as much a property as one that speaks.
///
/// The count is the caller's, because a buffer holding several regions has
/// no way to say which one the caller meant.
async fn nth_session_event(response: reqwest::Response, nth: usize) -> Option<String> {
    nth_session_event_within(response, nth, std::time::Duration::from_secs(5)).await
}

/// The region of the `nth` event, or `None` when it does not arrive in time.
async fn nth_session_event_within(
    response: reqwest::Response,
    nth: usize,
    within: std::time::Duration,
) -> Option<String> {
    use futures_util::StreamExt;
    let mut stream = response.bytes_stream();
    let mut seen = String::new();
    loop {
        let chunk = tokio::time::timeout(within, stream.next()).await.ok()??;
        seen.push_str(&String::from_utf8_lossy(&chunk.ok()?));
        if seen.matches("event: session").count() >= nth {
            return seen.rsplit("event: session").next().map(str::to_owned);
        }
    }
}

/// A worker's own page is served, not only its lead's: the home links
/// every row it draws, and a link into a 404 is worse than no link.
#[tokio::test]
async fn a_workers_own_page_serves() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (status, _content_type, page) =
        get(&config, "/session/Busytools/forge/em-dash-sweep").await;

    assert_eq!(status, reqwest::StatusCode::OK, "a worker's seat is a page too");
    assert!(
        page.contains("<span class=\"nm\">em-dash-sweep</span>"),
        "and the header names the worker rather than its project: {page}",
    );
}

/// A project's name can hold a slash - the mock's own roster has one - so
/// the route's segment escaping has to survive the round trip: the home
/// links the escaped form, and the route decodes it back to one segment.
#[tokio::test]
async fn a_project_name_with_a_slash_addresses_the_page() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Personal", &["companies/steward"])])
        .expect("the fleet builds");
    fleet.start("Personal", "companies/steward").expect("the project is declared");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (status, _content_type, page) =
        get(&config, "/session/Personal/companies%2Fsteward/lead").await;

    assert_eq!(status, reqwest::StatusCode::OK, "the escaped name addresses one segment: {page}");
    assert!(page.contains("companies/steward"), "and the page names the project: {page}");

    let (_status, _content_type, home) = get(&config, "/").await;
    assert!(
        home.contains("/session/Personal/companies%2Fsteward/lead"),
        "the home links the escaped form: {home}",
    );
}

/// A repository at `dir` with one commit and `changed` tracked files moved
/// in it. `git init -b` needs git 2.28 and CI runs 2.25, so HEAD is
/// pointed by `symbolic-ref` instead.
fn repo_with(dir: &Path, changed: usize) {
    git(dir, &["init", "-q"]);
    git(dir, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    git(dir, &["config", "user.email", "t@e.com"]);
    git(dir, &["config", "user.name", "T"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
    for n in 0..changed {
        std::fs::write(dir.join(format!("file-{n}.txt")), "one\n").expect("write");
    }
    git(dir, &["add", "."]);
    git(dir, &["commit", "-qm", "init"]);
    for n in 0..changed {
        std::fs::write(dir.join(format!("file-{n}.txt")), "one\ntwo\n").expect("modify");
    }
}

/// A repository at `dir` whose one commit carries `files`, each then grown
/// by its own number of lines: the scan orders its list by total changes,
/// so the sizes decide the order the section meets them in.
fn repo_of_sizes(dir: &Path, files: &[(&str, usize)]) {
    git(dir, &["init", "-q"]);
    git(dir, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    git(dir, &["config", "user.email", "t@e.com"]);
    git(dir, &["config", "user.name", "T"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
    for (path, _) in files {
        let file = dir.join(path);
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).expect("a directory for the file");
        }
        std::fs::write(&file, "one\n").expect("write");
    }
    git(dir, &["add", "."]);
    git(dir, &["commit", "-qm", "init"]);
    for (path, added) in files {
        std::fs::write(dir.join(path), format!("one\n{}", "grown\n".repeat(*added))).expect("grow");
    }
}

/// Spawn git the way the product does, scrub included: a fixture that
/// skipped the scrub answers about a foreign repository when the suite
/// runs under a git hook.
fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
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

/// The `result` frames of one captured baseline, as the wire sent them.
///
/// A capture rather than a frame written to match the row: the numbers the
/// turn body derives - an API clock that counts up across the session, a
/// compaction that reports every counter at zero - only exist on real
/// traffic, and a fixture would restate the reading it is meant to check.
fn captured_results(name: &str) -> Vec<forge_primitives::Message> {
    let baselines =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../forge-test-harness/baselines/sdk");
    // The version directory is named for the CLI the capture came from, so
    // the capture is what identifies it: two directories holding the same
    // name would make this walk pick by directory order.
    let holding: Vec<std::path::PathBuf> = std::fs::read_dir(&baselines)
        .expect("the baseline directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.join(format!("{name}.jsonl")).is_file())
        .collect();
    assert_eq!(holding.len(), 1, "one captured version holds {name}");
    let raw =
        std::fs::read_to_string(holding[0].join(format!("{name}.jsonl"))).expect("the capture");
    let results: Vec<forge_primitives::Message> = raw
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|envelope| envelope["dir"] == "in")
        .filter(|envelope| {
            serde_json::from_str::<serde_json::Value>(envelope["line"].as_str().unwrap_or_default())
                .is_ok_and(|frame| frame["type"] == "result")
        })
        .map(|envelope| {
            serde_json::from_str(envelope["line"].as_str().expect("the frame text"))
                .expect("a result frame the decoder takes")
        })
        .collect();
    assert!(!results.is_empty(), "the capture holds a result frame to read");
    results
}

/// A frame that reports nothing shows nothing. The compaction's result
/// carries every counter at zero and an API clock of zero, and a zero where
/// a measurement belongs reads as one; the row it leaves is dashes, with the
/// clock it does have standing beside them.
#[tokio::test]
async fn a_frame_that_reports_nothing_draws_no_zeroes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Busytools", &["forge"])]).expect("the fleet builds");
    fleet.start("Busytools", "forge").expect("forge is declared");
    fleet.install_agent("Busytools", "forge", "lead");
    fleet.seed_transcript("Busytools", "forge", "lead", &[]).expect("an empty transcript");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;
    let compaction = captured_results("compact").pop().expect("the captured compaction");

    let stream = open_stream_at(&config, "/session/Busytools/forge/lead/events").await;
    fleet.emit(forge_sessions::SessionUpdate::ChatAppended {
        key: SessionSlot::lead("Busytools", "forge"),
        msg: compaction,
    });
    let region = next_session_event(stream).await.expect("the frame redraws the region");

    assert!(region.contains("45.2s"), "the clock the frame does carry: {region}");
    assert!(!region.contains("0 read"), "and no zero where a count would go: {region}");
    assert!(region.contains("<b>api</b>-"), "the one it does not reads as a dash: {region}");
    assert!(!region.contains("<b>api</b>0.0s"), "and never as a zero: {region}");
    assert!(!region.contains("<b>in</b>0"), "no zero stands in for a count: {region}");
    assert!(!region.contains("0 written"), "not in the collapsed row's own chips either: {region}");
}
