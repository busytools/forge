//! The server: it binds what the config says, serves the home, and binds
//! nothing at all when it is turned off.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::path::Path;
use std::sync::Arc;

use forge_primitives::SessionSlot;
use forge_primitives::WebConfig;
use forge_sessions::SessionUpdate;
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

/// The two paths of the built-in mark, verbatim from the sheet `panes` was
/// sourced off: the divider between the panes, and the solid left pane. A
/// literal rather than a call into the crate, so a redrawn or mistyped
/// path fails here instead of agreeing with itself.
const PANES_DIVIDER: &str = "M13.6 3.2V20.8";

const PANES_SOLID: &str = "M4.4 6.4a2 2 0 0 1 2-2h4v15.2h-4a2 2 0 0 1-2-2Z";

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

/// Unset keys are the built-ins: panes on the tab, and the dark palette
/// as the page's own root variables rather than a stylesheet's copy of
/// it.
#[tokio::test]
async fn unset_names_fall_back_to_the_built_in_mark_and_palette() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (status, _content_type, favicon) = get(&config, "/favicon.svg").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(
        favicon.contains(PANES_DIVIDER) && favicon.contains(PANES_SOLID),
        "the default mark is panes, the sheet's own drawing, got: {favicon}",
    );

    let (status, _content_type, page) = get(&config, "/").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(
        page.contains("--accent:#f47600"),
        "the page carries the palette as its root variables, got: {page}",
    );
}

/// The home mockup draws the mark the page draws, so the side-by-side is a
/// comparison between two drawings of one mark rather than of two marks.
/// Both sides are read: the mockup as a document, the page as it is served,
/// because a comparison against one of them is not the comparison this
/// names.
#[tokio::test]
async fn the_home_mockup_draws_the_mark_the_page_draws() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (status, _content_type, page) = get(&config, "/").await;

    assert_eq!(status, reqwest::StatusCode::OK);
    for (side, drawn) in [("page", page.as_str()), ("mockup", MOCK_HOME)] {
        assert!(drawn.contains(PANES_DIVIDER), "the {side}'s panes divider");
        assert!(drawn.contains(PANES_SOLID), "and the {side}'s solid pane");
    }
}

/// The mockups draw the faces the view ships. A reference and a built page
/// in different typefaces is a difference to explain rather than act on,
/// which is the opposite of what a mockup is for.
#[test]
fn the_mockups_draw_the_faces_the_view_ships() {
    let built_in = forge_web::theme::font_variables(None).expect("the built-in pair");
    for (mockup, sheet) in [
        ("web-home.html", MOCK_HOME),
        ("web-session.html", MOCK),
        ("web-composer.html", MOCK_COMPOSER),
    ] {
        for token in ["--ui:", "--mono:"] {
            assert_eq!(
                first_family(sheet, token),
                first_family(built_in, token),
                "{mockup} draws a different face than the view ships for {token}",
            );
        }
    }
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

/// The home's mockup, read for the same reason: it is what the home is held
/// against, and the mark and the typefaces are the two things it draws that
/// the page has to match.
const MOCK_HOME: &str = include_str!("../../../docs/mockups/web-home.html");

/// The composer's mockup: a specimen read state by state against the built
/// page, so its faces are read as closely as its states.
const MOCK_COMPOSER: &str = include_str!("../../../docs/mockups/web-composer.html");

/// Every symbol the page's two mockups define, by id. Both mocks draw some
/// of the same marks, so the session mockup keeps the ids it already won -
/// it is the mockup the page was built from, and a shared mark must not
/// change weight under the page that already draws it - and the composer's
/// own are added to them.
fn mocks_sprite() -> std::collections::BTreeMap<String, String> {
    let mut want = symbols(MOCK);
    for (id, attrs) in symbols(MOCK_COMPOSER) {
        want.entry(id).or_insert(attrs);
    }
    want
}

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

/// A stylesheet with its comments taken out, so a scan reads declarations
/// rather than prose: a comment may name a token the sheet never declares.
fn strip_comments(css: &str) -> String {
    css.split("/*")
        .enumerate()
        .map(|(nth, chunk)| {
            if nth == 0 {
                chunk.to_owned()
            } else {
                chunk.split_once("*/").map_or(String::new(), |(_, rest)| rest.to_owned())
            }
        })
        .collect()
}

/// The first family a stylesheet's `token` declaration names, unquoted: the
/// one a browser draws with, the rest being the fallback chain.
fn first_family(css: &str, token: &str) -> String {
    css.split_once(token)
        .and_then(|(_, rest)| rest.split([',', ';']).next())
        .unwrap_or_default()
        .trim()
        .trim_matches('"')
        .to_owned()
}

/// The `--fs-*` tokens a stylesheet declares, by name.
fn scale_tokens(css: &str) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    let without_comments = strip_comments(css);
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

/// Every symbol the mockups define is the symbol the page draws, attribute
/// for attribute. The sprite is a hand-copied block with nothing else
/// comparing it, which is how a stroke width drifted once already.
#[tokio::test]
async fn the_sprite_is_the_mocks_sprite() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;
    let (_status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    let want = mocks_sprite();
    let got = symbols(&page);

    assert!(!want.is_empty(), "the mockups' sprite is what this pins the page against");
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
    // Declarations rather than the file's text: the comment above the
    // block names both tokens, so a whole-file search would fire on a
    // rewording and point at the cascade for a bug that is not there.
    let declared = strip_comments(&sheet);
    assert!(!declared.contains("--ui:"), "and the sheet declares no stack to outrank it: {sheet}");
    assert!(!declared.contains("--mono:"), "neither one: {sheet}");
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
    assert_eq!(sources.len(), 3, "a source per face: {sources:?}");

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

// ---------- the composer: the box and its autocomplete ----------

/// One query value, escaped the way a browser escapes a form field: a
/// literal rather than a call into the server's own decoder, so a decoder
/// that mangles its input cannot agree with itself here.
fn encode_query(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(char::from(byte));
            }
            b' ' => out.push('+'),
            other => {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                out.push('%');
                out.push(char::from(HEX[usize::from(other >> 4)]));
                out.push(char::from(HEX[usize::from(other & 0x0F)]));
            }
        }
    }
    out
}

/// A draft the browser has typed is a query parameter, so what the box
/// does with it is a real request rather than a claim about markup.
async fn composer(config: &WebConfig, draft: &str) -> (reqwest::StatusCode, String) {
    let url = format!(
        "http://127.0.0.1:{}/session/Busytools/forge/lead/composer?draft={}",
        config.port,
        encode_query(draft),
    );
    let response = reqwest::get(url).await.expect("served");
    (response.status(), response.text().await.expect("the body reads"))
}

/// Typing the trigger opens the list, and the list is the seat's own:
/// the CLI advertised these commands, so the popover carries them with
/// the typed span marked. Catches a popover opened on some other
/// trigger's list, and one that drops the match it was opened for.
#[tokio::test]
async fn typing_a_slash_opens_the_command_list() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    fleet.advertise(
        &SessionSlot::lead("Busytools", "forge"),
        vec![
            forge_primitives::AvailableCommand::new("model", "Switch model"),
            forge_primitives::AvailableCommand::new("memory", "Edit project memory"),
        ],
        Vec::new(),
    );
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (status, page) = composer(&config, "/m").await;

    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(page.contains("class=\"ac\""), "the popover opens on the trigger: {page}");
    assert!(page.contains("commands"), "and names the list it is showing: {page}");
    assert!(page.contains("/<em>m</em>odel"), "with the typed span marked: {page}");
    assert!(page.contains("Switch model"), "and the row's own description: {page}");
    assert!(
        page.contains("class=\"it sel\""),
        "and the first row is the one a key would take: {page}"
    );
}

/// The `&` trigger reads the agents the CLI advertised, and its rows
/// carry what the surface holds for them rather than an invented last-run.
#[tokio::test]
async fn the_ampersand_opens_the_subagent_list() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    fleet.advertise(
        &SessionSlot::lead("Busytools", "forge"),
        Vec::new(),
        vec![forge_primitives::AvailableAgent::new("cli-version", "Bump the pinned CLI")],
    );
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, page) = composer(&config, "&cli").await;

    assert!(page.contains("subagents"), "the agent list names itself: {page}");
    assert!(
        page.contains("<em>cli</em>-version"),
        "and the typed span is marked in its name: {page}"
    );
    assert!(page.contains("Bump the pinned CLI"), "with what the surface holds for it: {page}");
}

/// A list is what the query matched, not the whole set with the query
/// ignored: a command that does not carry what was typed is not offered,
/// and the header still counts where the list came from rather than how
/// many rows survived.
#[tokio::test]
async fn a_query_filters_the_list_it_opened() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    fleet.advertise(
        &SessionSlot::lead("Busytools", "forge"),
        vec![
            forge_primitives::AvailableCommand::new("model", "Switch model"),
            forge_primitives::AvailableCommand::new("memory", "Edit project memory"),
            forge_primitives::AvailableCommand::new("compact", "Compact conversation context"),
        ],
        Vec::new(),
    );
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, page) = composer(&config, "/mem").await;

    assert!(page.contains("/<em>mem</em>ory"), "the row that carries the query is offered: {page}");
    assert!(!page.contains("Compact conversation context"), "and one that does not is not: {page}");
    assert!(page.contains(">3<"), "while the header counts the list the rows came from: {page}");
}

/// A query nothing carries opens nothing, rather than a popover holding a
/// header and no rows.
#[tokio::test]
async fn a_query_that_matches_nothing_opens_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    fleet.advertise(
        &SessionSlot::lead("Busytools", "forge"),
        vec![forge_primitives::AvailableCommand::new("model", "Switch model")],
        Vec::new(),
    );
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, page) = composer(&config, "/zzz").await;

    assert!(!page.contains("class=\"ac\""), "nothing matched, so nothing opened: {page}");
}

/// An empty draft is the resting state: the placeholder, no popover, and
/// nothing that reads as a command being typed.
#[tokio::test]
async fn an_empty_draft_renders_the_placeholder() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, page) = composer(&config, "").await;

    assert!(page.contains("Type a message\u{2026}"), "the placeholder is the box's own: {page}");
    assert!(!page.contains("class=\"ac\""), "and no list is open with nothing typed: {page}");
    assert!(!page.contains("class=\"foot\""), "and no keys are advertised with no draft: {page}");
}

/// A seat cannot take input while it is held on a prompt, so the box is
/// replaced by the reason rather than drawn as an input that would drop
/// what was typed into it. Catches a composer that renders live-looking
/// controls over a session that is not running.
#[tokio::test]
async fn a_seat_with_nothing_running_gets_the_reason_not_a_box() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (status, _content_type, page) = get(&config, "/session/Personal/dotfiles/lead").await;

    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(page.contains("class=\"blocked\""), "the slot says why it takes no input: {page}");
    assert!(page.contains("not running"), "in the same words the chat column uses: {page}");
    assert!(!page.contains("id=\"draft\""), "and there is no input to lose a draft in: {page}");
}

/// The box's controls need the dispatch path, which is not built. Each is
/// drawn unavailable with the reason rather than as a control that does
/// nothing when it is clicked.
#[tokio::test]
async fn the_controls_say_they_cannot_act_yet() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, page) = composer(&config, "push it once CI is green").await;

    assert!(page.contains("push it once CI is green"), "the draft is the box's content: {page}");
    assert!(page.contains("disabled"), "the controls render unavailable: {page}");
    assert!(
        page.contains("not available yet"),
        "and say why rather than leaving a dead click: {page}",
    );
}

/// The composer is served as part of the session page, at the foot of the
/// chat column, so a session is somewhere you can type rather than only
/// read.
#[tokio::test]
async fn the_session_page_carries_the_composer() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _ct, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert!(page.contains("id=\"comp\""), "the session page carries the composer: {page}");
    assert!(page.contains("id=\"draft\""), "with the box the reader types in: {page}");
    assert!(
        page.contains("/vendor/htmx.js"),
        "and loads the script the box asks its own list with: {page}",
    );
}

/// A slot the roster does not hold has no composer either: the route
/// answers about a seat, and a seat that is not there is a 404 rather
/// than an empty box.
#[tokio::test]
async fn the_composer_of_an_unknown_slot_is_not_found() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (status, _ct, _page) = get(&config, "/session/Nobody/nothing/lead/composer").await;

    assert_eq!(status, reqwest::StatusCode::NOT_FOUND, "no seat, no composer");
}

// ---------- the composer: dictation ----------

/// The seat the composer tests drive takes, which the fixture's own fleet
/// already has running.
fn lead() -> SessionSlot {
    SessionSlot::lead("Busytools", "forge")
}

/// Let the process-wide fold task run. Two yields rather than one: the
/// first wakes it from `recv`, the second lets it reach its await again,
/// which is the state the request below reads.
async fn settle() {
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
}

fn started(generation: u64) -> SessionUpdate {
    SessionUpdate::DictateStarted { key: lead(), floor_db: -50.0, generation }
}

fn level(peak_db: f32) -> SessionUpdate {
    SessionUpdate::DictateLevel { key: lead(), peak_db }
}

fn ended(generation: u64, outcome: forge_sessions::surface::DictateOutcome) -> SessionUpdate {
    SessionUpdate::DictateEnded { key: lead(), outcome, generation }
}

/// A live take draws the meter from the readings the stream carried: one
/// cell per reading, and the row's figure is the newest of them. Catches a
/// meter drawn from a fixed shape rather than from the levels, and one
/// that keeps the latest reading alone.
#[tokio::test]
async fn a_live_take_draws_its_meter() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    fleet.emit(started(1));
    for peak in [-40.0_f32, -20.0, -10.0] {
        fleet.emit(level(peak));
    }
    settle().await;

    let (_status, page) = composer(&config, "").await;

    assert!(page.contains("class=\"box rec\""), "a live take tells the box so: {page}");
    assert_eq!(
        page.matches("style=\"height:").count(),
        3,
        "one meter cell per reading, and no more: {page}",
    );
    assert!(page.contains("-10 dB"), "and the row quotes the newest reading: {page}");
    assert!(page.contains("listening"), "while the take is still open: {page}");
    assert!(
        page.contains("stopping a take is not available yet"),
        "and its cancel says it cannot: {page}",
    );
}

/// The generation is what says which take a report belongs to. A take that
/// started after another is the live one, so the older take's end is not
/// news about it. Catches a resolver that clears whatever is running
/// rather than the take it names.
#[tokio::test]
async fn a_stale_takes_end_does_not_clear_a_newer_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    // The take that is running is the second, and the first one's resolver
    // arrives after it started.
    fleet.emit(started(2));
    fleet.emit(level(-12.0));
    fleet.emit(ended(
        1,
        forge_sessions::surface::DictateOutcome::NoAudio { peak_db: -38.2, seconds: 4 },
    ));
    settle().await;

    let (_status, page) = composer(&config, "").await;

    assert!(page.contains("class=\"box rec\""), "the newer take is still running: {page}");
    assert!(page.contains("-12 dB"), "with its meter intact: {page}");
    assert!(
        !page.contains("nothing above"),
        "and no notice from a take that was already over: {page}",
    );
}

/// Transcribing is the same row frozen: the take is still the box's state,
/// the meter is still drawn, and the label says what is happening now.
#[tokio::test]
async fn transcribing_freezes_the_same_row() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    fleet.emit(started(1));
    fleet.emit(level(-30.0));
    fleet.emit(SessionUpdate::DictateTranscribing { key: lead() });
    settle().await;

    let (_status, page) = composer(&config, "").await;

    assert!(page.contains("class=\"box tr\""), "the box turns to the transcribing tone: {page}");
    assert!(page.contains("class=\"dot tr\""), "so does the row's own mark: {page}");
    assert!(page.contains("class=\"wave tr\""), "and the meter freezes with it: {page}");
    assert!(page.contains("transcribing"), "and the label says what it is doing: {page}");
    assert!(!page.contains("listening"), "rather than still listening: {page}");
}

/// A progress report tallies settled segments onto the take it names.
#[tokio::test]
async fn progress_tallies_the_settled_segments() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    fleet.emit(started(3));
    fleet.emit(SessionUpdate::DictateProgress {
        key: lead(),
        generation: 3,
        done: 2,
        total: Some(6),
    });
    settle().await;

    let (_status, page) = composer(&config, "").await;

    assert!(page.contains("transcribing 2/6"), "the row counts what has settled: {page}");
}

/// A taken that landed puts its words where the caret was, and leaves no
/// row behind: the words are the answer.
#[tokio::test]
async fn a_landed_take_pastes_into_the_box() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    fleet.emit(started(1));
    fleet.emit(ended(
        1,
        forge_sessions::surface::DictateOutcome::Landed {
            text: "and run the gate".to_owned(),
            truncated: false,
        },
    ));
    settle().await;

    let (_status, page) = composer(&config, "fix the flaky retry test").await;

    assert!(
        page.contains("fix the flaky retry test and run the gate"),
        "the take's words land at the end of the draft: {page}",
    );
    assert!(page.contains("class=\"box done\""), "and the box takes the landed beat: {page}");
    assert!(!page.contains("class=\"dict\""), "with no take row left: {page}");
}

/// A take that produced nothing to insert leaves a line instead, worded
/// with its own measurement so the reader can tell a quiet room from a
/// muted microphone.
#[tokio::test]
async fn a_take_with_nothing_to_insert_leaves_its_notice() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    fleet.emit(started(1));
    fleet.emit(ended(
        1,
        forge_sessions::surface::DictateOutcome::NoAudio { peak_db: -38.2, seconds: 4 },
    ));
    settle().await;

    let (_status, page) = composer(&config, "").await;

    assert!(page.contains("class=\"notice q\""), "the notice takes the row: {page}");
    assert!(
        page.contains("nothing above -50 dBFS in 4s"),
        "and quotes the take's own floor: {page}",
    );
    assert!(page.contains("loudest was -38.2"), "and what it did hear: {page}");
    assert!(!page.contains("class=\"dict\""), "and the take row is gone: {page}");
}

/// With no take live the row reserves nothing: a composer that always
/// draws a status row is one whose box grows for a take nobody started.
#[tokio::test]
async fn no_take_reserves_no_row() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, page) = composer(&config, "").await;

    assert!(!page.contains("class=\"dict\""), "no take, no status row: {page}");
    assert!(!page.contains("class=\"notice"), "and no notice either: {page}");
    assert!(page.contains("class=\"box\""), "just the box: {page}");
}

// ---------- the composer: the prompt dock ----------

fn wire(value: serde_json::Value) -> forge_primitives::permission_ui::PermissionRequest {
    serde_json::from_value(value).expect("a permission request off the wire")
}

fn tool_call(id: &str, title: &str, input: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "tool_call_id": id,
        "title": title,
        "kind": "execute",
        "status": "pending",
        "content": [],
        "locations": [],
        "raw_input": input,
    })
}

/// A permission prompt, as the CLI sends it.
fn permission() -> forge_primitives::permission_ui::PermissionRequest {
    wire(serde_json::json!({
        "tool_call": tool_call("tu-1", "Bash", &serde_json::json!({
            "command": "git push origin polish/rate-limit-chip"
        })),
        "options": [
            {"option_id": "once", "name": "Allow once", "kind": "allow", "action": {"kind": "allow"}},
            {"option_id": "always", "name": "Allow always for Bash", "kind": "allow",
             "action": {"kind": "allow"}},
            {"option_id": "edits", "name": "Allow with edits", "kind": "edit",
             "action": {"kind": "allow_with_input"}},
            {"option_id": "deny", "name": "Deny", "kind": "deny", "action": {"kind": "deny"}},
            {"option_id": "notes", "name": "Tell Claude something else", "kind": "notes",
             "action": {"kind": "deny"}},
        ],
        "display": {"decision_reason": "not on the allow list"},
    }))
}

/// A question, as the CLI sends it.
fn question() -> forge_primitives::question::QuestionRequest {
    serde_json::from_value(serde_json::json!({
        "tool_call": tool_call("tu-2", "AskUserQuestion", &serde_json::json!({})),
        "prompt": {
            "question": "Pick the environments to deploy to.",
            "header": "Environments",
            "multi_select": true,
            "options": [
                {"option_id": "staging", "label": "Staging"},
                {"option_id": "prod", "label": "Production"},
            ],
        },
        "question_index": 1,
        "total_questions": 3,
    }))
    .expect("a question off the wire")
}

/// The dock is the box morphed, and its options are the ones the prompt
/// offered, in the CLI's own order. Catches a dock drawn from a fixed set,
/// and one that loses the order the core builds them in.
#[tokio::test]
async fn a_pending_prompt_morphs_the_box_and_lists_its_options() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    fleet.seed_test_pending_interaction(&lead(), PendingKind::Permission);
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;
    fleet.emit(SessionUpdate::PermissionRequest {
        key: lead(),
        tool_id: "tu-1".to_owned(),
        request: permission(),
    });
    settle().await;

    let (_status, page) = composer(&config, "half a draft").await;

    assert!(page.contains("class=\"dock\""), "the box morphs into the prompt: {page}");
    assert!(!page.contains("id=\"draft\""), "and the box is gone: {page}");
    assert!(page.contains("git push origin polish/rate-limit-chip"), "the call is named: {page}");
    assert!(page.contains("not on the allow list"), "with the CLI's own reason: {page}");
    let allow = page.find("Allow once").expect("the first option");
    let always = page.find("Allow always for Bash").expect("the second");
    let deny = page.find("Deny").expect("the deny option");
    let notes = page.find("Tell Claude something else").expect("the escape hatch");
    assert!(allow < always && always < deny && deny < notes, "in the order the CLI built: {page}");
}

/// An option is a control, not inert text: it renders a button that says
/// it cannot answer yet rather than a row that looks clickable and is not.
#[tokio::test]
async fn a_docks_options_are_controls_that_refuse() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    fleet.seed_test_pending_interaction(&lead(), PendingKind::Permission);
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;
    fleet.emit(SessionUpdate::PermissionRequest {
        key: lead(),
        tool_id: "tu-1".to_owned(),
        request: permission(),
    });
    settle().await;

    let (_status, page) = composer(&config, "").await;

    assert!(page.contains("<button class=\"lbl\""), "an option is a button: {page}");
    assert_eq!(
        page.matches("disabled=\"not available yet\"").count(),
        5,
        "one refusing control per option: {page}",
    );
    assert!(page.contains("answering is not available yet"), "and the dock says so: {page}");
    assert!(
        !page.contains("class=\"opt\"><span class=\"lbl\""),
        "and never an inert row that looks clickable: {page}",
    );
}

/// A question draws its own anatomy: the header, where it sits in the set,
/// and its options.
#[tokio::test]
async fn a_question_draws_its_own_anatomy() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    fleet.seed_test_pending_interaction(&lead(), PendingKind::Question);
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;
    fleet.emit(SessionUpdate::QuestionRequest {
        key: lead(),
        tool_id: "tu-2".to_owned(),
        request: question(),
    });
    settle().await;

    let (_status, page) = composer(&config, "").await;

    assert!(page.contains("Environments"), "the question's own header: {page}");
    assert!(page.contains("Q2 of 3"), "and where it sits in the set: {page}");
    assert!(page.contains("Staging") && page.contains("Production"), "and its options: {page}");
    assert!(page.contains("class=\"box2\""), "each with the multi-select box: {page}");
}

/// A prompt the core reports and this view never saw the offer of says so,
/// rather than drawing an ordinary box that would read as nothing pending.
#[tokio::test]
async fn a_prompt_with_no_options_says_so_rather_than_nothing_pending() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    fleet.seed_test_pending_interaction(&lead(), PendingKind::Question);
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, page) = composer(&config, "").await;

    assert!(page.contains("class=\"dock\""), "the core says a prompt waits: {page}");
    assert!(page.contains("a question is waiting for you"), "so the dock is drawn: {page}");
    assert!(
        page.contains("its options arrived before this view attached"),
        "and it says what it cannot show: {page}",
    );
    assert!(!page.contains("id=\"draft\""), "and never the ordinary box: {page}");
}

/// A session holding more than one prompt says how many wait behind the
/// one it is drawing, which is a read of the core's own queue.
#[tokio::test]
async fn a_queued_prompt_shows_the_depth() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    fleet.seed_test_prompt_queue(&lead(), PendingKind::Permission, 3);
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;
    fleet.emit(SessionUpdate::PermissionRequest {
        key: lead(),
        tool_id: "tu-1".to_owned(),
        request: permission(),
    });
    settle().await;

    let (_status, page) = composer(&config, "").await;

    assert!(page.contains("class=\"queue\""), "the dock counts the queue: {page}");
    assert!(page.contains("2 more pending"), "and says how many wait behind: {page}");
}

/// A session holding one prompt draws no queue line: a line reading "0
/// more" is noise about a queue that is not there.
#[tokio::test]
async fn one_prompt_draws_no_queue_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    fleet.seed_test_pending_interaction(&lead(), PendingKind::Permission);
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;
    fleet.emit(SessionUpdate::PermissionRequest {
        key: lead(),
        tool_id: "tu-1".to_owned(),
        request: permission(),
    });
    settle().await;

    let (_status, page) = composer(&config, "").await;

    assert!(page.contains("class=\"dock\""), "the prompt still draws its dock: {page}");
    assert!(!page.contains("class=\"queue\""), "with nothing queued behind it: {page}");
}

/// A seat holding nothing is the ordinary box: the dock is a window on the
/// core's pending set, so with nothing pending there is nothing to morph
/// into.
#[tokio::test]
async fn no_prompt_leaves_the_ordinary_box() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, page) = composer(&config, "").await;

    assert!(!page.contains("class=\"dock\""), "nothing pending, no dock: {page}");
    assert!(page.contains("id=\"draft\""), "the ordinary box is there to type in: {page}");
}

// ---------- the composer: the states the box is replaced by ----------

/// A seat whose spawn has not connected is connecting, which is a wait and
/// not a failure, and takes no input while it waits. Catches the connecting
/// arm folded into "not running", which is the state next door.
#[tokio::test]
async fn a_starting_seat_is_connecting() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Busytools", &["forge"])]).expect("the fleet builds");
    fleet.start("Busytools", "forge").expect("forge is declared");
    fleet.add_starting_worker("Busytools", "forge", "cli-version").expect("forge is declared");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _ct, page) = get(&config, "/session/Busytools/forge/cli-version").await;

    assert!(
        page.contains("Connecting to Claude Code"),
        "a spawn that has not connected says so: {page}",
    );
    assert!(page.contains("class=\"blocked\""), "and the box is the reason: {page}");
    assert!(!page.contains("id=\"draft\""), "with no input to type into: {page}");
}

/// A spawn that failed is a failure the reader has to act on, not a wait,
/// and it carries the reason the core recorded.
#[tokio::test]
async fn a_seat_that_could_not_start_says_why() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Busytools", &["forge"])]).expect("the fleet builds");
    fleet.start("Busytools", "forge").expect("forge is declared");
    fleet.fail_spawn("Busytools", "forge", "lead", "the subprocess exited");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _page) = composer(&config, "").await;

    let (_status, _ct, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert!(page.contains("could not start"), "a dead spawn says so: {page}");
    assert!(page.contains("the subprocess exited"), "with the core's own reason: {page}");
    assert!(page.contains("class=\"box err\""), "drawn as a failure rather than a wait: {page}");
}

/// A compaction the CLI announces replaces the box, and it clears when the
/// CLI says the status is over: a seat held on a compaction takes no more
/// input than a seat that is starting.
#[tokio::test]
async fn a_compacting_session_says_so_until_it_clears() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    fleet.emit(SessionUpdate::ChatAppended { key: lead(), msg: status("compacting") });
    settle().await;
    let (_status, page) = composer(&config, "").await;

    assert!(page.contains("Compacting context"), "the CLI's own status is drawn: {page}");
    assert!(!page.contains("id=\"draft\""), "and the box is gone while it runs: {page}");

    fleet.emit(SessionUpdate::ChatAppended { key: lead(), msg: status_null() });
    settle().await;
    let (_status, page) = composer(&config, "").await;

    assert!(!page.contains("Compacting context"), "and it clears when the CLI says so: {page}");
    assert!(page.contains("id=\"draft\""), "which gives the box back: {page}");
}

/// The sign-in hint names the method the wire is waiting on, which is what
/// lets it say which sign-in rather than only that there is one. The box
/// stays, because the mockup draws it staying.
#[tokio::test]
async fn the_hint_names_the_sign_in_it_waits_on() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    fleet.await_login(&lead());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;
    fleet.emit(SessionUpdate::AuthRequired {
        key: lead(),
        method_name: "claude.ai".to_owned(),
        method_description: "Anthropic OAuth (Pro)".to_owned(),
    });
    settle().await;

    let (_status, page) = composer(&config, "").await;

    assert!(page.contains("class=\"hint login\""), "the hint line is drawn: {page}");
    assert!(page.contains("Authentication required"), "naming the state: {page}");
    assert!(page.contains("claude.ai"), "and the method the wire named: {page}");
    assert!(page.contains("Anthropic OAuth (Pro)"), "with its own description: {page}");
    assert!(page.contains("id=\"draft\""), "while the box stays where the mockup draws it: {page}");
}

/// A status frame, as the CLI sends it.
fn status(value: &str) -> forge_primitives::Message {
    serde_json::from_value(serde_json::json!({
        "type": "system",
        "subtype": "status",
        "session_id": "s",
        "status": value,
    }))
    .expect("a status message")
}

/// The same frame with the status cleared, which is how a compaction ends.
fn status_null() -> forge_primitives::Message {
    serde_json::from_value(serde_json::json!({
        "type": "system",
        "subtype": "status",
        "session_id": "s",
        "status": null,
    }))
    .expect("a cleared status message")
}

// ---------- the composer: what the list and the dock say about themselves ----------

/// The rows scroll inside a window of their own. A list drawn in full grows
/// the popover until the box under it is past the viewport, where a page one
/// viewport tall clips it and the reader types blind. Both halves are pinned:
/// the markup's container and the sheet's bound on it.
#[tokio::test]
async fn a_long_list_scrolls_in_a_window() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    fleet.advertise(
        &SessionSlot::lead("Busytools", "forge"),
        vec![forge_primitives::AvailableCommand::new("model", "Switch model")],
        Vec::new(),
    );
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, page) = composer(&config, "/m").await;
    let (_s, _ct, sheet) = get(&config, "/web.css").await;

    assert!(page.contains("class=\"rows\""), "the rows sit in a window: {page}");
    assert!(
        declaration(&sheet, ".ac .rows", "max-height").is_some(),
        "and the sheet bounds that window, or the rows grow it without limit",
    );
}

/// The `value` a rule declares for `property`, if the rule is there at all.
fn declaration(sheet: &str, selector: &str, property: &str) -> Option<String> {
    let (_, rest) = sheet.split_once(&format!("{selector} {{"))?;
    let (body, _) = rest.split_once('}')?;
    body.split(';')
        .filter_map(|entry| entry.split_once(':'))
        .find(|(key, _)| key.trim() == property)
        .map(|(_, value)| value.trim().to_owned())
}

/// Every list draws its own mark rather than sharing one generic glyph: the
/// four triggers are four kinds of thing, and the mockup draws four symbols.
#[tokio::test]
async fn each_list_draws_its_own_mark() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let slot = SessionSlot::lead("Busytools", "forge");
    fleet.advertise(
        &slot,
        vec![forge_primitives::AvailableCommand::new("model", "Switch model")],
        vec![forge_primitives::AvailableAgent::new("cli-version", "Bump the pinned CLI")],
    );
    let project = dir.path().join("forge");
    std::fs::create_dir_all(project.join("src")).expect("mkdir");
    std::fs::write(project.join("src/home.rs"), "").expect("write");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    for (draft, mark) in [("/m", "#i-cmd"), ("&cli", "#i-bot"), (":sm", "#i-smile")] {
        let (_status, page) = composer(&config, draft).await;
        assert!(page.contains(&format!("href=\"{mark}\"")), "{draft} draws {mark}: {page}");
    }

    // The file list needs a seat whose tree has something in it.
    let (_status, page) = composer(&config, "@home").await;
    assert!(page.contains("class=\"ac\""), "a file query opens the file list: {page}");
    assert!(page.contains("href=\"#i-file\""), "which draws the file mark: {page}");
}

/// The filter is a window over the candidates and not the whole set: a list
/// long enough to fill a window is drawn in full and scrolled, and the header
/// keeps the count that says where it came from.
#[tokio::test]
async fn the_window_holds_more_candidates_than_it_shows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let commands: Vec<_> = (0..40)
        .map(|n| forge_primitives::AvailableCommand::new(format!("cmd{n}"), "A command"))
        .collect();
    fleet.advertise(&SessionSlot::lead("Busytools", "forge"), commands, Vec::new());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, page) = composer(&config, "/cmd").await;

    assert_eq!(
        page.matches("class=\"it sel\"").count() + page.matches("class=\"it\"").count(),
        40,
        "every candidate reaches the window, which is what scrolls: {page}",
    );
    assert!(page.contains(">40<"), "and the header counts them: {page}");
}

/// The list says a row cannot be chosen, which is the one place in the
/// composer that could leave it unsaid: a selected row advertises a key that
/// nothing reads, and every other unwired control refuses in text.
#[tokio::test]
async fn the_list_says_a_row_cannot_be_chosen_yet() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    fleet.advertise(
        &SessionSlot::lead("Busytools", "forge"),
        vec![forge_primitives::AvailableCommand::new("model", "Switch model")],
        Vec::new(),
    );
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, page) = composer(&config, "/m").await;

    assert!(
        page.contains("choosing a row is not available yet"),
        "the list says what it cannot do: {page}",
    );
}

/// A take's cancel refuses in text like every other control, and it is dimmed
/// like the send button: a reason living only in a disabled control's title is
/// a reason nobody reads, because a disabled control takes no pointer events.
#[tokio::test]
async fn a_takes_cancel_refuses_in_text() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;
    fleet.emit(started(1));
    settle().await;

    let (_status, page) = composer(&config, "").await;
    let (_s, _ct, sheet) = get(&config, "/web.css").await;

    assert!(
        page.contains("stopping a take is not available yet"),
        "the box says the take cannot be stopped: {page}",
    );
    assert!(
        declaration(&sheet, ".dict .esc[disabled]", "opacity").is_some(),
        "and the control is dimmed like the send button, not left looking live",
    );
}

/// The dock's rows do not keep the styling of a live choice while their
/// controls refuse: the selected row's emphasis goes with the choice it
/// advertises.
#[tokio::test]
async fn a_docks_rows_are_marked_unable_to_answer() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    fleet.seed_test_pending_interaction(&lead(), PendingKind::Permission);
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;
    fleet.emit(SessionUpdate::PermissionRequest {
        key: lead(),
        tool_id: "tu-1".to_owned(),
        request: permission(),
    });
    settle().await;

    let (_status, page) = composer(&config, "").await;

    assert!(page.contains("class=\"opt sel off\""), "the first row is marked: {page}");
    assert!(page.contains("class=\"opt off\""), "and so is every other: {page}");
    assert!(!page.contains("class=\"opt sel\""), "with none left reading as a live choice: {page}");
}

/// The meter's window is long enough to fill the slot it sits in. The
/// mockup's own track draws fifty-two cells, so a window shorter than that
/// reads as a clump at one end rather than as a history.
#[tokio::test]
async fn the_meter_window_fills_its_slot() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;
    fleet.emit(started(1));
    for reading in 0..52 {
        fleet.emit(level(-40.0 + f32::from(i16::try_from(reading).expect("a small count"))));
    }
    settle().await;

    let (_status, page) = composer(&config, "").await;

    assert_eq!(
        page.matches("style=\"height:").count(),
        52,
        "the mockup's own track length fits inside the window: {page}",
    );
}
