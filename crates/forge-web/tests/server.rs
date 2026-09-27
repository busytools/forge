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

/// The page draws with the built-in pair, and the sheet declares neither
/// stack of its own. The absence is the load-bearing half: the injected
/// block is emitted before the link, so a stack in the sheet's own `:root`
/// would win on document order at equal specificity and the page would
/// draw the OS face with nothing reporting it.
#[tokio::test]
async fn the_page_draws_with_the_built_in_pair() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, page) = get(&config, "/").await;
    let (_status, _content_type, sheet) = get(&config, "/web.css").await;

    assert!(page.contains("--ui:\"Inter\""), "the injected stack is the webfont: {page}");
    assert!(page.contains("--mono:\"Fira Code\""), "for code as well: {page}");
    assert!(!sheet.contains("--ui:"), "and the sheet declares no stack to outrank it: {sheet}");
    assert!(!sheet.contains("--mono:"), "neither one: {sheet}");
}

/// `[web] font = "system"` is the opt-out: the same page draws the OS
/// stacks instead, which is the whole of what the key selects.
#[tokio::test]
async fn the_font_key_opts_out_to_the_system_stack() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) =
        start_on_a_free_port_with(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface(), |config| {
            WebConfig { font: Some("system".to_owned()), ..config }
        })
        .await;

    let (_status, _content_type, page) = get(&config, "/").await;

    assert!(page.contains("--ui:system-ui"), "the OS stack is what `system` draws: {page}");
    assert!(page.contains("--mono:ui-monospace"), "and its own mono face: {page}");
    assert!(!page.contains("\"Inter\""), "with the webfont not asked for at all: {page}");
}

/// The two faces are served, at the path the sheet names: the request is
/// built from the sheet's own `url(...)`, so a renamed route fails here
/// rather than as a browser quietly falling back to the OS face.
#[tokio::test]
async fn the_faces_the_sheet_asks_for_are_served() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;
    let (_status, _content_type, sheet) = get(&config, "/web.css").await;

    let sources: Vec<&str> = sheet
        .split("@font-face")
        .skip(1)
        .filter_map(|block| block.split_once("url(\""))
        .filter_map(|(_, rest)| rest.split('"').next())
        .collect();
    assert_eq!(sources.len(), 2, "a source per face: {sources:?}");

    for src in sources {
        let response =
            reqwest::get(format!("http://127.0.0.1:{}{src}", config.port)).await.expect("served");
        assert_eq!(response.status(), reqwest::StatusCode::OK, "{src} is what the sheet asks for");
        assert_eq!(
            response.headers()["cache-control"],
            "no-cache",
            "{src} must not be held stale by a browser",
        );
        let content_type = response.headers()["content-type"].to_str().expect("readable");
        assert!(
            content_type.starts_with("font/woff2"),
            "{src} labelled {content_type} is refused by the browser",
        );
        let body = response.bytes().await.expect("the body reads");
        assert!(body.starts_with(b"wOF2"), "{src} is a woff2 and not a rename of something else");
    }
}

/// A name that is not a vendored face is a 404, and the scripts are not
/// reachable as fonts: each route looks its own set up, so a wrong name
/// says so rather than being served under the wrong type.
#[tokio::test]
async fn a_name_that_is_not_a_vendored_face_is_not_found() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (status, _content_type, _body) = get(&config, "/fonts/absent.woff2").await;
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND, "a face that is not vendored says so");

    let (status, _content_type, _body) = get(&config, "/fonts/htmx.js").await;
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND, "and a script is not served as a font");
}
